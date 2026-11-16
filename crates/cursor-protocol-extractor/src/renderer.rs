//! Render extracted declarations as stable proto3 text.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::resolver::{fallback_type_token, TypeResolver};
use crate::types::{
    parse_type_name, scalar_name, Diagnostics, Enum, Field, Message, Service, TypeToken,
};

#[derive(Default)]
struct TypeNode {
    message: Option<Message>,
    enum_item: Option<Enum>,
    children: BTreeMap<String, TypeNode>,
}

pub fn generate_proto_file(
    pkg_name: &str,
    messages: &[Message],
    enums: &[Enum],
    services: &[Service],
    resolver: &TypeResolver,
    copied: &HashMap<String, String>,
    diag: &mut Diagnostics,
) -> String {
    let imports = collect_imports(pkg_name, messages, services, resolver);
    let mut out = String::new();
    out.push_str("syntax = \"proto3\";\n\n");
    out.push_str(&format!("package {pkg_name};\n\n"));
    if !imports.is_empty() {
        let mut sorted: Vec<_> = imports.into_iter().collect();
        sorted.sort();
        for item in sorted {
            out.push_str(&format!("import \"{item}\";\n"));
        }
        out.push('\n');
    }
    let go_path = pkg_name.replace('.', "/");
    let go_name = pkg_name.replace('.', "");
    out.push_str(&format!(
        "option go_package = \"github.com/leookun/cursor-byok/cursor-proto/gen/{go_path};{go_name}\";\n\n"
    ));

    let mut root = TypeNode::default();
    for message in messages {
        insert_message(&mut root, &nested_path(&message.short_name), message.clone());
    }
    for item in enums {
        insert_enum(&mut root, &nested_path(&item.short_name), item.clone());
    }
    write_type_tree(&root, &mut out, resolver, copied, diag, 0);

    let mut services = services.to_vec();
    services.sort_by(|a, b| a.short_name.cmp(&b.short_name));
    for service in services {
        out.push_str(&format!(
            "// Source: {} (var: {})\n",
            service.type_name, service.var_name
        ));
        out.push_str(&format!("service {} {{\n", service.short_name));
        for method in &service.methods {
            let input = resolve_method_type(
                &method.input_type,
                resolver,
                pkg_name,
                service.pos,
                service.module_start,
                copied,
                diag,
            );
            let output = resolve_method_type(
                &method.output_type,
                resolver,
                pkg_name,
                service.pos,
                service.module_start,
                copied,
                diag,
            );
            let line = match method.kind.as_str() {
                "ServerStreaming" => format!("  rpc {}({}) returns (stream {}) {{}}\n", method.name, input, output),
                "ClientStreaming" => format!("  rpc {}(stream {}) returns ({}) {{}}\n", method.name, input, output),
                "BiDiStreaming" => {
                    format!("  rpc {}(stream {}) returns (stream {}) {{}}\n", method.name, input, output)
                }
                _ => format!("  rpc {}({}) returns ({}) {{}}\n", method.name, input, output),
            };
            out.push_str(&line);
        }
        out.push_str("}\n\n");
    }
    out
}

fn nested_path(short_name: &str) -> Vec<String> {
    short_name.split('.').map(str::to_string).collect()
}

fn insert_message(node: &mut TypeNode, path: &[String], message: Message) {
    let Some((name, rest)) = path.split_first() else {
        return;
    };
    let child = node.children.entry(name.clone()).or_default();
    if rest.is_empty() {
        child.message = Some(message);
    } else {
        insert_message(child, rest, message);
    }
}

fn insert_enum(node: &mut TypeNode, path: &[String], item: Enum) {
    let Some((name, rest)) = path.split_first() else {
        return;
    };
    let child = node.children.entry(name.clone()).or_default();
    if rest.is_empty() {
        child.enum_item = Some(item);
    } else {
        insert_enum(child, rest, item);
    }
}

