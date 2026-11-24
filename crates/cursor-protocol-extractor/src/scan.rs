//! Walk a Cursor JS bundle with oxc and collect protobuf declarations.

use std::collections::HashMap;

use oxc::allocator::Allocator;
use oxc::ast::ast::{
    ArrayExpression, CallExpression, Class, ClassElement, Expression, FormalParameters,
    MethodDefinition, ObjectExpression, ObjectPropertyKind, VariableDeclarator,
};
use oxc::ast_visit::{Visit, walk};
use oxc::parser::Parser;
use oxc::span::{GetSpan, SourceType, Span};
use regex::Regex;
use std::sync::OnceLock;

use crate::ast_util::{
    argument_expr, array_from_expr, assignment_ident, assignment_member, binding_ident,
    callee_tail, f64_to_i32, f64_to_u32, ident_name, is_js_true, last_segment, member_or_ident,
    number_lit, object_prop, object_prop_shorthand, property_name, property_numeric_key,
    returned_expression, string_lit,
};
use crate::compact::{
    CompactClass, CompactPendingMessage, CompactPrefix, CompactSchema, compact_class_name_at,
    extract_compact_enum_values, finish_compact_messages, package_for_compact_pos,
    parse_compact_schema_array, resolve_compact_package_expr,
};
use crate::error::Error;
use crate::resolver::ModuleStart;
use crate::types::{
    Diagnostics, Enum, EnumValue, Field, FieldKind, Message, Method, MethodKind, Service, TypeName,
    TypeToken, ValueKind, google_well_known, is_field_name, is_google_pkg, is_ident,
    is_local_loop_pkg, parse_type_name,
};

#[derive(Debug, Default)]
pub struct BundleScan {
    pub messages: Vec<Message>,
    pub enums: Vec<Enum>,
    pub services: Vec<Service>,
    pub aliases: Vec<(Option<u32>, String, String)>,
    pub export_aliases: Vec<(Option<u32>, String, String)>,
    pub module_imports: Vec<(Option<u32>, String, u32)>,
    pub module_starts: Vec<ModuleStart>,
    pub declared: Vec<String>,
    pub diagnostics: Diagnostics,
}

pub fn scan_source(source: &str) -> BundleScan {
    let scan = scan_source_once(source);
    if looks_like_webpack_modules(source) && scan.module_starts.is_empty() {
        let wrapped = format!("({{\n{source}\n}})");
        let wrapped_scan = scan_source_once(&wrapped);
        if wrapped_scan_is_better(&scan, &wrapped_scan) {
            return wrapped_scan;
        }
    }
    scan
}

fn wrapped_scan_is_better(original: &BundleScan, wrapped: &BundleScan) -> bool {
    if wrapped.module_starts.is_empty() && !scan_has_protocol(wrapped) {
        return false;
    }
    let original_decls = original.messages.len() + original.enums.len() + original.services.len();
    let wrapped_decls = wrapped.messages.len() + wrapped.enums.len() + wrapped.services.len();
    if wrapped_decls < original_decls {
        return false;
    }
    if wrapped.diagnostics.parse_errors.len() > original.diagnostics.parse_errors.len()
        && wrapped_decls == original_decls
    {
        return false;
    }
    true
}

fn looks_like_webpack_modules(source: &str) -> bool {
    webpack_module_key().is_match(source)
}

fn webpack_module_key() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|,)\s*\d+\s*:\s*(?:function\s*\(|\()").expect("webpack module key")
    })
}

fn scan_has_protocol(scan: &BundleScan) -> bool {
    !scan.messages.is_empty()
        || !scan.enums.is_empty()
        || !scan.services.is_empty()
        || !scan.export_aliases.is_empty()
        || scan.module_starts.len() > 1
}

fn scan_source_once(source: &str) -> BundleScan {
    let allocator = Allocator::new();
    let parsed = Parser::new(&allocator, source, SourceType::cjs()).parse();
    let mut visitor = Scanner {
        source,
        scan: BundleScan::default(),
        module_starts: Vec::new(),
        compact_classes: Vec::new(),
        compact_prefixes: Vec::new(),
        pending_compact: Vec::new(),
        pending_enum_pkgs: Vec::new(),
        current_class: None,
        message_index: HashMap::new(),
        enum_index: HashMap::new(),
        service_index: HashMap::new(),
        enum_by_pos: HashMap::new(),
    };
    for error in parsed.diagnostics.errors() {
        visitor.scan.diagnostics.add_parse_error(&error.to_string());
    }
    visitor.visit_program(&parsed.program);
    visitor.finish()
}

