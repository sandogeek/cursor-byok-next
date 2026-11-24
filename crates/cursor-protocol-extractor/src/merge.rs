//! Merge duplicate declarations and keep local-loop packages only.

use std::collections::{HashMap, HashSet};

use crate::resolver::{TypeContext, TypeResolver};
use crate::types::{
    Diagnostics, Enum, Field, FieldKind, Message, Service, SymbolKind, TypeName, TypeToken,
    ValueKind, is_google_pkg, is_local_loop_pkg, parse_type_name,
};

/// A definition's field count outweighs any per-field marker when choosing
/// between duplicate declarations.
const FIELD_COUNT_WEIGHT: i32 = 10;
/// Maps and oneofs are rare in Cursor bundles, so they break ties in favour of
/// the declaration that modelled the wire shape more closely.
const MAP_FIELD_WEIGHT: i32 = 2;
const ONEOF_FIELD_WEIGHT: i32 = 1;

/// Keeps only messages the local loop compiles or serves.
#[must_use]
pub fn keep_local_loop_messages(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|message| is_local_loop_pkg(&message.package) || is_google_pkg(&message.package))
        .collect()
}

/// Keeps only enums the local loop compiles or serves.
#[must_use]
pub fn keep_local_loop_enums(enums: Vec<Enum>) -> Vec<Enum> {
    enums
        .into_iter()
        .filter(|item| is_local_loop_pkg(&item.package) || is_google_pkg(&item.package))
        .collect()
}

/// Keeps only services the local loop serves.
#[must_use]
pub fn keep_local_loop_services(services: Vec<Service>) -> Vec<Service> {
    services
        .into_iter()
        .filter(|service| is_local_loop_pkg(&service.package))
        .collect()
}

/// A declaration that can be deduplicated by its fully-qualified name.
trait NamedDeclaration {
    /// The fully-qualified type name two declarations share when they collide.
    fn type_name(&self) -> &str;
    /// How completely this declaration models the wire shape; higher wins.
    fn completeness(&self) -> i32;
}

/// Keeps the most complete declaration per type name, preserving first-seen order.
fn merge_by_type_name<T: NamedDeclaration>(items: Vec<T>) -> Vec<T> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<T> = Vec::new();
    for item in items {
        if let Some(&existing) = index.get(item.type_name()) {
            if item.completeness() > merged[existing].completeness() {
                merged[existing] = item;
            }
        } else {
            index.insert(item.type_name().to_owned(), merged.len());
            merged.push(item);
        }
    }
    merged
}

/// Merges duplicate messages, keeping the most complete declaration.
#[must_use]
pub fn merge_messages_by_type_name(messages: Vec<Message>) -> Vec<Message> {
    merge_by_type_name(messages)
}

impl NamedDeclaration for Message {
    fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Field count dominates; maps, oneofs and resolved type references add to it.
    fn completeness(&self) -> i32 {
        let mut score = length_score(self.fields.len()) * FIELD_COUNT_WEIGHT;
        for field in &self.fields {
            if field.kind == FieldKind::Map {
                score += MAP_FIELD_WEIGHT;
            }
            if field.oneof.is_some() {
                score += ONEOF_FIELD_WEIGHT;
            }
            score += type_ref_completeness(field.t.as_ref());
            score += type_ref_completeness(field.map_value_t.as_ref());
        }
        score
    }
}

fn type_ref_completeness(token: Option<&TypeToken>) -> i32 {
    token.map_or(0, TypeToken::completeness)
}

