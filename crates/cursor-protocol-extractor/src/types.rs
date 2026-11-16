//! Domain types for extracted protobuf declarations.

use std::collections::HashMap;
use std::fmt;

pub const SCALAR_TYPES: [(i32, &str); 15] = [
    (1, "double"),
    (2, "float"),
    (3, "int64"),
    (4, "uint64"),
    (5, "int32"),
    (6, "fixed64"),
    (7, "fixed32"),
    (8, "bool"),
    (9, "string"),
    (12, "bytes"),
    (13, "uint32"),
    (15, "sfixed32"),
    (16, "sfixed64"),
    (17, "sint32"),
    (18, "sint64"),
];

pub fn scalar_name(id: i32) -> Option<&'static str> {
    SCALAR_TYPES
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, name)| *name)
}

pub fn is_scalar_id(id: i32) -> bool {
    scalar_name(id).is_some()
}

pub fn is_google_pkg(pkg: &str) -> bool {
    pkg == "google.protobuf" || pkg == "google.rpc"
}

/// Packages the local loop actually compiles and serves.
pub const LOCAL_LOOP_PACKAGES: [&str; 2] = ["agent.v1", "aiserver.v1"];

pub fn is_local_loop_pkg(pkg: &str) -> bool {
    LOCAL_LOOP_PACKAGES.contains(&pkg)
}

pub const GOOGLE_WELL_KNOWN: [(&str, &str); 7] = [
    ("Any", "google.protobuf.Any"),
    ("Duration", "google.protobuf.Duration"),
    ("Empty", "google.protobuf.Empty"),
    ("ListValue", "google.protobuf.ListValue"),
    ("Struct", "google.protobuf.Struct"),
    ("Timestamp", "google.protobuf.Timestamp"),
    ("Value", "google.protobuf.Value"),
];

pub fn google_well_known(short: &str) -> Option<&'static str> {
    GOOGLE_WELL_KNOWN
        .iter()
        .find(|(key, _)| *key == short)
        .map(|(_, name)| *name)
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeToken {
    Scalar(i32),
    Named(String),
}