struct Scanner<'a> {
    source: &'a str,
    scan: BundleScan,
    module_starts: Vec<ModuleStart>,
    compact_classes: Vec<CompactClass>,
    compact_prefixes: Vec<CompactPrefix>,
    pending_compact: Vec<CompactPendingMessage>,
    pending_enum_pkgs: Vec<(u32, String)>,
    current_class: Option<CurrentClass>,
    message_index: HashMap<String, usize>,
    enum_index: HashMap<String, usize>,
    service_index: HashMap<String, usize>,
    enum_by_pos: HashMap<u32, usize>,
}

/// The `newFieldList` class currently being walked, kept by value on clone so
/// `handle_new_field_list` can detach from `self` before mutating the scan.
#[derive(Clone)]
struct CurrentClass {
    var_name: String,
    internal_name: String,
    start: u32,
}

impl Scanner<'_> {
    fn finish(mut self) -> BundleScan {
        self.qualify_late_prefixes();
        let compact_enums = std::mem::take(&mut self.scan.enums);
        let pending = std::mem::take(&mut self.pending_compact);
        let compact_messages = finish_compact_messages(pending, &compact_enums);
        self.scan.enums = compact_enums;
        for message in compact_messages {
            self.push_message(message);
        }
        self.scan.module_starts = self.module_starts;
        self.scan
    }

    fn module_start_for(&self, pos: u32) -> Option<u32> {
        match self
            .module_starts
            .partition_point(|module| module.start <= pos)
        {
            0 => None,
            index => Some(self.module_starts[index - 1].start),
        }
    }

    fn push_declared(&mut self, type_name: &str) {
        let package = parse_type_name(type_name).package;
        if !type_name.is_empty() && !is_google_pkg(&package) {
            self.scan.declared.push(type_name.to_string());
        }
    }

    fn keep_type(type_name: &str) -> bool {
        let package = parse_type_name(type_name).package;
        is_local_loop_pkg(&package) || is_google_pkg(&package)
    }

    fn keep_package(pkg: &str) -> bool {
        is_local_loop_pkg(pkg)
    }

    fn push_message(&mut self, message: Message) {
        if !Self::keep_type(&message.type_name) {
            return;
        }
        if let Some(&index) = self.message_index.get(&message.type_name) {
            let existing = &mut self.scan.messages[index];
            if existing.var_name.is_empty() && !message.var_name.is_empty() {
                existing.var_name = message.var_name;
                existing.internal_name = message.internal_name;
            }
            if existing.fields.is_empty() && !message.fields.is_empty() {
                existing.fields = message.fields;
            }
            return;
        }
        self.message_index
            .insert(message.type_name.clone(), self.scan.messages.len());
        self.scan.messages.push(message);
    }

    fn push_enum(&mut self, item: Enum) {
        if !item.package.is_empty() && !Self::keep_type(&item.type_name) {
            return;
        }
        if let Some(&index) = self.enum_index.get(&item.type_name) {
            let existing = &mut self.scan.enums[index];
            if existing.var_name.is_empty() && !item.var_name.is_empty() {
                existing.var_name = item.var_name;
            }
            if existing.values.len() < item.values.len() {
                existing.values = item.values;
            }
            return;
        }
        let index = self.scan.enums.len();
        self.enum_index.insert(item.type_name.clone(), index);
        self.enum_by_pos.insert(item.pos, index);
        self.scan.enums.push(item);
    }

    fn push_service(&mut self, service: Service) {
        if !Self::keep_type(&service.type_name) {
            return;
        }
        if self.service_index.contains_key(&service.type_name) {
            return;
        }
        self.service_index
            .insert(service.type_name.clone(), self.scan.services.len());
        self.scan.services.push(service);
    }

    fn add_alias(&mut self, pos: u32, alias: &str, target: &str) {
        if alias.is_empty() || target.is_empty() || alias == target {
            return;
        }
        self.scan.aliases.push((
            self.module_start_for(pos),
            alias.to_string(),
            target.to_string(),
        ));
    }

    fn maybe_package_prefix(&mut self, name: &str, value: &Expression<'_>, pos: u32) {
        let Some(raw) = string_lit(value) else {
            return;
        };
        let package = raw.trim_end_matches('.').to_string();
        if crate::types::looks_like_package_name(&package) {
            self.compact_prefixes.push(CompactPrefix {
                name: name.to_string(),
                package,
                pos,
            });
        }
    }

    fn extract_fields_from_expr(&mut self, expr: &Expression<'_>) -> Vec<Field> {
        let Some(array) = array_from_expr(expr) else {
            return Vec::new();
        };
        self.extract_fields_from_array(array)
    }

    fn extract_fields_from_array(&mut self, array: &ArrayExpression<'_>) -> Vec<Field> {
        let mut fields = Vec::new();
        for element in &array.elements {
            let Some(expr) = element.as_expression() else {
                continue;
            };
            match parse_field_object(expr) {
                Ok(field) => {
                    self.scan.diagnostics.add_parsed_field();
                    fields.push(field);
                }
                Err(err) => {
                    let snippet = self.source_span(expr.span()).to_string();
                    self.scan.diagnostics.add_skipped_field(&snippet, &err);
                }
            }
        }
        fields
    }

    fn source_span(&self, span: Span) -> &str {
        let start = usize::try_from(span.start).unwrap_or(usize::MAX);
        let end = usize::try_from(span.end)
            .unwrap_or(usize::MAX)
            .min(self.source.len());
        if start >= end || start >= self.source.len() {
            ""
        } else {
            &self.source[start..end]
        }
    }

    fn parse_enum_values(expr: &Expression<'_>) -> Vec<EnumValue> {
        let Some(array) = array_from_expr(expr) else {
            return Vec::new();
        };
        let mut values = Vec::new();
        for element in &array.elements {
            let Some(item) = element.as_expression() else {
                continue;
            };
            let Some(no) = object_prop(item, "no")
                .and_then(number_lit)
                .and_then(f64_to_i32)
            else {
                continue;
            };
            let Some(name) = object_prop(item, "name").and_then(string_lit) else {
                continue;
            };
            values.push(EnumValue {
                no,
                name: name.to_string(),
            });
        }
        values
    }

    fn parse_service_from_object(
        &mut self,
        object: &ObjectExpression<'_>,
        var_name: &str,
        pos: u32,
    ) {
        let type_name = object.properties.iter().find_map(|property| {
            let ObjectPropertyKind::ObjectProperty(prop) = property else {
                return None;
            };
            if property_name(&prop.key) == Some("typeName") {
                string_lit(&prop.value).map(str::to_string)
            } else {
                None
            }
        });
        let Some(type_name) = type_name else {
            return;
        };
        let methods_expr = object.properties.iter().find_map(|property| {
            let ObjectPropertyKind::ObjectProperty(prop) = property else {
                return None;
            };
            (property_name(&prop.key) == Some("methods")).then_some(&prop.value)
        });
        let Some(Expression::ObjectExpression(methods_object)) =
            methods_expr.map(Expression::get_inner_expression)
        else {
            return;
        };
        self.push_declared(&type_name);
        let methods = parse_methods(methods_object);
        let TypeName { package, short } = parse_type_name(&type_name);
        self.push_service(Service {
            type_name,
            var_name: var_name.to_string(),
            methods,
            package,
            short_name: short,
            pos,
            module_start: self.module_start_for(pos),
        });
    }

    fn record_message(
        &mut self,
        type_name: &str,
        var_name: &str,
        internal_name: &str,
        fields: Vec<Field>,
        pos: u32,
    ) {
        self.push_declared(type_name);
        let TypeName { package, short } = parse_type_name(type_name);
        self.push_message(Message {
            type_name: type_name.to_string(),
            var_name: var_name.to_string(),
            internal_name: internal_name.to_string(),
            fields,
            package,
            short_name: short,
            pos,
            module_start: self.module_start_for(pos),
        });
    }

    fn record_enum(&mut self, type_name: &str, var_name: &str, values: Vec<EnumValue>, pos: u32) {
        self.push_declared(type_name);
        let TypeName { package, short } = parse_type_name(type_name);
        self.push_enum(Enum {
            type_name: type_name.to_string(),
            var_name: var_name.to_string(),
            values,
            package,
            short_name: short,
            pos,
            module_start: self.module_start_for(pos),
        });
    }

    /// Dispatches one call expression to the handler for its factory name.
    ///
    /// The names are Cursor's minified helpers: `makeMessageType` / `makeEnum`
    /// build a declaration, `setEnumType` rewrites an enum's values,
    /// `enumType` and `QT` are the compact-schema enum forms, `newFieldList`
    /// adds fields to the enclosing class, `d` is webpack's define/exports and
    /// `n` is webpack's require.
    fn handle_factory_call(&mut self, assigned: Option<&str>, call: &CallExpression<'_>) {
        let Some(name) = callee_tail(call) else {
            return;
        };
        match name {
            "makeMessageType" => self.handle_make_message(assigned, call),
            "makeEnum" => self.handle_make_enum(assigned, call),
            "setEnumType" => self.handle_set_enum(call),
            "enumType" | "QT" => self.handle_compact_enum(assigned, call),
            "newFieldList" => self.handle_new_field_list(call),
            "d" => self.handle_webpack_export(call),
            _ => {}
        }
    }

    fn handle_make_message(&mut self, assigned: Option<&str>, call: &CallExpression<'_>) {
        let Some(type_name) = first_string_argument(call) else {
            return;
        };
        let fields = call
            .arguments
            .get(1)
            .and_then(argument_expr)
            .map(|expr| self.extract_fields_from_expr(expr))
            .unwrap_or_default();
        self.record_message(
            type_name,
            assigned.unwrap_or(""),
            "",
            fields,
            call.span.start,
        );
    }

    fn handle_make_enum(&mut self, assigned: Option<&str>, call: &CallExpression<'_>) {
        let Some(type_name) = first_string_argument(call) else {
            return;
        };
        let values = call
            .arguments
            .get(1)
            .and_then(argument_expr)
            .map(Self::parse_enum_values)
            .unwrap_or_default();
        self.record_enum(type_name, assigned.unwrap_or(""), values, call.span.start);
    }

    fn handle_set_enum(&mut self, call: &CallExpression<'_>) {
        let var_name = call
            .arguments
            .first()
            .and_then(argument_expr)
            .and_then(ident_name)
            .unwrap_or("");
        let Some(type_name) = call
            .arguments
            .get(1)
            .and_then(argument_expr)
            .and_then(string_lit)
        else {
            return;
        };
        let values = call
            .arguments
            .get(2)
            .and_then(argument_expr)
            .map(Self::parse_enum_values)
            .unwrap_or_default();
        self.record_enum(type_name, var_name, values, call.span.start);
    }

    fn handle_new_field_list(&mut self, call: &CallExpression<'_>) {
        let Some(fields_expr) = call.arguments.first().and_then(argument_expr) else {
            return;
        };
        let fields = self.extract_fields_from_expr(fields_expr);
        let Some(class) = self.current_class.clone() else {
            return;
        };
        let Some(type_name) = self.class_type_name() else {
            return;
        };
        self.record_message(
            &type_name,
            &class.var_name,
            &class.internal_name,
            fields,
            class.start,
        );
    }

    fn qualify_late_prefixes(&mut self) {
        let prefixes = std::mem::take(&mut self.compact_prefixes);
        let pending = std::mem::take(&mut self.pending_enum_pkgs);
        for (pos, pkg_expr) in pending {
            let Some(pkg) = resolve_compact_package_expr(&pkg_expr, pos, &prefixes) else {
                continue;
            };
            if !Self::keep_package(&pkg) {
                continue;
            }
            let Some(&index) = self.enum_by_pos.get(&pos) else {
                continue;
            };
            let item = &mut self.scan.enums[index];
            let short = if item.short_name.is_empty() {
                crate::ast_util::last_segment(&item.type_name).to_string()
            } else {
                crate::ast_util::last_segment(&item.short_name).to_string()
            };
            item.package.clone_from(&pkg);
            item.short_name.clone_from(&short);
            let type_name = format!("{pkg}.{short}");
            if item.type_name != type_name {
                self.enum_index.remove(&item.type_name);
                item.type_name = type_name;
                self.enum_index.insert(item.type_name.clone(), index);
            }
        }
        self.compact_prefixes = prefixes;
        let mut kept = Vec::with_capacity(self.scan.enums.len());
        let mut enum_index = HashMap::new();
        let mut enum_by_pos = HashMap::new();
        for item in self.scan.enums.drain(..) {
            if !(is_local_loop_pkg(&item.package)
                || is_google_pkg(&item.package)
                || item.package.is_empty())
            {
                continue;
            }
            enum_index.insert(item.type_name.clone(), kept.len());
            enum_by_pos.insert(item.pos, kept.len());
            kept.push(item);
        }
        self.scan.enums = kept;
        self.enum_index = enum_index;
        self.enum_by_pos = enum_by_pos;
    }

    fn class_type_name(&self) -> Option<String> {
        let class = self.current_class.as_ref()?;
        self.scan
            .messages
            .iter()
            .rev()
            .find(|message| message.var_name == class.var_name)
            .map(|message| message.type_name.clone())
    }

    fn handle_compact_enum(&mut self, assigned: Option<&str>, call: &CallExpression<'_>) {
        if call.arguments.len() < 4 {
            return;
        }
        let pkg_expr = call
            .arguments
            .get(1)
            .and_then(argument_expr)
            .and_then(|expr| {
                string_lit(expr)
                    .map(str::to_string)
                    .or_else(|| ident_name(expr).map(str::to_string))
            });
        let Some(pkg_expr) = pkg_expr else {
            return;
        };
        let Some(short_name) = call
            .arguments
            .get(2)
            .and_then(argument_expr)
            .and_then(string_lit)
        else {
            return;
        };
        let pkg = resolve_compact_package_expr(&pkg_expr, call.span.start, &self.compact_prefixes);
        if let Some(pkg) = &pkg
            && !Self::keep_package(pkg)
        {
            return;
        }
        let values = call
            .arguments
            .get(3)
            .and_then(argument_expr)
            .map(|expr| extract_compact_enum_values(self.source_span(expr.span()), short_name))
            .unwrap_or_default();
        let type_name = match &pkg {
            Some(pkg) => format!("{pkg}.{short_name}"),
            None => short_name.to_owned(),
        };
        self.record_enum(&type_name, assigned.unwrap_or(""), values, call.span.start);
        if pkg.is_none() {
            self.pending_enum_pkgs.push((call.span.start, pkg_expr));
        }
    }

    fn handle_webpack_export(&mut self, call: &CallExpression<'_>) {
        let Some(exports) = call.arguments.get(1).and_then(argument_expr) else {
            return;
        };
        let Expression::ObjectExpression(object) = exports.get_inner_expression() else {
            return;
        };
        let module_start = self.module_start_for(call.span.start);
        for property in &object.properties {
            let ObjectPropertyKind::ObjectProperty(prop) = property else {
                continue;
            };
            let Some(alias) = property_name(&prop.key) else {
                continue;
            };
            let Some(target) = arrow_ident(&prop.value) else {
                continue;
            };
            if alias != target {
                self.scan
                    .export_aliases
                    .push((module_start, alias.to_owned(), target.to_owned()));
            }
        }
    }

    fn handle_module_factory(&mut self, key: u32, params: &FormalParameters<'_>, span: Span) {
        // A webpack module factory takes `(module, exports, require)`; fewer
        // parameters means this numbered property is not a module.
        if params.items.len() >= 3 {
            self.module_starts.push(ModuleStart {
                start: span.start,
                id: key,
            });
        }
    }

    fn handle_require_call(&mut self, assigned: Option<&str>, call: &CallExpression<'_>) {
        if call.arguments.len() != 1 {
            return;
        }
        let Some(module_id) = call
            .arguments
            .first()
            .and_then(argument_expr)
            .and_then(number_lit)
            .and_then(f64_to_u32)
        else {
            return;
        };
        let Some(alias) = assigned else {
            return;
        };
        let current = self.module_start_for(call.span.start);
        let target_start = self
            .module_starts
            .iter()
            .find(|module| module.id == module_id)
            .map_or(module_id, |module| module.start);
        self.scan
            .module_imports
            .push((current, alias.to_owned(), target_start));
    }

    fn collect_class(&mut self, class: &Class<'_>, assigned: Option<&str>) {
        let internal = class
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .unwrap_or_default();
        let var_name = assigned
            .map(str::to_string)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| internal.clone());
        let extends = class
            .heritage_expression()
            .and_then(member_or_ident)
            .unwrap_or_default();
        let package_ref = class_package_ref(class).unwrap_or_default();
        let compact = CompactClass {
            name: if internal.is_empty() {
                var_name.clone()
            } else {
                internal.clone()
            },
            extends: last_segment(&extends).to_string(),
            start: class.span.start,
            end: class.span.end,
            package_ref,
        };
        self.compact_classes.push(compact);

        let mut type_name = None;
        let mut fields = None;
        let mut schema = None;
        for element in &class.body.body {
            match element {
                ClassElement::PropertyDefinition(prop) if prop.r#static => {
                    if property_name(&prop.key) == Some("typeName") {
                        type_name = prop.value.as_ref().and_then(string_lit).map(str::to_string);
                    }
                    if property_name(&prop.key) == Some("fields") {
                        fields = prop
                            .value
                            .as_ref()
                            .map(|expr| self.extract_fields_from_expr(expr));
                    }
                }
                ClassElement::MethodDefinition(method) if method.r#static => {
                    if property_name(&method.key) == Some("typeName") {
                        type_name = method_returned_string(method);
                    }
                    if property_name(&method.key) == Some("$") {
                        schema = method_returned_array(method);
                    }
                }
                _ => {}
            }
        }

        if let Some(type_name) = type_name {
            self.record_message(
                &type_name,
                &var_name,
                &internal,
                fields.unwrap_or_default(),
                class.span.start,
            );
        }
        if let Some(array) = schema {
            self.record_compact_schema(array, class.span.start);
        }
    }

    fn record_compact_schema(&mut self, array: &ArrayExpression<'_>, pos: u32) {
        let schema = self.source_span(array.span).to_owned();
        let Some(CompactSchema {
            short_name,
            fields,
            type_refs,
        }) = parse_compact_schema_array(&schema, &mut self.scan.diagnostics)
        else {
            return;
        };
        if google_well_known(&short_name).is_some() {
            return;
        }
        let Some(pkg) = package_for_compact_pos(pos, &self.compact_classes, &self.compact_prefixes)
        else {
            return;
        };
        if !Self::keep_package(&pkg) {
            return;
        }
        let type_name = format!("{pkg}.{short_name}");
        if self
            .pending_compact
            .iter()
            .any(|item| item.msg.type_name == type_name)
        {
            return;
        }
        self.push_declared(&type_name);
        self.pending_compact.push(CompactPendingMessage {
            msg: Message {
                type_name,
                var_name: compact_class_name_at(pos, &self.compact_classes),
                internal_name: String::new(),
                fields,
                package: pkg,
                short_name,
                pos,
                module_start: self.module_start_for(pos),
            },
            type_refs,
        });
    }
    /// Classifies the expression bound to `name` at `pos`, whichever syntax
    /// (variable declaration, assignment, or sequence tail) produced it.
    fn classify_init(&mut self, name: &str, pos: u32, expr: &Expression<'_>) {
        self.maybe_package_prefix(name, expr, pos);
        match expr.get_inner_expression() {
            Expression::Identifier(id) => {
                self.add_alias(pos, name, id.name.as_str());
            }
            Expression::ClassExpression(class) => self.collect_class(class, Some(name)),
            Expression::CallExpression(call) => {
                self.handle_factory_call(Some(name), call);
                self.handle_require_call(Some(name), call);
            }
            Expression::SequenceExpression(seq) => {
                let Some(last) = seq.expressions.last() else {
                    return;
                };
                match last.get_inner_expression() {
                    Expression::Identifier(id) => {
                        self.add_alias(pos, name, id.name.as_str());
                    }
                    Expression::CallExpression(call) => {
                        self.handle_factory_call(Some(name), call);
                        self.handle_require_call(Some(name), call);
                    }
                    _ => {}
                }
            }
            Expression::ObjectExpression(object) => {
                self.parse_service_from_object(object, name, pos);
            }
            _ => {}
        }
    }
}

