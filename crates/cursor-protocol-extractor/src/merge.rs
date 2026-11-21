//! Merge duplicate declarations and keep local-loop packages only.

use std::collections::{HashMap, HashSet};

use crate::resolver::TypeResolver;
use crate::types::{
    Diagnostics, Enum, Field, Message, Service, TypeToken, is_google_pkg, is_local_loop_pkg,
    parse_type_name,
};

pub fn keep_local_loop_messages(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|message| is_local_loop_pkg(&message.package) || is_google_pkg(&message.package))
        .collect()
}

pub fn keep_local_loop_enums(enums: Vec<Enum>) -> Vec<Enum> {
    enums
        .into_iter()
        .filter(|item| is_local_loop_pkg(&item.package) || is_google_pkg(&item.package))
        .collect()
}

pub fn keep_local_loop_services(services: Vec<Service>) -> Vec<Service> {
    services
        .into_iter()
        .filter(|service| is_local_loop_pkg(&service.package))
        .collect()
}

pub fn merge_messages_by_type_name(messages: Vec<Message>) -> Vec<Message> {
    let mut index = HashMap::new();
    let mut merged = Vec::new();
    for message in messages {
        if let Some(&existing) = index.get(&message.type_name) {
            if message_completeness(&message) > message_completeness(&merged[existing]) {
                merged[existing] = message;
            }
            continue;
        }
        index.insert(message.type_name.clone(), merged.len());
        merged.push(message);
    }
    merged
}

fn message_completeness(message: &Message) -> i32 {
    let mut score = message.fields.len() as i32 * 10;
    for field in &message.fields {
        if field.kind == "map" {
            score += 2;
        }
        if !field.oneof.is_empty() {
            score += 1;
        }
        score += type_ref_completeness(field.t.as_ref());
        score += type_ref_completeness(field.map_value_t.as_ref());
    }
    score
}

fn type_ref_completeness(token: Option<&TypeToken>) -> i32 {
    token.map(TypeToken::completeness).unwrap_or(0)
}

pub fn merge_enums_by_type_name(enums: Vec<Enum>) -> Vec<Enum> {
    let mut index = HashMap::new();
    let mut merged = Vec::new();
    for item in enums {
        if let Some(&existing) = index.get(&item.type_name) {
            if enum_completeness(&item) > enum_completeness(&merged[existing]) {
                merged[existing] = item;
            }
            continue;
        }
        index.insert(item.type_name.clone(), merged.len());
        merged.push(item);
    }
    merged
}

fn enum_completeness(item: &Enum) -> i32 {
    let mut score = item.values.len() as i32 * 10;
    for value in &item.values {
        score += value.name.len() as i32;
    }
    score
}

pub fn merge_services_by_type_name(services: Vec<Service>) -> Vec<Service> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<Service> = Vec::new();
    for service in services {
        if let Some(&existing) = index.get(&service.type_name) {
            if service_completeness(&service) > service_completeness(&merged[existing]) {
                merged[existing] = service;
            }
            continue;
        }
        index.insert(service.type_name.clone(), merged.len());
        merged.push(service);
    }
    merged
}

fn service_completeness(service: &Service) -> i32 {
    let mut score = service.methods.len() as i32 * 10;
    for method in &service.methods {
        score += type_ref_completeness(Some(&TypeToken::Named(method.input_type.clone())));
        score += type_ref_completeness(Some(&TypeToken::Named(method.output_type.clone())));
    }
    score
}

pub fn normalize_type_references(
    messages: &mut [Message],
    services: &mut [Service],
    resolver: &TypeResolver,
) {
    for message in messages {
        for field in &mut message.fields {
            if field.kind == "message" || field.kind == "enum" {
                field.t = resolve_token(
                    field.t.take(),
                    message.pos,
                    message.module_start,
                    &message.package,
                    &field.kind,
                    resolver,
                );
            }
            if field.kind == "map"
                && (field.map_value_kind == "message" || field.map_value_kind == "enum")
            {
                field.map_value_t = resolve_token(
                    field.map_value_t.take(),
                    message.pos,
                    message.module_start,
                    &message.package,
                    &field.map_value_kind,
                    resolver,
                );
            }
        }
    }
    for service in services {
        for method in &mut service.methods {
            if let Some(name) = resolver.resolve_type_name(
                &method.input_type,
                service.pos,
                service.module_start,
                &service.package,
                "message",
            ) {
                method.input_type = name;
            }
            if let Some(name) = resolver.resolve_type_name(
                &method.output_type,
                service.pos,
                service.module_start,
                &service.package,
                "message",
            ) {
                method.output_type = name;
            }
        }
    }
}

fn resolve_token(
    token: Option<TypeToken>,
    pos: u32,
    module_start: u32,
    pkg: &str,
    kind: &str,
    resolver: &TypeResolver,
) -> Option<TypeToken> {
    match token {
        Some(TypeToken::Named(symbol)) if !symbol.trim().is_empty() => resolver
            .resolve_type_name(&symbol, pos, module_start, pkg, kind)
            .map(TypeToken::Named)
            .or(Some(TypeToken::Named(symbol))),
        other => other,
    }
}

