//! Extract Cursor local-loop protobuf schemas from installed JS bundles.

mod ast;
mod compact;
mod error;
mod extract;
mod merge;
mod renderer;
mod resolver;
mod scan;
mod types;
mod validate;

pub use compact::{
    CompactIndex, CompactPendingMessage, CompactSchema, parse_compact_field,
    parse_compact_field_with_diagnostics, parse_compact_schema_array,
};
pub use error::Error;
pub use extract::{
    ExtractError, ExtractResult, RollbackFailure, Strictness, extract_from_files,
    extract_from_sources, write_protos,
};
pub use merge::{
    DeclarationCoverage, declaration_coverage, merge_enums_by_type_name,
    merge_messages_by_type_name, merge_services_by_type_name, normalize_type_references,
    reclassify_enum_fields, sorted_dedup,
};
pub use renderer::{CopiedType, ProtoFile, collect_imports, generate_proto_file};
pub use resolver::{
    AliasIndex, ModuleImports, ModuleStart, TypeContext, TypeResolver, build_alias_index,
    build_module_imports,
};
pub use scan::{BundleScan, parse_field_object, parse_field_object_with_diagnostics, scan_source};
pub use types::{
    Diagnostics, Enum, EnumValue, Field, FieldKind, FieldNumber, FieldType, InvalidFieldNumber,
    InvalidFieldNumberReason, MapValue, Message, Method, MethodKind, NamedFieldKind, Service,
    SymbolKind, TypeName, TypeToken, ValueKind, google_well_known, is_field_name, is_google_pkg,
    is_ident, is_local_loop_pkg, is_scalar_id, looks_like_full_type_name, looks_like_package_name,
    looks_like_versioned_type, parse_type_name, scalar_name,
};
pub use validate::{has_placeholder, proto_block, validate_required_shapes};

/// Indexes a scan's declarations, aliases and module imports into a resolver.
#[must_use]
pub fn resolver_from_scan(scan: &BundleScan) -> TypeResolver {
    let aliases = build_alias_index(&scan.aliases);
    let export_aliases = build_alias_index(&scan.export_aliases);
    TypeResolver::new(
        &scan.messages,
        &scan.enums,
        &aliases,
        &export_aliases,
        build_module_imports(&scan.module_imports, &scan.module_starts),
    )
}

/// Parses a single JS field-object literal into a [`Field`].
///
/// # Errors
///
/// Returns [`Error::Parse`] when the source is not valid JavaScript and
/// [`Error::Field`] when the literal is not a field object.
pub fn parse_field_object_source(source: &str) -> Result<Field, Error> {
    let allocator = oxc::allocator::Allocator::new();
    let expression =
        oxc::parser::Parser::new(&allocator, source, oxc::span::SourceType::unambiguous())
            .parse_expression()
            .map_err(|diagnostics| {
                let mut diagnostics = diagnostics.into_vec().into_iter();
                let Some(first) = diagnostics.next() else {
                    return Error::Field(format!("cannot parse `{}`", source.trim()));
                };
                Error::Parse {
                    diagnostic: first,
                    other_errors: diagnostics.count(),
                }
            })?;
    parse_field_object(&expression)
}

#[cfg(test)]
mod tests;