impl<'a> Visit<'a> for Scanner<'a> {
    fn visit_object_property(&mut self, prop: &oxc::ast::ast::ObjectProperty<'a>) {
        if let Some(key) = property_numeric_key(&prop.key) {
            match &prop.value {
                Expression::FunctionExpression(func) => {
                    self.handle_module_factory(key, &func.params, prop.span);
                }
                Expression::ArrowFunctionExpression(arrow) => {
                    self.handle_module_factory(key, &arrow.params, prop.span);
                }
                _ => {}
            }
        }
        walk::walk_object_property(self, prop);
    }

    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        if let Some(name) = binding_ident(&decl.id)
            && let Some(init) = &decl.init
        {
            self.classify_init(name, decl.span.start, init);
        }
        walk::walk_variable_declarator(self, decl);
    }

    fn visit_assignment_expression(&mut self, expr: &oxc::ast::ast::AssignmentExpression<'a>) {
        if let Some(name) = assignment_ident(&expr.left) {
            self.classify_init(name, expr.span.start, &expr.right);
        }
        if let Some((object, prop)) = assignment_member(&expr.left) {
            let assigned_var =
                if matches!(object.get_inner_expression(), Expression::ThisExpression(_)) {
                    self.current_class
                        .as_ref()
                        .map(|class| class.var_name.clone())
                } else {
                    ident_name(object).map(str::to_string)
                };
            if prop == "typeName"
                && let (Some(type_name), Some(var_name)) =
                    (string_lit(&expr.right), assigned_var.clone())
            {
                let internal = self
                    .current_class
                    .as_ref()
                    .map(|class| class.internal_name.clone())
                    .unwrap_or_default();
                self.record_message(type_name, &var_name, &internal, Vec::new(), expr.span.start);
            }
            if prop == "fields"
                && let Some(var_name) = assigned_var
            {
                let fields = self.extract_fields_from_expr(&expr.right);
                if let Some(message) = self
                    .scan
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.var_name == var_name && message.fields.is_empty())
                {
                    message.fields = fields;
                }
            }
        }
        walk::walk_assignment_expression(self, expr);
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        if class.is_declaration() {
            let name = class
                .id
                .as_ref()
                .map(|id| id.name.as_str().to_string())
                .unwrap_or_default();
            self.collect_class(class, Some(name.as_str()));
        }
        let previous = self.current_class.replace(CurrentClass {
            var_name: class
                .id
                .as_ref()
                .map(|id| id.name.as_str().to_string())
                .unwrap_or_default(),
            internal_name: class
                .id
                .as_ref()
                .map(|id| id.name.as_str().to_string())
                .unwrap_or_default(),
            start: class.span.start,
        });
        walk::walk_class(self, class);
        self.current_class = previous;
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        self.handle_factory_call(None, call);
        // `callee_tail` already returns the identifier name for a bare `n(…)`
        // callee and the last member's name for `a.b.n(…)`.
        if callee_tail(call) == Some("n") {
            self.handle_require_call(None, call);
        }
        walk::walk_call_expression(self, call);
    }

    fn visit_object_expression(&mut self, object: &ObjectExpression<'a>) {
        self.parse_service_from_object(object, "", object.span.start);
        walk::walk_object_expression(self, object);
    }
}

