//! Compact protobuf `$()` schema and `enumType` / `QT` value parsing.

use crate::types::{
    Diagnostics, Enum, EnumValue, Field, Message, TypeToken, google_well_known, is_field_name,
    is_scalar_id, looks_like_full_type_name, looks_like_package_name,
};

#[derive(Debug, Clone)]
pub struct CompactClass {
    pub name: String,
    pub extends: String,
    pub start: u32,
    pub end: u32,
    pub package_ref: String,
}

#[derive(Debug, Clone)]
pub struct CompactPrefix {
    pub name: String,
    pub package: String,
    pub pos: u32,
}

pub struct CompactPendingMessage {
    pub msg: Message,
    pub type_refs: Vec<String>,
}

pub fn package_for_compact_pos(
    pos: u32,
    classes: &[CompactClass],
    prefixes: &[CompactPrefix],
) -> String {
    let mut current = innermost_compact_class(pos, classes);
    let mut seen = Vec::new();
    while let Some(class) = current {
        if seen.iter().any(|name| name == &class.name) {
            break;
        }
        seen.push(class.name.clone());
        let pkg = resolve_compact_package_expr(&class.package_ref, class.start, prefixes);
        if !pkg.is_empty() {
            return pkg;
        }
        current = compact_class_by_name(&class.extends, class.start, classes);
    }
    String::new()
}

pub fn innermost_compact_class(pos: u32, classes: &[CompactClass]) -> Option<&CompactClass> {
    classes
        .iter()
        .filter(|class| class.start <= pos && pos < class.end)
        .max_by_key(|class| class.start)
}

pub fn compact_class_by_name<'a>(
    name: &str,
    pos: u32,
    classes: &'a [CompactClass],
) -> Option<&'a CompactClass> {
    let name = crate::ast_util::last_segment(name.trim());
    if name.is_empty() {
        return None;
    }
    classes
        .iter()
        .filter(|class| class.name == name)
        .min_by_key(|class| (class.start.abs_diff(pos), class.start))
}

pub fn compact_class_name_at(pos: u32, classes: &[CompactClass]) -> String {
    innermost_compact_class(pos, classes)
        .map(|class| class.name.clone())
        .unwrap_or_default()
}

pub fn resolve_compact_package_expr(expr: &str, pos: u32, prefixes: &[CompactPrefix]) -> String {
    let expr = expr
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim_end_matches('.')
        .to_string();
    if expr.is_empty() {
        return String::new();
    }
    if looks_like_package_name(&expr) {
        return expr;
    }
    prefixes
        .iter()
        .filter(|prefix| prefix.name == expr)
        .min_by_key(|prefix| (prefix.pos.abs_diff(pos), prefix.pos))
        .map(|prefix| prefix.package.clone())
        .unwrap_or_default()
}

pub fn parse_compact_schema_array(
    array_text: &str,
    diag: &mut Diagnostics,
) -> Option<(String, Vec<Field>, Vec<String>)> {
    let body = array_text
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if body.is_empty() {
        return None;
    }
    let (schema, rest) = split_compact_first_string(body)?;
    let schema = schema.replace("\\\n", "").replace("\\\r\n", "");
    let mut parts = schema.split('|');
    let short_name = parts.next()?.trim();
    if short_name.is_empty() {
        return None;
    }
    let type_refs = parse_compact_type_refs(rest.trim());
    let mut fields = Vec::new();
    for part in parts {
        match parse_compact_field(part.trim()) {
            Ok(field) => {
                diag.add_parsed_field();
                fields.push(field);
            }
            Err(err) => diag.add_skipped_field(part, &err),
        }
    }
    Some((short_name.to_string(), fields, type_refs))
}

pub fn split_compact_first_string(body: &str) -> Option<(String, &str)> {
    let body = body.trim();
    let mut chars = body.chars();
    let quote = chars.next()?;
    let quote_byte = match quote {
        '"' => b'"',
        '\'' => b'\'',
        _ => return None,
    };
    let bytes = body.as_bytes();
    let mut escaped = false;
    for i in 1..bytes.len() {
        let ch = bytes[i];
        if escaped {
            escaped = false;
            continue;
        }
        if ch == b'\\' {
            escaped = true;
            continue;
        }
        if ch == quote_byte {
            let rest = body[i + 1..].trim();
            let rest = rest.strip_prefix(',').unwrap_or(rest).trim();
            return Some((body[1..i].to_string(), rest));
        }
    }
    None
}

