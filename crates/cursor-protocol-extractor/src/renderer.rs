//! Render extracted declarations as stable proto3 text.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
};

use crate::resolver::{TypeContext, TypeResolver, fallback_type_token};
use crate::types::{
    Diagnostics, Enum, Field, FieldType, MapValue, Message, MethodKind, Service, SymbolKind,
    TypeName, parse_type_name, scalar_name,
};

/// The repository the generated `go_package` option points at.
const GO_PACKAGE_ROOT: &str = "github.com/leookun/cursor-byok/cursor-proto/gen";
/// `bytes` is the only proto3 type that can always stand in for a value whose
/// real type the extractor could not recover, so unresolved references keep it.
const FALLBACK_SCALAR: &str = "bytes";

/// One proto file's declarations, plus how each of its short names maps back to
/// the fully-qualified type it came from.
#[derive(Debug, Clone, Default)]
pub struct ProtoFile {
    pub messages: Vec<Message>,
    pub enums: Vec<Enum>,
    pub services: Vec<Service>,
    pub copied: HashMap<String, CopiedType>,
}

/// Where a rendered type already exists: declared in this file, or copied in
/// from another package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopiedType {
    /// Declared in this file under its own name.
    Local,
    /// Copied in under `short_name` from this fully-qualified type.
    External(String),
}

#[derive(Default)]
struct TypeNode {
    message: Option<Message>,
    enum_item: Option<Enum>,
    children: BTreeMap<String, TypeNode>,
}

/// What every declaration in one file renders against: how its short names map
/// back to fully-qualified types, the resolver that names those types, and the
/// diagnostics the pass records.
struct Renderer<'a> {
    copied: &'a HashMap<String, CopiedType>,
    resolver: &'a TypeResolver,
    diag: &'a mut Diagnostics,
}

/// Formats straight into `out`. Writing to a `String` is infallible, so the
/// `Result` is discarded rather than turned into a panic path.
fn emit(out: &mut String, args: std::fmt::Arguments<'_>) {
    let _ = out.write_fmt(args);
}

/// Renders one package's declarations, recording unresolved references.
///
/// Takes the file by value: the declarations are moved into the render tree
/// instead of being deep-cloned.
pub fn generate_proto_file(
    pkg_name: &str,
    file: ProtoFile,
    resolver: &TypeResolver,
    diag: &mut Diagnostics,
) -> String {
    let ProtoFile {
        messages,
        enums,
        services,
        copied,
    } = file;
    let imports = collect_imports(&messages, &services, resolver);
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
        format_args!("option go_package = \"{GO_PACKAGE_ROOT}/{go_path};{go_name}\";\n\n"),
    );

    let mut root = TypeNode::default();
    for message in messages {
        let path = nested_path(&message.short_name);
        insert_message(&mut root, &path, message);
    }
    for item in enums {
        let path = nested_path(&item.short_name);
        insert_enum(&mut root, &path, item);
    }
    let mut renderer = Renderer {
        copied: &copied,
        resolver,
        diag,
    };
    renderer.write_type_tree(&root, &mut out, 0);

    let mut services: Vec<&Service> = services.iter().collect();
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
            let input = renderer.resolve_method_type(&method.input_type, context);
            let output = renderer.resolve_method_type(&method.output_type, context);
            let (input_stream, output_stream) = match method.kind {
                MethodKind::ServerStreaming => ("", "stream "),
                MethodKind::ClientStreaming => ("stream ", ""),
                MethodKind::BiDiStreaming => ("stream ", "stream "),
                MethodKind::Unary => ("", ""),
            };
            emit(
                &mut out,
                format_args!(
                    "  rpc {}({input_stream}{input}) returns ({output_stream}{output}) {{}}\n",
                    method.name
                ),
            );
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

/// The `// Source:`/`// Copied from:` line above an enum.
fn write_enum_comment(
    out: &mut String,
    indent: &str,
    item: &Enum,
    copied: &HashMap<String, CopiedType>,
) {
    match copied.get(&item.short_name) {
        Some(CopiedType::External(original)) => emit(
            out,
            format_args!(
                "{indent}// Copied from: {original} (var: {})\n",
                item.var_name
            ),
        ),
        Some(CopiedType::Local) | None => emit(
            out,
            format_args!(
                "{indent}// Source: {} (var: {})\n",
                item.type_name, item.var_name
            ),
        ),
    }
}