impl TypeToken {
    pub fn as_named(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name),
            Self::Scalar(_) => None,
        }
    }

    pub fn completeness(&self) -> i32 {
        match self {
            Self::Named(name) if looks_like_full_type_name(name) || google_well_known(name).is_some() => {
                3
            }
            Self::Named(_) => 1,
            Self::Scalar(_) => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub no: i32,
    pub name: String,
    pub kind: String,
    pub t: Option<TypeToken>,
    pub oneof: String,
    pub repeated: bool,
    pub opt: bool,
    pub map_key: i32,
    pub map_value_kind: String,
    pub map_value_t: Option<TypeToken>,
}

impl Field {
    pub fn new(no: i32, name: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            no,
            name: name.into(),
            kind: kind.into(),
            t: None,
            oneof: String::new(),
            repeated: false,
            opt: false,
            map_key: 0,
            map_value_kind: String::new(),
            map_value_t: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub type_name: String,
    pub var_name: String,
    pub internal_name: String,
    pub fields: Vec<Field>,
    pub package: String,
    pub short_name: String,
    pub pos: u32,
    pub module_start: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumValue {
    pub no: i32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Enum {
    pub type_name: String,
    pub var_name: String,
    pub values: Vec<EnumValue>,
    pub package: String,
    pub short_name: String,
    pub pos: u32,
    pub module_start: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Method {
    pub name: String,
    pub input_type: String,
    pub output_type: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub type_name: String,
    pub var_name: String,
    pub methods: Vec<Method>,
    pub package: String,
    pub short_name: String,
    pub pos: u32,
    pub module_start: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolDef {
    pub type_name: String,
    pub pos: u32,
    pub kind: String,
    pub module_start: u32,
}

#[derive(Debug, Default)]
pub struct Diagnostics {
    pub total_field_objects: usize,
    pub parsed_field_objects: usize,
    pub skipped_field_objects: usize,
    pub skipped_field_samples: Vec<String>,
    pub unresolved_type_refs: HashMap<String, usize>,
    pub empty_messages: Vec<String>,
    pub placeholder_hits: Vec<String>,
    pub declared_types: usize,
    pub extracted_types: usize,
    pub missing_declarations: Vec<String>,
    pub parse_errors: Vec<String>,
    pub shape_errors: Vec<String>,
}

impl Diagnostics {
    pub fn add_skipped_field(&mut self, field_object: &str, reason: &str) {
        self.total_field_objects += 1;
        self.skipped_field_objects += 1;
        if self.skipped_field_samples.len() < 20 {
            let mut trimmed = field_object.trim().to_string();
            if trimmed.len() > 140 {
                trimmed.truncate(140);
                trimmed.push_str("...");
            }
            self.skipped_field_samples
                .push(format!("{reason} | {trimmed}"));
        }
    }

    pub fn add_parsed_field(&mut self) {
        self.total_field_objects += 1;
        self.parsed_field_objects += 1;
    }

    pub fn add_unresolved_type(&mut self, r#ref: &str) {
        let key = if r#ref.trim().is_empty() {
            "<empty>".to_string()
        } else {
            r#ref.trim().to_string()
        };
        *self.unresolved_type_refs.entry(key).or_insert(0) += 1;
    }

    pub fn add_parse_error(&mut self, message: &str) {
        if self.parse_errors.len() < 20 {
            self.parse_errors.push(message.to_string());
        } else if self.parse_errors.len() == 20 {
            self.parse_errors
                .push(format!("... and more parse errors after {message}"));
        }
    }

    pub fn add_shape_error(&mut self, message: &str) {
        self.shape_errors.push(message.to_string());
    }

    pub fn merge_from(&mut self, other: &mut Diagnostics) {
        self.total_field_objects += other.total_field_objects;
        self.parsed_field_objects += other.parsed_field_objects;
        self.skipped_field_objects += other.skipped_field_objects;
        self.skipped_field_samples
            .append(&mut other.skipped_field_samples);
        if self.skipped_field_samples.len() > 20 {
            self.skipped_field_samples.truncate(20);
        }
        self.parse_errors.append(&mut other.parse_errors);
        if self.parse_errors.len() > 20 {
            self.parse_errors.truncate(20);
        }
    }

    pub fn has_failure(&self, validate_err: bool) -> bool {
        validate_err
            || self.skipped_field_objects > 0
            || !self.unresolved_type_refs.is_empty()
            || !self.placeholder_hits.is_empty()
            || !self.missing_declarations.is_empty()
            || !self.parse_errors.is_empty()
            || !self.shape_errors.is_empty()
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "诊断汇总: fields {}/{} 解析成功, declarations {}/{} 已提取, skipped={}, unresolved={}, placeholders={}, empty_messages={}, parse_errors={}, shape_errors={}",
            self.parsed_field_objects,
            self.total_field_objects,
            self.extracted_types,
            self.declared_types,
            self.skipped_field_objects,
            self.unresolved_type_refs.len(),
            self.placeholder_hits.len(),
            self.empty_messages.len(),
            self.parse_errors.len(),
            self.shape_errors.len()
        )?;
        if self.skipped_field_objects > 0 {
            writeln!(f, "字段解析失败样例:")?;
            for sample in &self.skipped_field_samples {
                writeln!(f, "  - {sample}")?;
            }
        }
        if !self.parse_errors.is_empty() {
            writeln!(f, "JS bundle 解析错误:")?;
            for error in &self.parse_errors {
                writeln!(f, "  - {error}")?;
            }
        }
        if !self.shape_errors.is_empty() {
            writeln!(f, "必要形状校验失败:")?;
            for error in &self.shape_errors {
                writeln!(f, "  - {error}")?;
            }
        }
        if !self.unresolved_type_refs.is_empty() {
            writeln!(f, "未解析类型引用:")?;
            let mut keys: Vec<_> = self.unresolved_type_refs.keys().cloned().collect();
            keys.sort();
            for key in keys {
                let count = self.unresolved_type_refs[&key];
                writeln!(f, "  - {key} ({count})")?;
            }
        }
        if !self.placeholder_hits.is_empty() {
            writeln!(f, "占位字段命中:")?;
            for (i, hit) in self.placeholder_hits.iter().enumerate() {
                if i >= 20 {
                    writeln!(
                        f,
                        "  - ... and {} more",
                        self.placeholder_hits.len() - 20
                    )?;
                    break;
                }
                writeln!(f, "  - {hit}")?;
            }
        }
        if !self.missing_declarations.is_empty() {
            writeln!(f, "未提取的 Proto 声明:")?;
            for (i, name) in self.missing_declarations.iter().enumerate() {
                if i >= 20 {
                    writeln!(
                        f,
                        "  - ... and {} more",
                        self.missing_declarations.len() - 20
                    )?;
                    break;
                }
                writeln!(f, "  - {name}")?;
            }
        }
        Ok(())
    }
}

pub fn looks_like_full_type_name(r#ref: &str) -> bool {
    let trimmed = r#ref.trim();
    if trimmed.starts_with("google.protobuf.") || trimmed.starts_with("google.rpc.") {
        return true;
    }
    looks_like_versioned_type(trimmed)
}

pub fn looks_like_versioned_type(value: &str) -> bool {
    let mut parts = value.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty() || !is_ident(first) {
        return false;
    }
    let mut saw_version = false;
    let mut after_version = 0usize;
    for part in parts {
        if part.is_empty() || !is_ident(part) {
            return false;
        }
        if is_version_token(part) {
            saw_version = true;
            after_version = 0;
        } else if saw_version {
            after_version += 1;
        }
    }
    saw_version && after_version > 0
}

pub fn looks_like_package_name(value: &str) -> bool {
    let trimmed = value.trim().trim_matches(|c| c == '"' || c == '\'').trim_end_matches('.');
    let mut parts = trimmed.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty() || !first.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) || !is_ident(first)
    {
        return false;
    }
    let rest: Vec<_> = parts.collect();
    if rest.is_empty() {
        return false;
    }
    is_version_token(rest[rest.len() - 1])
        && rest.iter().all(|part| is_ident(part))
}

pub fn parse_type_name(type_name: &str) -> (String, String) {
    if let Some(rest) = type_name.strip_prefix("google.protobuf.") {
        return ("google.protobuf".into(), rest.to_string());
    }
    if let Some(rest) = type_name.strip_prefix("google.rpc.") {
        return ("google.rpc".into(), rest.to_string());
    }
    if let Some((pkg, short)) = split_versioned(type_name) {
        return (pkg, short);
    }
    match type_name.rsplit_once('.') {
        Some((pkg, short)) => (pkg.to_string(), short.to_string()),
        None => (String::new(), type_name.to_string()),
    }
}

fn split_versioned(type_name: &str) -> Option<(String, String)> {
    let bytes = type_name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' && i + 2 < bytes.len() && bytes[i + 1] == b'v' {
            let mut j = i + 2;
            if j < bytes.len() && bytes[j].is_ascii_digit() {
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'.' {
                    return Some((
                        type_name[..j].to_string(),
                        type_name[j + 1..].to_string(),
                    ));
                }
            }
        }
        i += 1;
    }
    None
}

fn is_version_token(part: &str) -> bool {
    let mut chars = part.chars();
    if chars.next() != Some('v') {
        return false;
    }
    let rest: String = chars.collect();
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

pub fn is_ident(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

pub fn is_field_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
