//! Extract Cursor local-loop protobuf schemas from installed JS bundles.

#![allow(
    clippy::collapsible_if,
    clippy::get_first,
    clippy::too_many_arguments
)]

mod ast_util;
mod compact;
mod extract;
mod merge;
mod renderer;
mod resolver;
mod scan;
mod types;
mod validate;

pub use compact::parse_compact_field;
pub use extract::{extract_from_files, extract_from_sources, write_protos, ExtractError, ExtractResult};
pub use merge::{
    declaration_coverage, merge_enums_by_type_name, merge_messages_by_type_name,
    merge_services_by_type_name, normalize_type_references, reclassify_enum_fields,
};
pub use resolver::{build_alias_index, build_module_imports, TypeResolver};
pub use scan::{parse_field_object, scan_source, BundleScan};

pub fn resolver_from_scan(scan: &BundleScan) -> TypeResolver {
    let aliases = build_alias_index(&scan.aliases);
    let export_aliases = build_alias_index(&scan.export_aliases);
    let mut resolver = TypeResolver::new(&scan.messages, &scan.enums, &aliases, &export_aliases);
    resolver.module_imports = build_module_imports(&scan.module_imports, &scan.module_starts);
    resolver
}
pub use types::{
    parse_type_name, Diagnostics, Enum, EnumValue, Field, Message, Method, Service, TypeToken,
};
pub use validate::{has_placeholder, validate_required_agent_shapes};

pub fn parse_field_object_source(source: &str) -> Result<Field, String> {
    let allocator = oxc::allocator::Allocator::new();
    let expr = oxc::parser::Parser::new(&allocator, source, oxc::span::SourceType::unambiguous())
        .parse_expression()
        .map_err(|err| format!("{err:?}"))?;
    parse_field_object(&expr)
}

#[cfg(test)]
mod tests;