fn write_type_tree(
    node: &TypeNode,
    out: &mut String,
    resolver: &TypeResolver,
    copied: &HashMap<String, String>,
    diag: &mut Diagnostics,
    indent: usize,
) {
    let indent_str = "  ".repeat(indent);
    for (name, child) in &node.children {
        if let Some(item) = &child.enum_item {
            let original = copied.get(&item.short_name).cloned().unwrap_or_default();
            if original.is_empty() || original.starts_with("local:") {
                out.push_str(&format!(
                    "{indent_str}// Source: {} (var: {})\n",
                    item.type_name, item.var_name
                ));
            } else {
                out.push_str(&format!(
                    "{indent_str}// Copied from: {original} (var: {})\n",
                    item.var_name
                ));
            }
            out.push_str(&format!("{indent_str}enum {name} {{\n"));
            for value in &item.values {
                out.push_str(&format!(
                    "{indent_str}  {} = {};\n",
                    value.name, value.no
                ));
            }
            out.push_str(&format!("{indent_str}}}\n\n"));
        } else if child.message.is_some() || !child.children.is_empty() {
            if let Some(message) = &child.message {
                let mut var_info = message.var_name.clone();
                if !message.internal_name.is_empty() && message.internal_name != message.var_name {
                    var_info = format!("{}, class: {}", message.var_name, message.internal_name);
                }
                let original = copied.get(&message.short_name).cloned().unwrap_or_default();
                if original.is_empty() || original.starts_with("local:") {
                    out.push_str(&format!(
                        "{indent_str}// Source: {} (var: {var_info})\n",
                        message.type_name
                    ));
                } else {
                    out.push_str(&format!(
                        "{indent_str}// Copied from: {original} (var: {var_info})\n"
                    ));
                }
            }
            out.push_str(&format!("{indent_str}message {name} {{\n"));
            write_type_tree(child, out, resolver, copied, diag, indent + 1);
            if let Some(message) = &child.message {
                write_message_fields(message, out, resolver, copied, diag, indent + 1);
            }
            out.push_str(&format!("{indent_str}}}\n\n"));
        }
    }
}

fn write_message_fields(
    message: &Message,
    out: &mut String,
    resolver: &TypeResolver,
    copied: &HashMap<String, String>,
    diag: &mut Diagnostics,
    indent: usize,
) {
    let indent_str = "  ".repeat(indent);
    let (preferred_pkg, _) = parse_type_name(&message.type_name);
    let mut oneofs: BTreeMap<String, Vec<&Field>> = BTreeMap::new();
    let mut regular = Vec::new();
    for field in &message.fields {
        if field.oneof.is_empty() {
            regular.push(field);
        } else {
            oneofs.entry(field.oneof.clone()).or_default().push(field);
        }
    }
    for field in regular {
        let field_type = resolve_field_type(
            field,
            resolver,
            &message.short_name,
            &message.package,
            &preferred_pkg,
            message.pos,
            message.module_start,
            copied,
            diag,
        );
        let prefix = if field.repeated {
            "repeated "
        } else if field.opt {
            "optional "
        } else {
            ""
        };
        out.push_str(&format!(
            "{indent_str}{prefix}{field_type} {} = {};\n",
            field.name, field.no
        ));
    }
    for (oneof_name, fields) in oneofs {
        out.push_str(&format!("{indent_str}oneof {oneof_name} {{\n"));
        for field in fields {
            let field_type = resolve_field_type(
                field,
                resolver,
                &message.short_name,
                &message.package,
                &preferred_pkg,
                message.pos,
                message.module_start,
                copied,
                diag,
            );
            out.push_str(&format!(
                "{indent_str}  {field_type} {} = {};\n",
                field.name, field.no
            ));
        }
        out.push_str(&format!("{indent_str}}}\n"));
    }
}

fn resolve_method_type(
    type_ref: &str,
    resolver: &TypeResolver,
    current_pkg: &str,
    context_pos: u32,
    context_module_start: u32,
    copied: &HashMap<String, String>,
    diag: &mut Diagnostics,
) -> String {
    let Some(type_name) =
        resolver.resolve_type_name(type_ref, context_pos, context_module_start, current_pkg, "message")
    else {
        diag.add_unresolved_type(&format!("method:{type_ref}"));
        return fallback_type_token(type_ref);
    };
    let (ref_pkg, short_name) = parse_type_name(&type_name);
    if ref_pkg == current_pkg || ref_pkg.is_empty() || copied.contains_key(&short_name) {
        short_name
    } else {
        format!("{ref_pkg}.{short_name}")
    }
}