/// The `// Source:`/`// Copied from:` line above a message.
fn write_message_comment(
    out: &mut String,
    indent: &str,
    message: &Message,
    copied: &HashMap<String, CopiedType>,
) {
    let mut var_info = message.var_name.clone();
    if !message.internal_name.is_empty() && message.internal_name != message.var_name {
        var_info = format!("{}, class: {}", message.var_name, message.internal_name);
    }
    match copied.get(&message.short_name) {
        Some(CopiedType::External(original)) => emit(
            out,
            format_args!("{indent}// Copied from: {original} (var: {var_info})\n"),
        ),
        Some(CopiedType::Local) | None => emit(
            out,
            format_args!(
                "{indent}// Source: {} (var: {var_info})\n",
                message.type_name
            ),
        ),
    }
}

impl Renderer<'_> {
    fn write_type_tree(&mut self, node: &TypeNode, out: &mut String, indent: usize) {
        let indent_str = "  ".repeat(indent);
        for (name, child) in &node.children {
            if let Some(item) = &child.enum_item {
                write_enum_comment(out, &indent_str, item, self.copied);
                emit(out, format_args!("{indent_str}enum {name} {{\n"));
                for value in &item.values {
                    emit(
                        out,
                        format_args!("{indent_str}  {} = {};\n", value.name, value.number),
                    );
                }
                emit(out, format_args!("{indent_str}}}\n\n"));
            } else if child.message.is_some() || !child.children.is_empty() {
                if let Some(message) = &child.message {
                    write_message_comment(out, &indent_str, message, self.copied);
                }
                emit(out, format_args!("{indent_str}message {name} {{\n"));
                self.write_type_tree(child, out, indent + 1);
                if let Some(message) = &child.message {
                    self.write_message_fields(message, out, indent + 1);
                }
                emit(out, format_args!("{indent_str}}}\n\n"));
            }
        }
    }

    fn write_message_fields(&mut self, message: &Message, out: &mut String, indent: usize) {
        let indent_str = "  ".repeat(indent);
        let context = TypeContext::new(&message.package, message.pos, message.module_start);
        let mut oneofs: BTreeMap<&str, Vec<&Field>> = BTreeMap::new();
        let mut regular = Vec::new();
        for field in &message.fields {
            match &field.oneof {
                Some(oneof) => oneofs.entry(oneof.as_str()).or_default().push(field),
                None => regular.push(field),
            }
        }
        for field in regular {
            let field_type = self.resolve_field_type(field, &message.short_name, context);
            let prefix = if field.repeated {
                "repeated "
            } else if field.optional {
                "optional "
            } else {
                ""
            };
            emit(
                out,
                format_args!(
                    "{indent_str}{prefix}{field_type} {} = {number};\n",
                    field.name,
                    number = field.number
                ),
            );
        }
        for (oneof_name, fields) in oneofs {
            emit(out, format_args!("{indent_str}oneof {oneof_name} {{\n"));
            for field in fields {
                let field_type = self.resolve_field_type(field, &message.short_name, context);
                emit(
                    out,
                    format_args!(
                        "{indent_str}  {field_type} {} = {number};\n",
                        field.name,
                        number = field.number
                    ),
                );
            }
            emit(out, format_args!("{indent_str}}}\n"));
        }
    }

    fn resolve_method_type(&mut self, type_ref: &str, context: TypeContext<'_>) -> String {
        let Some(type_name) =
            self.resolver
                .resolve_type_name(type_ref, context, SymbolKind::Message)
        else {
            self.diag.add_unresolved_type(&format!("method:{type_ref}"));
            return fallback_type_token(type_ref);
        };
        self.qualify(&type_name, context)
    }

    /// The name a rendered reference uses: the bare short name when the type is
    /// local to this file (same package, well-known, or copied in), else the
    /// package-qualified name.
    fn qualify(&self, type_name: &str, context: TypeContext<'_>) -> String {
        let TypeName { package, short } = parse_type_name(type_name);
        if package == context.package || package.is_empty() || self.copied.contains_key(&short) {
            short
        } else {
            format!("{package}.{short}")
        }
    }

    fn resolve_field_type(
        &mut self,
        field: &Field,
        parent_path: &str,
        context: TypeContext<'_>,
    ) -> String {
        match field.field_type() {
            FieldType::Scalar { width } => width
                .and_then(scalar_name)
                .unwrap_or(FALLBACK_SCALAR)
                .to_owned(),
            FieldType::Named { kind, name } => match name {
                Some(name) => self.resolve_named(&name, parent_path, context, kind.symbol_kind()),
                None => FALLBACK_SCALAR.to_owned(),
            },
            FieldType::Map { key, value } => {
                let key = key.and_then(scalar_name).unwrap_or("string");
                let value = match value {
                    MapValue::Scalar { width } => width
                        .and_then(scalar_name)
                        .unwrap_or(FALLBACK_SCALAR)
                        .to_owned(),
                    MapValue::Named { kind, name } => name.map_or_else(
                        || FALLBACK_SCALAR.to_owned(),
                        |name| self.resolve_named(&name, parent_path, context, kind.symbol_kind()),
                    ),
                };
                format!("map<{key}, {value}>")
            }
        }
    }

    /// Resolves one named field type, shortening it when the reference is nested
    /// in its parent message, or qualifying it when it lives in another package.
    fn resolve_named(
        &mut self,
        type_ref: &str,
        parent_path: &str,
        context: TypeContext<'_>,
        expected: SymbolKind,
    ) -> String {
        let Some(type_name) = self.resolver.resolve_type_name(type_ref, context, expected) else {
            self.diag
                .add_unresolved_type(&format!("{expected:?}:{type_ref}"));
            return fallback_type_token(type_ref);
        };
        let TypeName { short, .. } = parse_type_name(&type_name);
        if !parent_path.is_empty()
            && let Some(nested) = short
                .strip_prefix(parent_path)
                .and_then(|rest| rest.strip_prefix('.'))
        {
            return nested.to_owned();
        }
        self.qualify(&type_name, context)
    }
}