pub fn reclassify_enum_fields(messages: &mut [Message], enums: &[Enum]) {
    let mut enum_names = HashSet::new();
    for item in enums {
        enum_names.insert(item.type_name.clone());
        enum_names.insert(item.short_name.clone());
        if let Some((_, last)) = item.short_name.rsplit_once('.') {
            enum_names.insert(last.to_string());
        }
    }
    let reclassify = |kind: &mut String, token: Option<&TypeToken>| {
        if kind != "message" {
            return;
        }
        let Some(TypeToken::Named(name)) = token else {
            return;
        };
        if enum_names.contains(name) {
            *kind = "enum".into();
            return;
        }
        let (_, short) = parse_type_name(name);
        if enum_names.contains(&short) {
            *kind = "enum".into();
        }
    };
    for message in messages {
        for field in &mut message.fields {
            if field.kind == "map" {
                reclassify(&mut field.map_value_kind, field.map_value_t.as_ref());
            } else {
                reclassify(&mut field.kind, field.t.as_ref());
            }
        }
    }
}

pub fn declaration_coverage(
    declared: &[String],
    messages: &[Message],
    enums: &[Enum],
    services: &[Service],
) -> (usize, usize, Vec<String>) {
    let mut declared_set = HashSet::new();
    for type_name in declared {
        let (pkg, _) = parse_type_name(type_name);
        if !type_name.is_empty() && !is_google_pkg(&pkg) && is_local_loop_pkg(&pkg) {
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
    (declared_set.len(), matched, missing)
}

pub fn compact_strings(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

pub fn record_empty_messages(diag: &mut Diagnostics, messages: &[Message]) {
    for message in messages {
        if message.fields.is_empty() {
            diag.empty_messages.push(message.type_name.clone());
        }
    }
}

pub fn collect_needed_types(
    current_pkg: &str,
    messages: &[Message],
    services: &[Service],
    resolver: &TypeResolver,
    local_types: &HashSet<String>,
) -> HashSet<String> {
    let mut needed = HashSet::new();
    for message in messages {
        let (preferred_pkg, _) = parse_type_name(&message.type_name);
        for field in &message.fields {
            collect_field_refs(
                field,
                current_pkg,
                &preferred_pkg,
                message.pos,
                message.module_start,
                resolver,
                &mut needed,
                local_types,
            );
        }
    }
    for service in services {
        for method in &service.methods {
            collect_method_ref(
                &method.input_type,
                current_pkg,
                service.pos,
                service.module_start,
                resolver,
                &mut needed,
                local_types,
            );
            collect_method_ref(
                &method.output_type,
                current_pkg,
                service.pos,
                service.module_start,
                resolver,
                &mut needed,
                local_types,
            );
        }
    }
    needed
}

fn collect_field_refs(
    field: &Field,
    current_pkg: &str,
    preferred_pkg: &str,
    context_pos: u32,
    context_module_start: u32,
    resolver: &TypeResolver,
    needed: &mut HashSet<String>,
    local_types: &HashSet<String>,
) {
    let mut refs = Vec::new();
    if field.kind == "message" || field.kind == "enum" {
        if let Some(TypeToken::Named(name)) = &field.t {
            refs.push((name.as_str(), field.kind.as_str()));
        }
    }
    if field.kind == "map" && (field.map_value_kind == "message" || field.map_value_kind == "enum")
    {
        if let Some(TypeToken::Named(name)) = &field.map_value_t {
            refs.push((name.as_str(), field.map_value_kind.as_str()));
        }
    }
    for (r#ref, kind) in refs {
        let Some(type_name) = resolver.resolve_type_name(
            r#ref,
            context_pos,
            context_module_start,
            preferred_pkg,
            kind,
        ) else {
            continue;
        };
        let (ref_pkg, short_name) = parse_type_name(&type_name);
        if ref_pkg.is_empty() || ref_pkg == current_pkg || is_google_pkg(&ref_pkg) {
            continue;
        }
        if local_types.contains(&short_name) {
            continue;
        }
        needed.insert(type_name);
    }
}

fn collect_method_ref(
    r#ref: &str,
    current_pkg: &str,
    context_pos: u32,
    context_module_start: u32,
    resolver: &TypeResolver,
    needed: &mut HashSet<String>,
    local_types: &HashSet<String>,
) {
    let Some(type_name) = resolver.resolve_type_name(
        r#ref,
        context_pos,
        context_module_start,
        current_pkg,
        "message",
    ) else {
        return;
    };
    let (ref_pkg, short_name) = parse_type_name(&type_name);
    if ref_pkg.is_empty() || ref_pkg == current_pkg || is_google_pkg(&ref_pkg) {
        return;
    }
    if local_types.contains(&short_name) {
        return;
    }
    needed.insert(type_name);
}