/// Score for "how much this declaration has", kept in `i32` because the
/// comparisons only care about magnitude, not the absolute value.
fn length_score(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

/// Merges duplicate enums, keeping the most complete declaration.
#[must_use]
pub fn merge_enums_by_type_name(enums: Vec<Enum>) -> Vec<Enum> {
    merge_by_type_name(enums)
}

impl NamedDeclaration for Enum {
    fn type_name(&self) -> &str {
        &self.type_name
    }

    fn completeness(&self) -> i32 {
        let mut score = length_score(self.values.len()) * FIELD_COUNT_WEIGHT;
        for value in &self.values {
            score += length_score(value.name.len());
        }
        score
    }
}

/// Merges duplicate services, keeping the most complete declaration.
#[must_use]
pub fn merge_services_by_type_name(services: Vec<Service>) -> Vec<Service> {
    merge_by_type_name(services)
}

impl NamedDeclaration for Service {
    fn type_name(&self) -> &str {
        &self.type_name
    }

    fn completeness(&self) -> i32 {
        let mut score = length_score(self.methods.len()) * FIELD_COUNT_WEIGHT;
        for method in &self.methods {
            score += name_completeness(&method.input_type);
            score += name_completeness(&method.output_type);
        }
        score
    }
}

fn name_completeness(type_ref: &str) -> i32 {
    TypeToken::Named(type_ref.to_owned()).completeness()
}

/// Rewrites field and method type references to fully-qualified names.
pub fn normalize_type_references(
    messages: &mut [Message],
    services: &mut [Service],
    resolver: &TypeResolver,
) {
    for message in messages {
        let context = TypeContext::new(&message.package, message.pos, message.module_start);
        for field in &mut message.fields {
            if field.kind.is_named() {
                field.t = resolve_token(field.t.take(), context, field.kind, resolver);
            }
            if field.kind == FieldKind::Map && field.map_value_kind.is_named() {
                field.map_value_t = resolve_token(
                    field.map_value_t.take(),
                    context,
                    FieldKind::of_value(field.map_value_kind),
                    resolver,
                );
            }
        }
    }
    for service in services {
        let context = TypeContext::new(&service.package, service.pos, service.module_start);
        for method in &mut service.methods {
            if let Some(name) =
                resolver.resolve_type_name(&method.input_type, context, SymbolKind::Message)
            {
                method.input_type = name;
            }
            if let Some(name) =
                resolver.resolve_type_name(&method.output_type, context, SymbolKind::Message)
            {
                method.output_type = name;
            }
        }
    }
}

fn resolve_token(
    token: Option<TypeToken>,
    context: TypeContext<'_>,
    kind: FieldKind,
    resolver: &TypeResolver,
) -> Option<TypeToken> {
    match token {
        Some(TypeToken::Named(symbol)) if !symbol.trim().is_empty() => resolver
            .resolve_type_name(&symbol, context, SymbolKind::of(kind))
            .map(TypeToken::Named)
            .or(Some(TypeToken::Named(symbol))),
        other => other,
    }
}

/// Re-labels message fields that actually name an enum.
pub fn reclassify_enum_fields(messages: &mut [Message], enums: &[Enum]) {
    let mut enum_names = HashSet::new();
    for item in enums {
        enum_names.insert(item.type_name.clone());
        enum_names.insert(item.short_name.clone());
        if let Some((_, last)) = item.short_name.rsplit_once('.') {
            enum_names.insert(last.to_owned());
        }
    }
    let is_enum =
        |name: &str| enum_names.contains(name) || enum_names.contains(&parse_type_name(name).short);
    for message in messages {
        for field in &mut message.fields {
            if field.kind == FieldKind::Map {
                if field.map_value_kind == ValueKind::Message
                    && let Some(TypeToken::Named(name)) = &field.map_value_t
                    && is_enum(name)
                {
                    field.map_value_kind = ValueKind::Enum;
                }
            } else if field.kind == FieldKind::Message
                && let Some(TypeToken::Named(name)) = &field.t
                && is_enum(name)
            {
                field.kind = FieldKind::Enum;
            }
        }
    }
}

/// How much of the bundle's declarations the extraction captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationCoverage {
    /// Non-Google declarations the bundle announced.
    pub declared: usize,
    /// Announced declarations that made it into the output.
    pub extracted: usize,
    /// Declared but never parsed, sorted.
    pub missing: Vec<String>,
}

