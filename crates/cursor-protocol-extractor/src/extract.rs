//! Extract, merge, copy local-loop dependencies and write proto files.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::merge::{
    collect_needed_types, compact_strings, declaration_coverage, keep_local_loop_enums,
    keep_local_loop_messages, keep_local_loop_services, merge_enums_by_type_name,
    merge_messages_by_type_name, merge_services_by_type_name, normalize_type_references,
    reclassify_enum_fields, record_empty_messages,
};
use crate::renderer::generate_proto_file;
use crate::resolver::{build_alias_index, build_module_imports, TypeResolver};
use crate::scan::{scan_source, BundleScan};
use crate::types::{
    is_google_pkg, is_local_loop_pkg, parse_type_name, Diagnostics, Enum, Message, Service,
};
use crate::validate::{has_placeholder, validate_required_agent_shapes};

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Validation(String),
}

#[derive(Debug)]
pub struct ExtractResult {
    pub messages: Vec<Message>,
    pub enums: Vec<Enum>,
    pub services: Vec<Service>,
    pub files: HashMap<String, String>,
    pub diagnostics: Diagnostics,
}

pub fn extract_from_sources(sources: &[(&str, String)], strict: bool) -> Result<ExtractResult, ExtractError> {
    let mut all_messages = Vec::new();
    let mut all_enums = Vec::new();
    let mut all_services = Vec::new();
    let mut diag = Diagnostics::default();
    let mut declared = Vec::new();

    for (_name, source) in sources {
        let mut scan = scan_source(source);
        diag.total_field_objects += scan.diagnostics.total_field_objects;
        diag.parsed_field_objects += scan.diagnostics.parsed_field_objects;
        diag.skipped_field_objects += scan.diagnostics.skipped_field_objects;
        diag.skipped_field_samples
            .append(&mut scan.diagnostics.skipped_field_samples);
        declared.append(&mut scan.declared);
        remap_module_imports(&mut scan);
        let aliases = build_alias_index(&scan.aliases);
        let export_aliases = build_alias_index(&scan.export_aliases);
        let mut resolver = TypeResolver::new(&scan.messages, &scan.enums, &aliases, &export_aliases);
        resolver.module_imports = build_module_imports(&scan.module_imports, &scan.module_starts);
        reclassify_enum_fields(&mut scan.messages, &scan.enums);
        normalize_type_references(&mut scan.messages, &mut scan.services, &resolver);
        all_messages.append(&mut scan.messages);
        all_enums.append(&mut scan.enums);
        all_services.append(&mut scan.services);
    }

    let messages = keep_local_loop_messages(merge_messages_by_type_name(all_messages));
    let enums = keep_local_loop_enums(merge_enums_by_type_name(all_enums));
    let services = keep_local_loop_services(merge_services_by_type_name(all_services));
    let resolver = TypeResolver::new(&messages, &enums, &Default::default(), &Default::default());

    record_empty_messages(&mut diag, &messages);
    let (declared_count, extracted_count, missing) =
        declaration_coverage(&declared, &messages, &enums, &services);
    diag.declared_types = declared_count;
    diag.extracted_types = extracted_count;
    diag.missing_declarations = compact_strings(missing);

    let files = generate_local_loop_protos(&messages, &enums, &services, &resolver, &mut diag)?;
    if strict && diag.has_failure(false) {
        return Err(ExtractError::Validation(diag.to_string()));
    }
    Ok(ExtractResult {
        messages,
        enums,
        services,
        files,
        diagnostics: diag,
    })
}

pub fn extract_from_files(
    input_files: &[impl AsRef<Path>],
    output_dir: impl AsRef<Path>,
    strict: bool,
) -> Result<ExtractResult, ExtractError> {
    let mut sources = Vec::new();
    for path in input_files {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| ExtractError::Read {
            path: path.display().to_string(),
            source,
        })?;
        sources.push((path.to_string_lossy().into_owned(), text));
    }
    let named: Vec<(&str, String)> = sources
        .iter()
        .map(|(name, text)| (name.as_str(), text.clone()))
        .collect();
    let result = extract_from_sources(&named, strict)?;
    write_protos(output_dir, &result.files)?;
    Ok(result)
}