/// The `google/protobuf/*` and `google/rpc/*` imports the file needs.
#[must_use]
pub fn collect_imports(
    messages: &[Message],
    services: &[Service],
    resolver: &TypeResolver,
) -> HashSet<String> {
    let mut imports = HashSet::new();
    let mut add = |type_ref: &str, context: TypeContext<'_>, expected: SymbolKind| {
        let Some(type_name) = resolver.resolve_type_name(type_ref, context, expected) else {
            return;
        };
        let TypeName { package, short } = parse_type_name(&type_name);
        if package == "google.protobuf" {
            imports.insert(google_protobuf_import(&short).to_owned());
        } else if package == "google.rpc" {
            imports.insert(
                if short == "Code" {
                    "google/rpc/code.proto"
                } else {
                    "google/rpc/status.proto"
                }
                .to_owned(),
            );
        }
    };
    for message in messages {
        let context = TypeContext::new(&message.package, message.pos, message.module_start);
        for field in &message.fields {
            // Only a named field, or a map's value, can reference a
            // declaration; `field_type` already bound the name to its kind.
            let referenced = match field.field_type() {
                FieldType::Named { kind, name } => name.map(|name| (name, kind)),
                FieldType::Map {
                    value: MapValue::Named { kind, name },
                    ..
                } => name.map(|name| (name, kind)),
                FieldType::Scalar { .. } | FieldType::Map { .. } => None,
            };
            if let Some((name, kind)) = referenced {
                add(&name, context, kind.symbol_kind());
            }
        }
    }
    for service in services {
        let context = TypeContext::new(&service.package, service.pos, service.module_start);
        for method in &service.methods {
            add(&method.input_type, context, SymbolKind::Message);
            add(&method.output_type, context, SymbolKind::Message);
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
