//! Domain types for extracted protobuf declarations.

use std::collections::HashMap;
use std::fmt;

use crate::error::Error;

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

#[must_use]
pub fn scalar_name(id: i32) -> Option<&'static str> {
    SCALAR_TYPES
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, name)| *name)
}

#[must_use]
pub fn is_scalar_id(id: i32) -> bool {
    scalar_name(id).is_some()
}

#[must_use]
pub fn is_google_pkg(pkg: &str) -> bool {
    pkg == "google.protobuf" || pkg == "google.rpc"
}

/// Packages the local loop actually compiles and serves.
pub const LOCAL_LOOP_PACKAGES: [&str; 2] = ["agent.v1", "aiserver.v1"];

#[must_use]
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

#[must_use]
pub fn google_well_known(short: &str) -> Option<&'static str> {
    GOOGLE_WELL_KNOWN
        .iter()
        .find(|(key, _)| *key == short)
        .map(|(_, name)| *name)
}

/// How a proto declaration stores its value: inline scalar, named message,
/// named enum, or a map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldKind {
    Scalar,
    Message,
    Enum,
    Map,
}

impl FieldKind {
    /// Reads a bundle `kind` string; unknown kinds are treated as scalars,
    /// which is what the renderer already did with them.
    #[must_use]
    pub fn from_bundle_kind(value: &str) -> Self {
        match value.trim() {
            "message" => Self::Message,
            "enum" => Self::Enum,
            "map" => Self::Map,
            _ => Self::Scalar,
        }
    }

    /// Whether the declaration points at a named type.
    #[must_use]
    pub fn is_named(self) -> bool {
        matches!(self, Self::Message | Self::Enum)
    }

    /// The named-kind equivalent of a map value, for symbol lookups.
    #[must_use]
    pub fn of_value(kind: ValueKind) -> Self {
        match kind {
            ValueKind::Enum => Self::Enum,
            ValueKind::Scalar => Self::Scalar,
            ValueKind::Message => Self::Message,
        }
    }
}

/// The value half of a map field, which cannot itself be a map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    Scalar,
    Message,
    Enum,
}

impl ValueKind {
    #[must_use]
    pub fn from_bundle_kind(value: &str) -> Self {
        match value.trim() {
            "message" => Self::Message,
            "enum" => Self::Enum,
            _ => Self::Scalar,
        }
    }

    #[must_use]
    pub fn is_named(self) -> bool {
        matches!(self, Self::Message | Self::Enum)
    }
}

/// How a service method streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MethodKind {
    #[default]
    Unary,
    ServerStreaming,
    ClientStreaming,
    BiDiStreaming,
}

impl MethodKind {
    #[must_use]
    pub fn from_bundle_kind(value: &str) -> Self {
        match value.trim() {
            "ServerStreaming" => Self::ServerStreaming,
            "ClientStreaming" => Self::ClientStreaming,
            "BiDiStreaming" => Self::BiDiStreaming,
            _ => Self::Unary,
        }
    }
}

/// What a resolved symbol is declared as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Message,
    Enum,
}

impl SymbolKind {
    #[must_use]
    pub fn of(kind: FieldKind) -> Self {
        match kind {
            FieldKind::Enum => Self::Enum,
            FieldKind::Scalar | FieldKind::Message | FieldKind::Map => Self::Message,
        }
    }

