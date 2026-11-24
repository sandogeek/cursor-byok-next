//! Extract, merge, copy local-loop dependencies and write proto files.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::merge::{
    collect_needed_types, declaration_coverage, keep_local_loop_enums, keep_local_loop_messages,
    keep_local_loop_services, merge_enums_by_type_name, merge_messages_by_type_name,
    merge_services_by_type_name, normalize_type_references, reclassify_enum_fields,
    record_empty_messages, sorted_dedup,
};
use crate::renderer::{CopiedType, ProtoFile, generate_proto_file};
use crate::resolver::{TypeResolver, build_alias_index, build_module_imports};
use crate::scan::scan_source;
use crate::types::{
    Diagnostics, Enum, Message, Service, TypeName, is_google_pkg, is_local_loop_pkg,
    parse_type_name,
};
use crate::validate::{has_placeholder, validate_required_shapes};

/// Failures that stop an extraction run.
#[derive(Debug, thiserror::Error)]
#[must_use]
pub enum ExtractError {
    #[error("failed to read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The generated directory could not be put in place.
    ///
    /// `backup` names where the previous contents still are when a rollback was
    /// attempted; `rollback` keeps the rollback failure while `source` keeps the
    /// original publish failure, so neither cause is lost.
    #[error(
        "failed to publish {}: {source}{}{}",
        output_dir.display(),
        describe_recovery(backup.as_deref(), rollback.as_ref()),
        cleanup_note(cleanup_warning.as_deref())
    )]
    Publish {
        output_dir: PathBuf,
        backup: Option<PathBuf>,
        #[source]
        source: std::io::Error,
        rollback: Option<RollbackFailure>,
        /// A cleanup failure that happened after the publish failure above; it
        /// is never reported instead of the cause.
        cleanup_warning: Option<String>,
    },
    #[error("extracted protos failed the local-loop shape checks:\n{diagnostics}")]
    Validation { diagnostics: Box<Diagnostics> },
}

/// The second failure of a publish: restoring the previous contents.
#[derive(Debug)]
pub struct RollbackFailure {
    /// Where the restore was heading.
    pub output_dir: PathBuf,
    pub source: std::io::Error,
}

impl std::fmt::Display for RollbackFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "could not restore {}: {}",
            self.output_dir.display(),
            self.source
        )
    }
}

impl std::error::Error for RollbackFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl ExtractError {
    /// Attaches a cleanup failure that must not replace the original cause.
    fn note(&mut self, warning: String) {
        if let Self::Publish {
            cleanup_warning, ..
        } = self
        {
            *cleanup_warning = Some(warning);
        }
    }
}

/// The trailing note for a cleanup that failed after the publish did.
fn cleanup_note(warning: Option<&str>) -> String {
    warning.map_or_else(String::new, |warning| format!(" ({warning})"))
}

/// The trailing part of a publish error: where the previous contents are, and
/// whether restoring them also failed.
fn describe_recovery(
    backup: Option<&Path>,
    rollback: Option<&RollbackFailure>,
) -> std::borrow::Cow<'static, str> {
    use std::borrow::Cow;
    match (backup, rollback) {
        (None, None) => Cow::Borrowed(""),
        (Some(backup), None) => Cow::Owned(format!(
            " (the previous contents were restored from {})",
            backup.display()
        )),
        (None, Some(rollback)) => Cow::Owned(format!(" (rollback failed: {rollback})")),
        (Some(backup), Some(rollback)) => Cow::Owned(format!(
            " (rollback failed: {rollback}; the previous contents are still in {})",
            backup.display()
        )),
    }
}

#[derive(Debug)]
pub struct ExtractResult {
    pub messages: Vec<Message>,
    pub enums: Vec<Enum>,
    pub services: Vec<Service>,
    pub files: HashMap<String, String>,
    pub diagnostics: Diagnostics,
    /// Cleanup that failed after the generated directory was published. Empty
    /// unless [`write_protos`](crate::write_protos) had something to report.
    pub write_warnings: Vec<String>,
}

/// Whether diagnostics fail the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// Any diagnostic (skipped fields, unresolved types, placeholders, missing
    /// declarations, parse or shape errors) fails the run.
    Strict,
    /// Diagnostics are returned in the result but never fail the run.
    Lenient,
}