pub fn parse_compact_type_refs(rest: &str) -> Vec<String> {
    if rest.is_empty() {
        return Vec::new();
    }
    let mut refs = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let bytes = rest.as_bytes();
    for i in 0..=bytes.len() {
        if i < bytes.len() {
            match bytes[i] {
                b'[' | b'(' => {
                    depth += 1;
                    continue;
                }
                b']' | b')' => {
                    depth -= 1;
                    continue;
                }
                b',' if depth == 0 => {}
                _ => continue,
            }
        }
        let r#ref = rest[start..i].trim();
        if !r#ref.is_empty() {
            refs.push(r#ref.to_string());
        }
        start = i + 1;
    }
    refs
}

pub fn parse_compact_field(spec: &str) -> Result<Field, String> {
    let mut parts = spec.splitn(3, char::is_whitespace);
    let no = parts
        .next()
        .ok_or_else(|| format!("invalid compact field: {spec}"))?
        .parse::<i32>()
        .map_err(|_| format!("invalid compact field: {spec}"))?;
    let name = parts
        .next()
        .ok_or_else(|| format!("invalid compact field: {spec}"))?;
    if !is_field_name(name) {
        return Err(format!("invalid compact field: {spec}"));
    }
    let type_spec = parts.next().unwrap_or("").trim();
    let (type_part, oneof) = split_compact_type_and_oneof(type_spec);
    let (raw, flag) = split_compact_flag(type_part);
    let mut field = Field::new(no, name, "scalar");
    field.repeated = flag == Some('*');
    field.opt = flag == Some('?');
    field.oneof = oneof.unwrap_or_default();

    if let Some((key, value)) = raw.split_once(',') {
        field.kind = "map".into();
        let scalar = compact_scalar(key).ok_or_else(|| format!("invalid compact field: {spec}"))?;
        field.map_key = scalar;
        let (kind, token) = compact_type_token(value);
        field.map_value_kind = kind;
        field.map_value_t = Some(token);
        return Ok(field);
    }
    let (kind, token) = compact_type_token(raw);
    field.kind = kind;
    field.t = Some(token);
    Ok(field)
}

fn split_compact_type_and_oneof(spec: &str) -> (&str, Option<String>) {
    match spec.split_once(char::is_whitespace) {
        Some((ty, oneof)) if is_field_name(oneof) => (ty, Some(oneof.to_string())),
        _ => (spec, None),
    }
}

fn split_compact_flag(spec: &str) -> (&str, Option<char>) {
    let bytes = spec.as_bytes();
    match bytes.last() {
        Some(b'*' | b'?') => {
            let flag = spec.chars().next_back();
            (&spec[..spec.len() - 1], flag)
        }
        _ => (spec, None),
    }
}

fn compact_scalar(token: &str) -> Option<i32> {
    let value = token.parse::<i32>().ok()?;
    is_scalar_id(value).then_some(value)
}

fn compact_type_token(token: &str) -> (String, TypeToken) {
    if let Some(scalar) = compact_scalar(token) {
        ("scalar".into(), TypeToken::Scalar(scalar))
    } else {
        ("message".into(), TypeToken::Named(token.to_string()))
    }
}

pub fn compact_type_index(
    pending: &[CompactPendingMessage],
    enums: &[Enum],
) -> (
    std::collections::HashMap<String, String>,
    std::collections::HashMap<String, String>,
) {
    let mut by_var = std::collections::HashMap::new();
    let mut by_short = std::collections::HashMap::new();
    let mut add = |var_name: &str, short_name: &str, type_name: &str| {
        if type_name.is_empty() {
            return;
        }
        if !var_name.is_empty() {
            by_var.insert(var_name.to_string(), type_name.to_string());
        }
        if !short_name.is_empty() {
            by_short.insert(short_name.to_string(), type_name.to_string());
            if let Some((_, last)) = short_name.rsplit_once('.') {
                by_short.insert(last.to_string(), type_name.to_string());
            }
        }
    };
    for item in pending {
        add(
            &item.msg.var_name,
            &item.msg.short_name,
            &item.msg.type_name,
        );
    }
    for enum_item in enums {
        add(
            &enum_item.var_name,
            &enum_item.short_name,
            &enum_item.type_name,
        );
    }
    (by_var, by_short)
}