pub fn write_protos(output_dir: impl AsRef<Path>, files: &HashMap<String, String>) -> Result<(), ExtractError> {
    let output_dir = output_dir.as_ref();
    fs::create_dir_all(output_dir).map_err(|source| ExtractError::Write {
        path: output_dir.display().to_string(),
        source,
    })?;
    for (name, body) in files {
        let path = output_dir.join(name);
        fs::write(&path, body).map_err(|source| ExtractError::Write {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

fn generate_local_loop_protos(
    messages: &[Message],
    enums: &[Enum],
    services: &[Service],
    resolver: &TypeResolver,
    diag: &mut Diagnostics,
) -> Result<HashMap<String, String>, ExtractError> {
    let mut packages: HashMap<String, Package> = HashMap::new();
    for message in messages {
        packages.entry(message.package.clone()).or_default().messages.push(message.clone());
    }
    for item in enums {
        packages.entry(item.package.clone()).or_default().enums.push(item.clone());
    }
    for service in services {
        packages.entry(service.package.clone()).or_default().services.push(service.clone());
    }

    let mut all_messages = HashMap::new();
    let mut all_enums = HashMap::new();
    for (pkg, package) in &packages {
        if is_google_pkg(pkg) {
            continue;
        }
        for message in &package.messages {
            all_messages.insert(message.type_name.clone(), message.clone());
        }
        for item in &package.enums {
            all_enums.insert(item.type_name.clone(), item.clone());
        }
    }

    let mut files = HashMap::new();
    for pkg_name in crate::types::LOCAL_LOOP_PACKAGES {
        let package = packages.remove(pkg_name).unwrap_or_default();
        let (augmented, copied) =
            copy_external_types(pkg_name, package, resolver, &all_messages, &all_enums);
        let body = generate_proto_file(
            pkg_name,
            &augmented.messages,
            &augmented.enums,
            &augmented.services,
            resolver,
            &copied,
            diag,
        );
        if let Err(err) = validate_required_agent_shapes(&format!("{}.proto", pkg_name.replace('.', "_")), &body)
        {
            return Err(ExtractError::Validation(err));
        }
        for (idx, line) in body.lines().enumerate() {
            if has_placeholder(line) {
                diag.placeholder_hits
                    .push(format!("{}:{}: {}", pkg_name, idx + 1, line.trim()));
            }
        }
        files.insert(format!("{}.proto", pkg_name.replace('.', "_")), body);
    }
    Ok(files)
}

#[derive(Default, Clone)]
struct Package {
    messages: Vec<Message>,
    enums: Vec<Enum>,
    services: Vec<Service>,
}

fn copy_external_types(
    pkg_name: &str,
    package: Package,
    resolver: &TypeResolver,
    all_messages: &HashMap<String, Message>,
    all_enums: &HashMap<String, Enum>,
) -> (Package, HashMap<String, String>) {
    let mut copied = HashMap::new();
    let mut local_types = HashSet::new();
    for message in &package.messages {
        local_types.insert(message.short_name.clone());
        copied.insert(message.short_name.clone(), format!("local:{}", message.type_name));
    }
    for item in &package.enums {
        local_types.insert(item.short_name.clone());
        copied.insert(item.short_name.clone(), format!("local:{}", item.type_name));
    }
    let mut result = package;
    for _ in 0..20 {
        let needed = collect_needed_types(
            pkg_name,
            &result.messages,
            &result.services,
            resolver,
            &local_types,
        );
        let mut copied_this_round = 0;
        for type_name in needed {
            let (ref_pkg, short_name) = parse_type_name(&type_name);
            if ref_pkg == pkg_name || is_google_pkg(&ref_pkg) || local_types.contains(&short_name) {
                continue;
            }
            if let Some(message) = all_messages.get(&type_name) {
                let mut copy = message.clone();
                copy.package = pkg_name.to_string();
                result.messages.push(copy);
                copied.insert(short_name.clone(), type_name);
                local_types.insert(short_name);
                copied_this_round += 1;
            } else if let Some(item) = all_enums.get(&type_name) {
                let mut copy = item.clone();
                copy.package = pkg_name.to_string();
                result.enums.push(copy);
                copied.insert(short_name.clone(), type_name);
                local_types.insert(short_name);
                copied_this_round += 1;
            } else if is_local_loop_pkg(&ref_pkg) {
                copied.insert(short_name.clone(), type_name);
                local_types.insert(short_name);
            }
        }
        if copied_this_round == 0 {
            break;
        }
    }
    (result, copied)
}

fn remap_module_imports(scan: &mut BundleScan) {
    let id_to_start: HashMap<u32, u32> = scan
        .module_starts
        .iter()
        .copied()
        .map(|(start, id)| (id, start))
        .collect();
    for (_, alias, target) in &mut scan.module_imports {
        if alias.is_empty() {
            continue;
        }
        if let Some(start) = id_to_start.get(target) {
            *target = *start;
        }
    }
}