/// Scans each JS bundle source and renders the local-loop protos.
///
/// # Errors
///
/// With [`Strictness::Strict`], returns [`ExtractError::Validation`] when any
/// diagnostic reports a skipped field, unresolved type, placeholder, missing
/// declaration or shape mismatch.
pub fn extract_from_sources(
    sources: &[String],
    strictness: Strictness,
) -> Result<ExtractResult, ExtractError> {
    let mut all_messages = Vec::new();
    let mut all_enums = Vec::new();
    let mut all_services = Vec::new();
    let mut diag = Diagnostics::default();
    let mut declared = Vec::new();

    for source in sources {
        let mut scan = scan_source(source);
        let scan_diagnostics = std::mem::take(&mut scan.diagnostics);
        diag.merge_from(scan_diagnostics);
        declared.append(&mut scan.declared);
        let aliases = build_alias_index(&scan.aliases);
        let export_aliases = build_alias_index(&scan.export_aliases);
        let module_imports = build_module_imports(&scan.module_imports, &scan.module_starts);
        let resolver = TypeResolver::new(
            &scan.messages,
            &scan.enums,
            &aliases,
            &export_aliases,
            module_imports,
        );
        reclassify_enum_fields(&mut scan.messages, &scan.enums);
        normalize_type_references(&mut scan.messages, &mut scan.services, &resolver);
        reclassify_enum_fields(&mut scan.messages, &scan.enums);
        all_messages.append(&mut scan.messages);
        all_enums.append(&mut scan.enums);
        all_services.append(&mut scan.services);
    }

    let messages = keep_local_loop_messages(merge_messages_by_type_name(all_messages));
    let enums = keep_local_loop_enums(merge_enums_by_type_name(all_enums));
    let services = keep_local_loop_services(merge_services_by_type_name(all_services));
    let resolver = TypeResolver::new(
        &messages,
        &enums,
        &HashMap::default(),
        &HashMap::default(),
        HashMap::default(),
    );

    record_empty_messages(&mut diag, &messages);
    let coverage = declaration_coverage(&declared, &messages, &enums, &services);
    diag.declared_types = coverage.declared;
    diag.extracted_types = coverage.extracted;
    diag.missing_declarations = sorted_dedup(coverage.missing);

    let files = generate_local_loop_protos(&messages, &enums, &services, &resolver, &mut diag);
    if strictness == Strictness::Strict && diag.has_failure() {
        return Err(ExtractError::Validation {
            diagnostics: Box::new(diag),
        });
    }
    Ok(ExtractResult {
        messages,
        enums,
        services,
        files,
        diagnostics: diag,
        write_warnings: Vec::new(),
    })
}

/// Extracts the local-loop protos and replaces `output_dir` with them.
///
/// # Errors
///
/// Returns [`ExtractError::Read`] when a bundle cannot be read,
/// [`ExtractError::Publish`] when the generated directory cannot be swapped in,
/// and [`ExtractError::Validation`] under [`Strictness::Strict`].
pub fn extract_from_files(
    input_files: &[PathBuf],
    output_dir: &Path,
    strictness: Strictness,
) -> Result<ExtractResult, ExtractError> {
    let mut sources = Vec::with_capacity(input_files.len());
    for path in input_files {
        let text = fs::read_to_string(path).map_err(|source| ExtractError::Read {
            path: path.clone(),
            source,
        })?;
        sources.push(text);
    }
    let mut result = extract_from_sources(&sources, strictness)?;
    result.write_warnings = write_protos(output_dir, &result.files)?;
    Ok(result)
}

/// Replaces `output_dir` with `files` through a staged swap.
///
/// The generated files are written to a sibling temporary directory first.
/// That directory is then renamed into place: the previous contents are moved
/// aside and restored if the swap fails. This is **not** a single atomic
/// publish — between the two renames `output_dir` does not exist, and a crash
/// there leaves the previous contents in the backup directory. Readers that
/// must never see a gap need a versioned directory plus an atomic pointer.
///
/// Returns warnings for cleanup that failed after the swap succeeded; the
/// generated directory is in place in that case, so they are not errors.
///
/// # Errors
///
/// Returns [`ExtractError::Publish`] when a file cannot be written or the
/// directory cannot be swapped in. A failed rollback is reported in the same
/// error, together with the directory that still holds the previous contents.
#[expect(
    clippy::implicit_hasher,
    reason = "callers always build the default `HashMap`; the extractor is a CLI"
)]
pub fn write_protos(
    output_dir: &Path,
    files: &HashMap<String, String>,
) -> Result<Vec<String>, ExtractError> {
    let parent = parent_or_cwd(output_dir);
    fs::create_dir_all(parent).map_err(|source| ExtractError::Publish {
        output_dir: output_dir.to_path_buf(),
        backup: None,
        source,
        rollback: None,
        cleanup_warning: None,
    })?;
    let temp_dir = unique_temp_dir(parent, output_dir)?;
    let write_result = (|| {
        for (name, body) in files {
            let path = temp_dir.join(name);
            fs::write(&path, body).map_err(|source| ExtractError::Publish {
                output_dir: output_dir.to_path_buf(),
                backup: None,
                source,
                rollback: None,
                cleanup_warning: None,
            })?;
        }
        replace_dir(output_dir, &temp_dir)
    })();
    match write_result {
        Ok(warnings) => Ok(warnings),
        Err(mut error) => {
            // Best effort: the publish failure stays the cause, and a cleanup
            // that also failed is attached to it rather than swallowing it.
            if let Some(warning) = remove_staged_directory(&temp_dir) {
                error.note(warning);
            }
            Err(error)
        }
    }
}