    #[must_use]
    pub fn of_value(kind: ValueKind) -> Self {
        match kind {
            ValueKind::Enum => Self::Enum,
            ValueKind::Scalar | ValueKind::Message => Self::Message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeToken {
    Scalar(i32),
    Named(String),
}

impl TypeToken {
    /// How completely this token names its type: `3` for a fully-qualified or
    /// well-known name, `1` for a bare name, `0` for a scalar. The scores are
    /// only compared against each other when picking between duplicate
    /// declarations, so they are tier labels, not measurements.
    #[must_use]
    pub fn completeness(&self) -> i32 {
        match self {
            Self::Named(name) => named_completeness(name),
            Self::Scalar(_) => 0,
        }
    }
}

/// The completeness tier of a bare type reference, without allocating a
/// [`TypeToken`] just to score it.
#[must_use]
pub fn named_completeness(type_ref: &str) -> i32 {
    if looks_like_full_type_name(type_ref) || google_well_known(type_ref).is_some() {
        3
    } else {
        1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Proto field number, read from the bundle's `no`.
    pub number: i32,
    pub name: String,
    pub kind: FieldKind,
    /// Named or scalar type, read from the bundle's `T`.
    pub type_token: Option<TypeToken>,
    pub oneof: Option<String>,
    pub repeated: bool,
    /// proto3 `optional` label, read from the bundle's `opt`.
    pub optional: bool,
    pub map_key: Option<i32>,
    pub map_value_kind: ValueKind,
    pub map_value_t: Option<TypeToken>,
}

impl Field {
    /// Builds a field with no type token, label or map part yet.
    #[must_use]
    pub fn new(number: i32, name: &str, kind: FieldKind) -> Self {
        Self {
            number,
            name: name.to_owned(),
            kind,
            type_token: None,
            oneof: None,
            repeated: false,
            optional: false,
            map_key: None,
            map_value_kind: ValueKind::Scalar,
            map_value_t: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub type_name: String,
    pub var_name: String,
    pub internal_name: String,
    pub fields: Vec<Field>,
    pub package: String,
    pub short_name: String,
    pub pos: u32,
    /// Byte offset of the enclosing webpack module; `None` at top level.
    pub module_start: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumValue {
    /// Proto enum number, read from the bundle's `no`.
    pub number: i32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enum {
    pub type_name: String,
    pub var_name: String,
    pub values: Vec<EnumValue>,
    pub package: String,
    pub short_name: String,
    pub pos: u32,
    /// Byte offset of the enclosing webpack module; `None` at top level.
    pub module_start: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub name: String,
    pub input_type: String,
    pub output_type: String,
    pub kind: MethodKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub type_name: String,
    pub var_name: String,
    pub methods: Vec<Method>,
    pub package: String,
    pub short_name: String,
    pub pos: u32,
    /// Byte offset of the enclosing webpack module; `None` at top level.
    pub module_start: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDef {
    pub type_name: String,
    pub pos: u32,
    pub kind: SymbolKind,
    /// Byte offset of the enclosing webpack module; `None` at top level.
    pub module_start: Option<u32>,
}

/// How many sample strings each diagnostic list keeps before it summarises the
/// remainder, so one pathological bundle cannot fill the report.
const DIAGNOSTIC_SAMPLE_LIMIT: usize = 20;
/// How many characters of a skipped field object the diagnostics quote.
const SAMPLE_PREVIEW_CHARS: usize = 140;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// Parse errors dropped after [`DIAGNOSTIC_SAMPLE_LIMIT`] were kept.
    pub overflowed_parse_errors: usize,
    pub shape_errors: Vec<String>,
}

impl Diagnostics {
    pub fn add_skipped_field(&mut self, field_object: &str, reason: &Error) {
        self.total_field_objects += 1;
        self.skipped_field_objects += 1;
        if self.skipped_field_samples.len() < DIAGNOSTIC_SAMPLE_LIMIT {
            let mut trimmed = field_object.trim().to_string();
            // Bundle snippets are arbitrary UTF-8, so cut on a char boundary.
            if trimmed.len() > SAMPLE_PREVIEW_CHARS {
                trimmed.truncate(trimmed.floor_char_boundary(SAMPLE_PREVIEW_CHARS));
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

    pub fn add_unresolved_type(&mut self, type_ref: &str) {
        let key = if type_ref.trim().is_empty() {
            "<empty>".to_string()
        } else {
            type_ref.trim().to_string()
        };
        *self.unresolved_type_refs.entry(key).or_insert(0) += 1;
    }

    pub fn add_parse_error(&mut self, reason: &str) {
        if self.parse_errors.len() < DIAGNOSTIC_SAMPLE_LIMIT {
            self.parse_errors.push(reason.to_string());
        } else {
            self.overflowed_parse_errors += 1;
        }
    }

    pub fn add_shape_error(&mut self, error: &Error) {
        self.shape_errors.push(error.to_string());
    }

    /// Folds `other` into `self`, keeping every counter and collection.
    pub fn merge_from(&mut self, mut other: Diagnostics) {
        self.total_field_objects += other.total_field_objects;
        self.parsed_field_objects += other.parsed_field_objects;
        self.skipped_field_objects += other.skipped_field_objects;
        self.skipped_field_samples
            .append(&mut other.skipped_field_samples);
        if self.skipped_field_samples.len() > DIAGNOSTIC_SAMPLE_LIMIT {
            self.skipped_field_samples.truncate(DIAGNOSTIC_SAMPLE_LIMIT);
        }
        for (key, count) in other.unresolved_type_refs {
            *self.unresolved_type_refs.entry(key).or_insert(0) += count;
        }
        self.empty_messages.append(&mut other.empty_messages);
        self.placeholder_hits.append(&mut other.placeholder_hits);
        self.missing_declarations
            .append(&mut other.missing_declarations);
        self.declared_types += other.declared_types;
        self.extracted_types += other.extracted_types;
        self.parse_errors.append(&mut other.parse_errors);
        self.overflowed_parse_errors += other.overflowed_parse_errors;
        if self.parse_errors.len() > DIAGNOSTIC_SAMPLE_LIMIT {
            let dropped = self.parse_errors.len() - DIAGNOSTIC_SAMPLE_LIMIT;
            self.parse_errors.truncate(DIAGNOSTIC_SAMPLE_LIMIT);
            self.overflowed_parse_errors += dropped;
        }
        self.shape_errors.append(&mut other.shape_errors);
    }

    #[must_use]
    pub fn has_failure(&self) -> bool {
        self.skipped_field_objects > 0
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
            if self.overflowed_parse_errors > 0 {
                writeln!(f, "  - ... and {} more", self.overflowed_parse_errors)?;
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
            let mut keys: Vec<_> = self.unresolved_type_refs.iter().collect();
            keys.sort_by_key(|(name, _)| name.as_str());
            for (key, count) in keys {
                writeln!(f, "  - {key} ({count})")?;
            }
        }
        if !self.placeholder_hits.is_empty() {
            writeln!(f, "占位字段命中:")?;
            for hit in self.placeholder_hits.iter().take(DIAGNOSTIC_SAMPLE_LIMIT) {
                writeln!(f, "  - {hit}")?;
            }
            if self.placeholder_hits.len() > DIAGNOSTIC_SAMPLE_LIMIT {
                writeln!(
                    f,
                    "  - ... and {} more",
                    self.placeholder_hits.len() - DIAGNOSTIC_SAMPLE_LIMIT
                )?;
            }
        }
        if !self.missing_declarations.is_empty() {
            writeln!(f, "未提取的 Proto 声明:")?;
            for name in self
                .missing_declarations
                .iter()
                .take(DIAGNOSTIC_SAMPLE_LIMIT)
            {
                writeln!(f, "  - {name}")?;
            }
            if self.missing_declarations.len() > DIAGNOSTIC_SAMPLE_LIMIT {
                writeln!(
                    f,
                    "  - ... and {} more",
                    self.missing_declarations.len() - DIAGNOSTIC_SAMPLE_LIMIT
                )?;
            }
        }
        Ok(())
    }
}

#[must_use]
pub fn looks_like_full_type_name(type_ref: &str) -> bool {
    let trimmed = type_ref.trim();
    if trimmed.starts_with("google.protobuf.") || trimmed.starts_with("google.rpc.") {
        return true;
    }
    looks_like_versioned_type(trimmed)
}

#[must_use]
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

#[must_use]
pub fn looks_like_package_name(value: &str) -> bool {
    let trimmed = value
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim_end_matches('.');
    let mut parts = trimmed.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty()
        || !first
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        || !is_ident(first)
    {
        return false;
    }
    let rest: Vec<_> = parts.collect();
    let Some(last) = rest.last() else {
        return false;
    };
    is_version_token(last) && rest.iter().all(|part| is_ident(part))
}

/// A type reference split into its package and its name inside that package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeName {
    /// Package part; empty when the reference is unqualified.
    pub package: String,
    /// Everything after the package (nested names keep their dots).
    pub short: String,
}

impl TypeName {
    #[must_use]
    pub fn new(package: &str, short: &str) -> Self {
        Self {
            package: package.to_owned(),
            short: short.to_owned(),
        }
    }
}

#[must_use]
pub fn parse_type_name(type_name: &str) -> TypeName {
    if let Some(rest) = type_name.strip_prefix("google.protobuf.") {
        return TypeName::new("google.protobuf", rest);
    }
    if let Some(rest) = type_name.strip_prefix("google.rpc.") {
        return TypeName::new("google.rpc", rest);
    }
    if let Some(TypeNamePart { package, short }) = split_versioned(type_name) {
        return TypeName::new(package, short);
    }
    match type_name.rsplit_once('.') {
        Some((package, short)) => TypeName::new(package, short),
        None => TypeName::new("", type_name),
    }
}

/// The package part of `type_name`, borrowed; empty when the name is
/// unqualified. Lets callers compare packages without allocating a [`TypeName`].
#[must_use]
pub fn package_of(type_name: &str) -> &str {
    if type_name.starts_with("google.protobuf.") {
        "google.protobuf"
    } else if type_name.starts_with("google.rpc.") {
        "google.rpc"
    } else if let Some(TypeNamePart { package, .. }) = split_versioned(type_name) {
        package
    } else {
        type_name
            .rsplit_once('.')
            .map_or("", |(package, _)| package)
    }
}

struct TypeNamePart<'a> {
    package: &'a str,
    short: &'a str,
}

fn split_versioned(type_name: &str) -> Option<TypeNamePart<'_>> {
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
                    return Some(TypeNamePart {
                        package: &type_name[..j],
                        short: &type_name[j + 1..],
                    });
                }
            }
        }
        i += 1;
    }
    None
}

fn is_version_token(part: &str) -> bool {
    let Some(rest) = part.strip_prefix('v') else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

#[must_use]
pub fn is_ident(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[must_use]
pub fn is_field_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