/// The literal string passed as a call's first argument.
fn first_string_argument<'a>(call: &'a CallExpression<'a>) -> Option<&'a str> {
    call.arguments
        .first()
        .and_then(argument_expr)
        .and_then(string_lit)
}

/// Parses one `{no, name, kind, …}` field object out of the bundle AST.
///
/// # Errors
///
/// Returns [`Error::Field`] when the expression is not an object, or when it
/// lacks a numeric `no`, a valid `name`, or a string `kind`.
pub fn parse_field_object(expr: &Expression<'_>) -> Result<Field, Error> {
    let Expression::ObjectExpression(_) = expr.get_inner_expression() else {
        return Err(Error::Field("missing field object".into()));
    };
    let no = object_prop(expr, "no")
        .and_then(number_lit)
        .and_then(f64_to_i32)
        .ok_or_else(|| Error::Field("missing field no".into()))?;
    let name = object_prop(expr, "name")
        .and_then(string_lit)
        .ok_or_else(|| Error::Field("missing field name".into()))?;
    if !is_field_name(name) {
        return Err(Error::Field(format!("invalid field name: {name}")));
    }
    let raw_kind = object_prop(expr, "kind")
        .and_then(string_lit)
        .ok_or_else(|| Error::Field("missing field kind".into()))?;
    let mut field = Field::new(no, name, FieldKind::parse(raw_kind));
    if let Some(enum_call) =
        object_prop(expr, "T").and_then(|value| match value.get_inner_expression() {
            Expression::CallExpression(call) if callee_tail(call) == Some("getEnumType") => {
                Some(call)
            }
            _ => None,
        })
    {
        if let Some(name) = enum_call
            .arguments
            .first()
            .and_then(argument_expr)
            .and_then(member_or_ident)
        {
            field.t = Some(TypeToken::Named(name));
        }
    } else if let Some(token) = object_prop(expr, "T").and_then(type_token_from_expr) {
        field.t = Some(token);
    } else if object_prop_shorthand(expr, "T") {
        field.t = Some(TypeToken::Named("T".into()));
    }
    if let Some(oneof) = object_prop(expr, "oneof").and_then(string_lit)
        && is_ident(oneof)
        && !oneof.contains('$')
    {
        field.oneof = Some(oneof.to_owned());
    }
    if object_prop(expr, "repeated").is_some_and(is_js_true) {
        field.repeated = true;
    }
    if object_prop(expr, "opt").is_some_and(is_js_true) {
        field.opt = true;
    }
    if field.kind == FieldKind::Map {
        if let Some(key) = object_prop(expr, "K")
            .and_then(number_lit)
            .and_then(f64_to_i32)
        {
            field.map_key = Some(key);
        }
        if let Some(value) = object_prop(expr, "V") {
            if let Some(raw) = object_prop(value, "kind").and_then(string_lit) {
                field.map_value_kind = ValueKind::parse(raw);
            }
            field.map_value_t = object_prop(value, "T").and_then(type_token_from_expr);
        }
    }
    Ok(field)
}