/// Removes a staged directory, returning the warning to report when it fails.
fn remove_staged_directory(path: &Path) -> Option<String> {
    match fs::remove_dir_all(path) {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!(
            "could not remove the temporary directory {}: {error}",
            path.display()
        )),
    }
}

fn parent_or_cwd(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// How many sibling temp names `unique_temp_dir` will try before giving up.
const TEMP_DIR_ATTEMPTS: u32 = 32;

fn unique_temp_dir(parent: &Path, output_dir: &Path) -> Result<PathBuf, ExtractError> {
    let stem = output_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("protos");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for attempt in 0..TEMP_DIR_ATTEMPTS {
        let candidate = parent.join(format!(".{stem}.tmp.{nanos}.{attempt}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(ExtractError::Publish {
                    output_dir: output_dir.to_path_buf(),
                    backup: None,
                    source,
                    rollback: None,
                    cleanup_warning: None,
                });
            }
        }
    }
    Err(ExtractError::Publish {
        output_dir: output_dir.to_path_buf(),
        backup: None,
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("could not allocate a temp directory after {TEMP_DIR_ATTEMPTS} attempts"),
        ),
        rollback: None,
        cleanup_warning: None,
    })
}

/// Swaps the staged directory in, restoring the previous contents on failure.
///
/// Between moving the old directory aside and renaming the new one into place,
/// `output_dir` does not exist: this is a staged replace with rollback, not a
/// single atomic publish.
fn replace_dir(output_dir: &Path, temp_dir: &Path) -> Result<Vec<String>, ExtractError> {
    let parent = parent_or_cwd(output_dir);
    let mut warnings = Vec::new();
    let backup = if output_dir.exists() {
        let backup = unique_temp_dir(parent, output_dir)?;
        // `unique_temp_dir` created it only to prove the name was free.
        fs::remove_dir(&backup).map_err(|source| ExtractError::Publish {
            output_dir: output_dir.to_path_buf(),
            backup: None,
            source,
            rollback: None,
            cleanup_warning: None,
        })?;
        fs::rename(output_dir, &backup).map_err(|source| ExtractError::Publish {
            output_dir: output_dir.to_path_buf(),
            backup: None,
            source,
            rollback: None,
            cleanup_warning: None,
        })?;
        Some(backup)
    } else {
        None
    };
    match fs::rename(temp_dir, output_dir) {
        Ok(()) => {
            // The generated directory is in place; a stale backup is only
            // clutter, so report it instead of failing a finished publish.
            if let Some(backup) = backup
                && let Err(error) = fs::remove_dir_all(&backup)
            {
                warnings.push(format!(
                    "could not remove the previous output at {}: {error}",
                    backup.display()
                ));
            }
            Ok(warnings)
        }
        Err(source) => {
            // `backup` names where the previous contents still are when the
            // rollback did not put them back. Both causes are kept: the publish
            // failure is `source`, the restore failure is `rollback`.
            let restore_failed = backup.as_ref().and_then(|backup| {
                fs::rename(backup, output_dir)
                    .err()
                    .map(|rollback| RollbackFailure {
                        output_dir: output_dir.to_path_buf(),
                        source: rollback,
                    })
            });
            let leftover = restore_failed.as_ref().and(backup);
            Err(ExtractError::Publish {
                output_dir: output_dir.to_path_buf(),
                backup: leftover,
                source,
                rollback: restore_failed,
                cleanup_warning: None,
            })
        }
    }
}