/// Counts how much of the bundle's declarations the extraction captured.
#[must_use]
pub fn declaration_coverage(
    declared: &[String],
    messages: &[Message],
    enums: &[Enum],
    services: &[Service],
) -> DeclarationCoverage {
    let mut declared_set = HashSet::new();
    for type_name in declared {
        let package = parse_type_name(type_name).package;
        if !type_name.is_empty() && !is_google_pkg(&package) && is_local_loop_pkg(&package) {
            declared_set.insert(type_name.clone());
        }
    }
    let mut extracted = HashSet::new();
    for message in messages {
        extracted.insert(message.type_name.clone());
    }
    for item in enums {
        extracted.insert(item.type_name.clone());
    }
    for service in services {
        extracted.insert(service.type_name.clone());
    }
    let mut missing = Vec::new();
    let mut matched = 0;
    for type_name in &declared_set {
        if extracted.contains(type_name) {
            matched += 1;
        } else {
            missing.push(type_name.clone());
        }
    }
    missing.sort();
    DeclarationCoverage {
        declared: declared_set.len(),
        extracted: matched,
        missing,
    }
}

/// Sorts and removes duplicate names so diagnostics are stable.
#[must_use]
pub fn sorted_dedup(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

/// Records messages that parsed with no fields.
pub fn record_empty_messages(diag: &mut Diagnostics, messages: &[Message]) {
    for message in messages {
        if message.fields.is_empty() {
            diag.empty_messages.push(message.type_name.clone());
        }
    }
}

/// The resolution inputs every type-reference collector needs.
struct RefScope<'a> {
    current_package: &'a str,
    resolver: &'a TypeResolver,
}

impl RefScope<'_> {
    /// Records `type_ref` unless it resolves inside the package being rendered.
    fn collect(
        &self,
        type_ref: &str,
        context: TypeContext<'_>,
        needed: &mut HashSet<String>,
        local_types: &HashSet<String>,
    ) {
        let Some(type_name) =
            self.resolver
                .resolve_type_name(type_ref, context, SymbolKind::Message)
        else {
            return;
        };
        let TypeName { package, short } = parse_type_name(&type_name);
        if package.is_empty() || package == self.current_package || is_google_pkg(&package) {
            return;
        }
        if local_types.contains(&short) {
            return;
        }
        needed.insert(type_name);
    }
}

/// The definitions a proto file still needs, split by role.
///
/// Each declaration contributes references seen from its own position, so
/// resolution prefers the module the declaration lives in.
pub fn collect_needed_types(
    current_package: &str,
    messages: &[Message],
    services: &[Service],
    resolver: &TypeResolver,
    local_types: &HashSet<String>,
) -> HashSet<String> {
    let scope = RefScope {
        current_package,
        resolver,
    };
    let mut needed = HashSet::new();
    for message in messages {
        let context = TypeContext::new(&message.package, message.pos, message.module_start);
        let references = message.fields.iter().flat_map(field_type_references);
        for type_ref in references.into_iter().flatten() {
            scope.collect(type_ref, context, &mut needed, local_types);
        }
    }
    for service in services {
        let context = TypeContext::new(&service.package, service.pos, service.module_start);
        let references = service
            .methods
            .iter()
            .flat_map(|method| [method.input_type.as_str(), method.output_type.as_str()]);
        for type_ref in references {
            scope.collect(type_ref, context, &mut needed, local_types);
        }
    }
    needed
}

/// The named types one field refers to: its own type and, for a map, its value.
fn field_type_references(field: &Field) -> [Option<&str>; 2] {
    let own = match &field.t {
        Some(TypeToken::Named(name)) if field.kind.is_named() => Some(name.as_str()),
        _ => None,
    };
    let map_value = match &field.map_value_t {
        Some(TypeToken::Named(name)) if field.map_value_kind.is_named() => Some(name.as_str()),
        _ => None,
    };
    [own, map_value]
}