fn type_token_from_expr(expr: &Expression<'_>) -> Option<TypeToken> {
    match number_lit(expr) {
        Some(number) => f64_to_i32(number).map(TypeToken::Scalar),
        None => member_or_ident(expr).map(TypeToken::Named),
    }
}

fn parse_methods(object: &ObjectExpression<'_>) -> Vec<Method> {
    let mut methods = Vec::new();
    for property in &object.properties {
        let ObjectPropertyKind::ObjectProperty(prop) = property else {
            continue;
        };
        let Some(name) = object_prop(&prop.value, "name").and_then(string_lit) else {
            continue;
        };
        let Some(input) = object_prop(&prop.value, "I").and_then(member_or_ident) else {
            continue;
        };
        let Some(output) = object_prop(&prop.value, "O").and_then(member_or_ident) else {
            continue;
        };
        let kind = object_prop(&prop.value, "kind")
            .and_then(member_or_ident)
            .map_or(MethodKind::Unary, |value| {
                MethodKind::parse(last_segment(&value))
            });
        methods.push(Method {
            name: name.to_string(),
            input_type: input,
            output_type: output,
            kind,
        });
    }
    methods
}

fn arrow_ident<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr.get_inner_expression() {
        Expression::ArrowFunctionExpression(arrow) => arrow.get_expression().and_then(ident_name),
        Expression::Identifier(id) => Some(id.name.as_str()),
        _ => None,
    }
}

fn class_package_ref(class: &Class<'_>) -> Option<String> {
    for element in &class.body.body {
        let ClassElement::MethodDefinition(method) = element else {
            continue;
        };
        if property_name(&method.key) != Some("$p") {
            continue;
        }
        if let Some(returned) = method_returned_expr(method) {
            if let Some(value) = string_lit(returned) {
                return Some(value.to_string());
            }
            if let Some(value) = ident_name(returned) {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn method_returned_expr<'a>(method: &'a MethodDefinition<'a>) -> Option<&'a Expression<'a>> {
    method
        .value
        .body
        .as_ref()
        .and_then(|body| returned_expression(body))
}

fn method_returned_string(method: &MethodDefinition<'_>) -> Option<String> {
    method_returned_expr(method)
        .and_then(string_lit)
        .map(str::to_string)
}

fn method_returned_array<'a>(method: &'a MethodDefinition<'a>) -> Option<&'a ArrayExpression<'a>> {
    method_returned_expr(method).and_then(array_from_expr)
}
