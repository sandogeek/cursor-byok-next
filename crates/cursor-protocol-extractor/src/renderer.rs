//! Render extracted declarations as stable proto3 text.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
};

use crate::resolver::{TypeContext, TypeResolver, fallback_type_token};
use crate::types::{
    Diagnostics, Enum, Field, Message, Service, TypeName, TypeToken, parse_type_name, scalar_name,
};

#[derive(Default)]
struct TypeNode {
    message: Option<Message>,
    enum_item: Option<Enum>,
    children: BTreeMap<String, TypeNode>,
}

/// Formats straight into `out`. Writing to a `String` is infallible, so the
/// `Result` is discarded rather than turned into a panic path.
fn emit(out: &mut String, args: std::fmt::Arguments<'_>) {
    let _ = out.write_fmt(args);
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
    let imports = collect_imports(messages, services, resolver);
    let mut out = String::new();
    out.push_str("syntax = \"proto3\";\n\n");
    emit(&mut out, format_args!("package {pkg_name};\n\n"));
    if !imports.is_empty() {
        let mut sorted: Vec<_> = imports.into_iter().collect();
        sorted.sort();
        for item in sorted {
            emit(&mut out, format_args!("import \"{item}\";\n"));
        }
        out.push('\n');
    }
    let go_path = pkg_name.replace('.', "/");
    let go_name = pkg_name.replace('.', "");
    emit(
        &mut out,
        format_args!(
            "option go_package = \"github.com/leookun/cursor-byok/cursor-proto/gen/{go_path};{go_name}\";\n\n"
        ),
    );

    let mut root = TypeNode::default();
    for message in messages {
        insert_message(
            &mut root,
            &nested_path(&message.short_name),
            message.clone(),
        );
    }
    for item in enums {
        insert_enum(&mut root, &nested_path(&item.short_name), item.clone());
    }
    write_type_tree(&root, &mut out, resolver, copied, diag, 0);

    let mut services = services.to_vec();
    services.sort_by(|a, b| a.short_name.cmp(&b.short_name));
    for service in services {
        emit(
            &mut out,
            format_args!(
                "// Source: {} (var: {})\n",
                service.type_name, service.var_name
            ),
        );
        emit(
            &mut out,
            format_args!("service {} {{\n", service.short_name),
        );
        let context = TypeContext::new(pkg_name, service.pos, service.module_start);
        for method in &service.methods {
            let input = resolve_method_type(&method.input_type, resolver, context, copied, diag);
            let output = resolve_method_type(&method.output_type, resolver, context, copied, diag);
            let line = match method.kind.as_str() {
                "ServerStreaming" => format!(
                    "  rpc {}({}) returns (stream {}) {{}}\n",
                    method.name, input, output
                ),
                "ClientStreaming" => format!(
                    "  rpc {}(stream {}) returns ({}) {{}}\n",
                    method.name, input, output
                ),
                "BiDiStreaming" => {
                    format!(
                        "  rpc {}(stream {}) returns (stream {}) {{}}\n",
                        method.name, input, output
                    )
                }
                _ => format!(
                    "  rpc {}({}) returns ({}) {{}}\n",
                    method.name, input, output
                ),
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
                emit(
                    out,
                    format_args!(
                        "{indent_str}// Source: {} (var: {})\n",
                        item.type_name, item.var_name
                    ),
                );
            } else {
                emit(
                    out,
                    format_args!(
                        "{indent_str}// Copied from: {original} (var: {})\n",
                        item.var_name
                    ),
                );
            }
            emit(out, format_args!("{indent_str}enum {name} {{\n"));
            for value in &item.values {
                emit(
                    out,
                    format_args!("{indent_str}  {} = {};\n", value.name, value.no),
                );
            }
            emit(out, format_args!("{indent_str}}}\n\n"));
        } else if child.message.is_some() || !child.children.is_empty() {
            if let Some(message) = &child.message {
                let mut var_info = message.var_name.clone();
                if !message.internal_name.is_empty() && message.internal_name != message.var_name {
                    var_info = format!("{}, class: {}", message.var_name, message.internal_name);
                }
                let original = copied.get(&message.short_name).cloned().unwrap_or_default();
                if original.is_empty() || original.starts_with("local:") {
                    emit(
                        out,
                        format_args!(
                            "{indent_str}// Source: {} (var: {var_info})\n",
                            message.type_name
                        ),
                    );
                } else {
                    emit(
                        out,
                        format_args!("{indent_str}// Copied from: {original} (var: {var_info})\n"),
                    );
                }
            }
            emit(out, format_args!("{indent_str}message {name} {{\n"));
            write_type_tree(child, out, resolver, copied, diag, indent + 1);
            if let Some(message) = &child.message {
                write_message_fields(message, out, resolver, copied, diag, indent + 1);
            }
            emit(out, format_args!("{indent_str}}}\n\n"));
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
    let context = TypeContext::new(&message.package, message.pos, message.module_start);
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
        let field_type =
            resolve_field_type(field, resolver, &message.short_name, context, copied, diag);
        let prefix = if field.repeated {
            "repeated "
        } else if field.opt {
            "optional "
        } else {
            ""
        };
        emit(
            out,
            format_args!(
                "{indent_str}{prefix}{field_type} {} = {};\n",
                field.name, field.no
            ),
        );
    }
    for (oneof_name, fields) in oneofs {
        emit(out, format_args!("{indent_str}oneof {oneof_name} {{\n"));
        for field in fields {
            let field_type =
                resolve_field_type(field, resolver, &message.short_name, context, copied, diag);
            emit(
                out,
                format_args!(
                    "{indent_str}  {field_type} {} = {};\n",
                    field.name, field.no
                ),
            );
        }
        emit(out, format_args!("{indent_str}}}\n"));
    }
}

fn resolve_method_type(
    type_ref: &str,
    resolver: &TypeResolver,
    context: TypeContext<'_>,
    copied: &HashMap<String, String>,
    diag: &mut Diagnostics,
) -> String {
    let Some(type_name) = resolver.resolve_type_name(type_ref, context, "message") else {
        diag.add_unresolved_type(&format!("method:{type_ref}"));
        return fallback_type_token(type_ref);
    };
    let TypeName { package, short } = parse_type_name(&type_name);
    if package == context.package || package.is_empty() || copied.contains_key(&short) {
        short
    } else {
        format!("{package}.{short}")
    }
}

fn resolve_field_type(
    field: &Field,
    resolver: &TypeResolver,
    parent_path: &str,
    context: TypeContext<'_>,
    copied: &HashMap<String, String>,
    diag: &mut Diagnostics,
) -> String {
    let mut resolve_named = |type_ref: &str, expected_kind: &str| {
        let Some(type_name) = resolver.resolve_type_name(type_ref, context, expected_kind) else {
            diag.add_unresolved_type(&format!("{expected_kind}:{type_ref}"));
            return fallback_type_token(type_ref);
        };
        let TypeName { package, short } = parse_type_name(&type_name);
        if !parent_path.is_empty() && short.starts_with(&format!("{parent_path}.")) {
            return short[parent_path.len() + 1..].to_string();
        }
        if package == context.package || package.is_empty() || copied.contains_key(&short) {
            short
        } else {
            format!("{package}.{short}")
        }
    };

    if field.kind == "scalar"
        && let Some(TypeToken::Scalar(id)) = field.t
    {
        return scalar_name(id).unwrap_or("bytes").to_string();
    }
    if (field.kind == "message" || field.kind == "enum")
        && let Some(TypeToken::Named(name)) = &field.t
    {
        return resolve_named(name, &field.kind);
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

/// The `google/protobuf/*` and `google/rpc/*` imports the file needs.
pub fn collect_imports(
    messages: &[Message],
    services: &[Service],
    resolver: &TypeResolver,
) -> HashSet<String> {
    let mut imports = HashSet::new();
    let mut add = |type_ref: &str, context: TypeContext<'_>, expected_kind: &str| {
        let Some(type_name) = resolver.resolve_type_name(type_ref, context, expected_kind) else {
            return;
        };
        let TypeName { package, short } = parse_type_name(&type_name);
        if package == "google.protobuf" {
            imports.insert(google_protobuf_import(&short).to_string());
        } else if package == "google.rpc" {
            imports.insert(
                if short == "Code" {
                    "google/rpc/code.proto"
                } else {
                    "google/rpc/status.proto"
                }
                .to_string(),
            );
        }
    };
    for message in messages {
        let context = TypeContext::new(&message.package, message.pos, message.module_start);
        for field in &message.fields {
            if (field.kind == "message" || field.kind == "enum")
                && let Some(TypeToken::Named(name)) = &field.t
            {
                add(name, context, &field.kind);
            }
            if field.kind == "map"
                && (field.map_value_kind == "message" || field.map_value_kind == "enum")
                && let Some(TypeToken::Named(name)) = &field.map_value_t
            {
                add(name, context, &field.map_value_kind);
            }
        }
    }
    for service in services {
        let context = TypeContext::new(&service.package, service.pos, service.module_start);
        for method in &service.methods {
            add(&method.input_type, context, "message");
            add(&method.output_type, context, "message");
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