fn resolve_field_type(
    field: &Field,
    resolver: &TypeResolver,
    parent_path: &str,
    current_pkg: &str,
    preferred_pkg: &str,
    context_pos: u32,
    context_module_start: u32,
    copied: &HashMap<String, String>,
    diag: &mut Diagnostics,
) -> String {
    let mut resolve_named = |type_ref: &str, expected_kind: &str| {
        let Some(type_name) = resolver.resolve_type_name(
            type_ref,
            context_pos,
            context_module_start,
            preferred_pkg,
            expected_kind,
        ) else {
            diag.add_unresolved_type(&format!("{expected_kind}:{type_ref}"));
            return fallback_type_token(type_ref);
        };
        let (ref_pkg, short_name) = parse_type_name(&type_name);
        if !parent_path.is_empty() && short_name.starts_with(&format!("{parent_path}.")) {
            return short_name[parent_path.len() + 1..].to_string();
        }
        if ref_pkg == current_pkg || ref_pkg.is_empty() || copied.contains_key(&short_name) {
            short_name
        } else {
            format!("{ref_pkg}.{short_name}")
        }
    };

    if field.kind == "scalar" {
        if let Some(TypeToken::Scalar(id)) = field.t {
            return scalar_name(id).unwrap_or("bytes").to_string();
        }
    }
    if field.kind == "message" || field.kind == "enum" {
        if let Some(TypeToken::Named(name)) = &field.t {
            return resolve_named(name, &field.kind);
        }
    }
    if field.kind == "map" {
        let key = scalar_name(field.map_key).unwrap_or("string");
        let value = if field.map_value_kind == "scalar" {
            match field.map_value_t {
                Some(TypeToken::Scalar(id)) => scalar_name(id).unwrap_or("bytes").to_string(),
                _ => "bytes".into(),
            }
        } else if field.map_value_kind == "message" || field.map_value_kind == "enum" {
            match &field.map_value_t {
                Some(TypeToken::Named(name)) => resolve_named(name, &field.map_value_kind),
                _ => "bytes".into(),
            }
        } else {
            "bytes".into()
        };
        return format!("map<{key}, {value}>");
    }
    "bytes".into()
}

pub fn collect_imports(
    current_pkg: &str,
    messages: &[Message],
    services: &[Service],
    resolver: &TypeResolver,
) -> HashSet<String> {
    let mut imports = HashSet::new();
    let mut add = |r#ref: &str, context_pos: u32, context_module_start: u32, expected_kind: &str| {
        let Some(type_name) =
            resolver.resolve_type_name(r#ref, context_pos, context_module_start, current_pkg, expected_kind)
        else {
            return;
        };
        let (ref_pkg, short_name) = parse_type_name(&type_name);
        if ref_pkg == "google.protobuf" {
            imports.insert(google_protobuf_import(&short_name).to_string());
        } else if ref_pkg == "google.rpc" {
            imports.insert(
                if short_name == "Code" {
                    "google/rpc/code.proto"
                } else {
                    "google/rpc/status.proto"
                }
                .to_string(),
            );
        }
    };
    for message in messages {
        for field in &message.fields {
            if field.kind == "message" || field.kind == "enum" {
                if let Some(TypeToken::Named(name)) = &field.t {
                    add(name, message.pos, message.module_start, &field.kind);
                }
            }
            if field.kind == "map" && (field.map_value_kind == "message" || field.map_value_kind == "enum")
            {
                if let Some(TypeToken::Named(name)) = &field.map_value_t {
                    add(name, message.pos, message.module_start, &field.map_value_kind);
                }
            }
        }
    }
    for service in services {
        for method in &service.methods {
            add(&method.input_type, service.pos, service.module_start, "message");
            add(&method.output_type, service.pos, service.module_start, "message");
        }
    }
    imports
}

fn google_protobuf_import(short_name: &str) -> &'static str {
    match short_name {
        "Struct" | "Value" | "ListValue" | "NullValue" => "google/protobuf/struct.proto",
        "Timestamp" => "google/protobuf/timestamp.proto",
        "Duration" => "google/protobuf/duration.proto",
        "Any" => "google/protobuf/any.proto",
        "Empty" => "google/protobuf/empty.proto",
        "FieldMask" => "google/protobuf/field_mask.proto",
        "BoolValue" | "BytesValue" | "DoubleValue" | "FloatValue" | "Int32Value" | "Int64Value"
        | "StringValue" | "UInt32Value" | "UInt64Value" => "google/protobuf/wrappers.proto",
        _ => "google/protobuf/descriptor.proto",
    }
}