/// Groups local-loop declarations per package, renders one proto file each, and
/// returns the rendered file name per fully-qualified type name. Records the
/// types that had to be copied from another package in `diag`.
fn generate_local_loop_protos(
    messages: &[Message],
    enums: &[Enum],
    services: &[Service],
    resolver: &TypeResolver,
    diag: &mut Diagnostics,
) -> HashMap<String, String> {
    let mut packages: HashMap<&str, ProtoFile> = HashMap::new();
    for message in messages {
        packages
            .entry(message.package.as_str())
            .or_default()
            .messages
            .push(message.clone());
    }
    for item in enums {
        packages
            .entry(item.package.as_str())
            .or_default()
            .enums
            .push(item.clone());
    }
    for service in services {
        packages
            .entry(service.package.as_str())
            .or_default()
            .services
            .push(service.clone());
    }

    let index = DeclarationIndex::new(messages, enums);

    let mut files = HashMap::new();
    for pkg_name in crate::types::LOCAL_LOOP_PACKAGES {
        let package = packages.remove(pkg_name).unwrap_or_default();
        let augmented = copy_external_types(pkg_name, package, resolver, &index);
        let body = generate_proto_file(pkg_name, augmented, resolver, diag);
        let file_name = format!("{}.proto", pkg_name.replace('.', "_"));
        if let Err(err) = validate_required_shapes(&file_name, pkg_name, &body) {
            diag.add_shape_error(&err);
        }
        for (idx, line) in body.lines().enumerate() {
            if has_placeholder(line) {
                diag.placeholder_hits
                    .push(format!("{pkg_name}:{}: {}", idx + 1, line.trim()));
            }
        }
        files.insert(file_name, body);
    }
    files
}

/// Every message and enum by fully-qualified name, so copies can be found
/// without re-scanning the declaration lists. `google` types are excluded: they
/// are imported, never copied in.
struct DeclarationIndex<'a> {
    messages: HashMap<&'a str, &'a Message>,
    enums: HashMap<&'a str, &'a Enum>,
}

impl<'a> DeclarationIndex<'a> {
    fn new(messages: &'a [Message], enums: &'a [Enum]) -> Self {
        Self {
            messages: messages
                .iter()
                .filter(|message| !is_google_pkg(&message.package))
                .map(|message| (message.type_name.as_str(), message))
                .collect(),
            enums: enums
                .iter()
                .filter(|item| !is_google_pkg(&item.package))
                .map(|item| (item.type_name.as_str(), item))
                .collect(),
        }
    }
}

/// How many fixpoint rounds `copy_external_types` will run before giving up on
/// a reference cycle it could not resolve.
const COPY_ROUNDS: usize = 20;

/// Builds a cloned copy of a message declared in another package.
fn copied_message(message: &Message, pkg_name: &str) -> Message {
    let mut copy = message.clone();
    pkg_name.clone_into(&mut copy.package);
    copy
}

/// Builds a cloned copy of an enum declared in another package.
fn copied_enum(item: &Enum, pkg_name: &str) -> Enum {
    let mut copy = item.clone();
    pkg_name.clone_into(&mut copy.package);
    copy
}

/// Copies the declarations of other packages that this one refers to, in
/// `COPY_ROUNDS` passes so copies of copies are found too.
fn copy_external_types(
    pkg_name: &str,
    package: ProtoFile,
    resolver: &TypeResolver,
    index: &DeclarationIndex<'_>,
) -> ProtoFile {
    let mut result = package;
    let mut local_types = seed_local_types(&mut result);
    for _ in 0..COPY_ROUNDS {
        let needed = collect_needed_types(
            pkg_name,
            &result.messages,
            &result.services,
            resolver,
            &local_types,
        );
        let copied_this_round =
            copy_one_round(pkg_name, needed, index, &mut result, &mut local_types);
        if copied_this_round == 0 {
            break;
        }
    }
    result
}

/// Registers a file's own types so external references to them are not copied.
fn seed_local_types(result: &mut ProtoFile) -> HashSet<String> {
    let mut local_types = HashSet::new();
    for message in &result.messages {
        local_types.insert(message.short_name.clone());
        result
            .copied
            .insert(message.short_name.clone(), CopiedType::Local);
    }
    for item in &result.enums {
        local_types.insert(item.short_name.clone());
        result
            .copied
            .insert(item.short_name.clone(), CopiedType::Local);
    }
    local_types
}

/// Copies every needed external declaration the index knows, returning how
/// many declarations it added.
fn copy_one_round(
    pkg_name: &str,
    needed: HashSet<String>,
    index: &DeclarationIndex<'_>,
    result: &mut ProtoFile,
    local_types: &mut HashSet<String>,
) -> usize {
    let mut copied = 0;
    for type_name in needed {
        let TypeName { package, short } = parse_type_name(&type_name);
        if package == pkg_name || is_google_pkg(&package) || local_types.contains(&short) {
            continue;
        }
        if let Some(message) = index.messages.get(type_name.as_str()) {
            result.messages.push(copied_message(message, pkg_name));
            copied += 1;
        } else if let Some(item) = index.enums.get(type_name.as_str()) {
            result.enums.push(copied_enum(item, pkg_name));
            copied += 1;
        } else if !is_local_loop_pkg(&package) {
            continue;
        }
        // A local-loop type nobody extracted still needs a name in `copied` so
        // the renderer reports it, but it does not count as a copy.
        result
            .copied
            .insert(short.clone(), CopiedType::External(type_name));
        local_types.insert(short);
    }
    copied
}