pub fn qualify_compact_fields(
    fields: &mut [Field],
    type_refs: &[String],
    pkg: &str,
    by_var: &std::collections::HashMap<String, String>,
    by_short: &std::collections::HashMap<String, String>,
) {
    let qualify = |token: Option<TypeToken>| match token {
        Some(TypeToken::Named(r#ref)) if r#ref.starts_with('#') => {
            let index = r#ref.trim_start_matches('#').parse::<usize>().ok();
            match index.and_then(|i| type_refs.get(i)) {
                Some(resolved) => Some(TypeToken::Named(qualify_compact_type_ref(
                    resolved, pkg, by_var, by_short,
                ))),
                None => Some(TypeToken::Named(r#ref)),
            }
        }
        other => other,
    };
    for field in fields {
        if field.kind == "map" {
            field.map_value_t = qualify(field.map_value_t.take());
        } else {
            field.t = qualify(field.t.take());
        }
    }
}

pub fn qualify_compact_type_ref(
    r#ref: &str,
    pkg: &str,
    by_var: &std::collections::HashMap<String, String>,
    by_short: &std::collections::HashMap<String, String>,
) -> String {
    let r#ref = r#ref.trim();
    if r#ref.is_empty() {
        return r#ref.to_string();
    }
    if let Some(type_name) = by_var.get(r#ref) {
        return type_name.clone();
    }
    let last = crate::ast_util::last_segment(r#ref);
    if let Some(type_name) = by_var.get(last) {
        return type_name.clone();
    }
    if let Some(type_name) = google_well_known(last) {
        return type_name.to_string();
    }
    if looks_like_full_type_name(r#ref) {
        return r#ref.to_string();
    }
    if let Some(type_name) = by_short.get(last) {
        return type_name.clone();
    }
    if pkg.is_empty() {
        last.to_string()
    } else {
        format!("{pkg}.{last}")
    }
}

pub fn extract_compact_enum_values(array_text: &str, short_name: &str) -> Vec<EnumValue> {
    let prefix = compact_enum_prefix(short_name);
    let mut values = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let bytes = array_text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some((no, name, next)) = parse_enum_pair(array_text, i) {
                if seen.insert(no) {
                    let name = if !prefix.is_empty() && !name.starts_with(&prefix) {
                        format!("{prefix}{name}")
                    } else {
                        name
                    };
                    values.push(EnumValue { no, name });
                }
                i = next;
                continue;
            }
        }
        i += 1;
    }
    values
}

fn parse_enum_pair(text: &str, start: usize) -> Option<(i32, String, usize)> {
    let rest = &text[start..];
    let end = rest.find(']')?;
    let inner = rest[1..end].trim();
    let (no, name) = inner.split_once(',')?;
    let no = no.trim().parse::<i32>().ok()?;
    let name = name
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_string();
    Some((no, name, start + end + 1))
}

pub fn compact_enum_prefix(short_name: &str) -> String {
    let name = crate::ast_util::last_segment(short_name);
    if name.is_empty() {
        return String::new();
    }
    let mut prefix = String::new();
    for (i, ch) in name.chars().enumerate() {
        if i > 0 && ch.is_ascii_uppercase() {
            prefix.push('_');
        }
        prefix.push(ch);
    }
    format!("{}_", prefix.to_ascii_uppercase())
}

pub fn finish_compact_messages(
    mut pending: Vec<CompactPendingMessage>,
    enums: &[Enum],
) -> Vec<Message> {
    let (by_var, by_short) = compact_type_index(&pending, enums);
    for item in &mut pending {
        qualify_compact_fields(
            &mut item.msg.fields,
            &item.type_refs,
            &item.msg.package,
            &by_var,
            &by_short,
        );
    }
    pending.into_iter().map(|item| item.msg).collect()
}
