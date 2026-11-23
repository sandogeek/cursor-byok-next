//! Resolve local symbols, aliases and short names to fully-qualified types.

use std::collections::HashMap;

use crate::ast_util::last_segment;
use crate::types::{
    Enum, Message, SymbolDef, google_well_known, looks_like_full_type_name, parse_type_name,
};

pub type AliasIndex = HashMap<u32, HashMap<String, Vec<String>>>;
pub type ModuleImports = HashMap<u32, HashMap<String, u32>>;

/// Where a type reference appears: the package it prefers, its byte offset and
/// the byte offset where its module starts.
#[derive(Clone, Copy, Debug)]
pub struct TypeContext<'a> {
    /// Package whose definitions win ties during resolution.
    pub package: &'a str,
    /// Byte offset of the referencing declaration.
    pub pos: u32,
    /// Byte offset where the referencing module starts, or `0` when unknown.
    pub module_start: u32,
}

impl<'a> TypeContext<'a> {
    /// Builds a context for the declaration that starts at `pos` in `package`.
    pub fn new(package: &'a str, pos: u32, module_start: u32) -> Self {
        Self {
            package,
            pos,
            module_start,
        }
    }
}

#[derive(Debug, Default)]
pub struct TypeResolver {
    by_symbol: HashMap<String, Vec<SymbolDef>>,
    by_alias: HashMap<String, Vec<SymbolDef>>,
    by_short: HashMap<String, Vec<SymbolDef>>,
    pub module_imports: ModuleImports,
}

impl TypeResolver {
    pub fn new(
        messages: &[Message],
        enums: &[Enum],
        aliases: &AliasIndex,
        export_aliases: &AliasIndex,
    ) -> Self {
        let mut resolver = Self::default();
        for message in messages {
            resolver.add(
                &message.var_name,
                &message.type_name,
                message.pos,
                message.module_start,
                "message",
            );
            if !message.internal_name.is_empty() && message.internal_name != message.var_name {
                resolver.add(
                    &message.internal_name,
                    &message.type_name,
                    message.pos,
                    message.module_start,
                    "message",
                );
            }
            for alias in aliases_for_symbols(
                aliases.get(&message.module_start),
                &[&message.var_name, &message.internal_name],
            ) {
                resolver.add_alias(
                    &alias,
                    &message.type_name,
                    message.pos,
                    message.module_start,
                    "message",
                );
            }
            for alias in aliases_for_symbols(
                export_aliases.get(&message.module_start),
                &[&message.var_name, &message.internal_name],
            ) {
                resolver.add_alias(
                    &alias,
                    &message.type_name,
                    message.pos,
                    message.module_start,
                    "message",
                );
            }
        }
        for item in enums {
            resolver.add(
                &item.var_name,
                &item.type_name,
                item.pos,
                item.module_start,
                "enum",
            );
            for alias in aliases_for_symbols(aliases.get(&item.module_start), &[&item.var_name]) {
                resolver.add_alias(&alias, &item.type_name, item.pos, item.module_start, "enum");
            }
            for alias in
                aliases_for_symbols(export_aliases.get(&item.module_start), &[&item.var_name])
            {
                resolver.add_alias(&alias, &item.type_name, item.pos, item.module_start, "enum");
            }
        }
        resolver
    }

    fn add(&mut self, symbol: &str, type_name: &str, pos: u32, module_start: u32, kind: &str) {
        let symbol = symbol.trim();
        let type_name = type_name.trim();
        if symbol.is_empty() || type_name.is_empty() {
            return;
        }
        let def = SymbolDef {
            type_name: type_name.to_string(),
            pos,
            kind: kind.to_string(),
            module_start,
        };
        self.by_symbol
            .entry(symbol.to_string())
            .or_default()
            .push(def.clone());
        let short_name = parse_type_name(type_name).short;
        if short_name.is_empty() {
            return;
        }
        self.by_short
            .entry(short_name.clone())
            .or_default()
            .push(def.clone());
        let underscore = short_name.replace('.', "_");
        if underscore != short_name {
            self.by_short
                .entry(underscore.clone())
                .or_default()
                .push(def.clone());
        }
        if let Some((_, last)) = short_name.rsplit_once('.') {
            self.by_short
                .entry(last.to_string())
                .or_default()
                .push(def.clone());
        }
        if let Some((_, last)) = underscore.rsplit_once('_') {
            self.by_short.entry(last.to_string()).or_default().push(def);
        }
    }

    fn add_alias(
        &mut self,
        symbol: &str,
        type_name: &str,
        pos: u32,
        module_start: u32,
        kind: &str,
    ) {
        let symbol = symbol.trim();
        let type_name = type_name.trim();
        if symbol.is_empty() || type_name.is_empty() {
            return;
        }
        self.by_alias
            .entry(symbol.to_string())
            .or_default()
            .push(SymbolDef {
                type_name: type_name.to_string(),
                pos,
                kind: kind.to_string(),
                module_start,
            });
    }

    /// Resolves `type_ref` from `context`, preferring definitions of `expected_kind`.
    ///
    /// Tries, in order: an exact symbol, a re-export alias, and a short name.
    /// Dotted references also fall back to their imported module, then to the
    /// well-known types.
    ///
    /// Returns `None` when nothing matches.
    pub fn resolve_type_name(
        &self,
        type_ref: &str,
        context: TypeContext<'_>,
        expected_kind: &str,
    ) -> Option<String> {
        let trimmed = type_ref.trim();
        if trimmed.is_empty() {
            return None;
        }
        if looks_like_full_type_name(trimmed) {
            return Some(trimmed.to_string());
        }
        if trimmed.contains('.')
            && let Some(type_name) = well_known_type_name(trimmed)
        {
            return Some(type_name);
        }
        let scope = if trimmed.contains('.') {
            ModuleScope::AnyModule
        } else {
            ModuleScope::SameModuleFirst
        };
        if let Some(name) = self.resolve_by_symbol(trimmed, context, expected_kind, scope) {
            return Some(name);
        }
        if let Some(name) = self.resolve_by_alias(trimmed, context, 0, expected_kind) {
            return Some(name);
        }
        if let Some(name) = self.resolve_by_short(trimmed, context, expected_kind, scope) {
            return Some(name);
        }
        if !trimmed.contains('.') {
            return well_known_type_name(trimmed);
        }
        let first = trimmed.split('.').next().unwrap_or("");
        let last = last_segment(trimmed);
        let target_module = self
            .module_imports
            .get(&context.module_start)
            .and_then(|imports| imports.get(first))
            .copied()
            .unwrap_or(0);
        if let Some(name) = self.resolve_by_alias(last, context, target_module, expected_kind) {
            return Some(name);
        }
        if let Some(name) =
            self.resolve_by_symbol(last, context, expected_kind, ModuleScope::AnyModule)
        {
            return Some(name);
        }
        if let Some(name) =
            self.resolve_by_short(last, context, expected_kind, ModuleScope::AnyModule)
        {
            return Some(name);
        }
        if let Some(name) =
            self.resolve_by_symbol(first, context, expected_kind, ModuleScope::AnyModule)
        {
            return Some(name);
        }
        well_known_type_name(last).or_else(|| well_known_type_name(trimmed))
    }
}

/// Which module's definitions a lookup prefers: the caller's module, or any.
#[derive(Clone, Copy)]
enum ModuleScope {
    /// Prefer a definition from the caller's own module.
    SameModuleFirst,
    /// Consider every module equally.
    AnyModule,
}

impl TypeResolver {
    fn resolve_by_symbol(
        &self,
        symbol: &str,
        context: TypeContext<'_>,
        expected_kind: &str,
        scope: ModuleScope,
    ) -> Option<String> {
        let candidates = self.by_symbol.get(symbol)?;
        let module_start = match scope {
            ModuleScope::SameModuleFirst => context.module_start,
            ModuleScope::AnyModule => 0,
        };
        pick_best_definition(candidates, context, module_start, expected_kind)
            .map(|definition| definition.type_name.clone())
    }

    fn resolve_by_alias(
        &self,
        symbol: &str,
        context: TypeContext<'_>,
        target_module_start: u32,
        expected_kind: &str,
    ) -> Option<String> {
        let candidates = self.by_alias.get(symbol)?;
        pick_best_definition(candidates, context, target_module_start, expected_kind)
            .map(|definition| definition.type_name.clone())
    }

    fn resolve_by_short(
        &self,
        symbol: &str,
        context: TypeContext<'_>,
        expected_kind: &str,
        scope: ModuleScope,
    ) -> Option<String> {
        let candidates = self.by_short.get(symbol)?;
        let module_start = match scope {
            ModuleScope::SameModuleFirst => context.module_start,
            ModuleScope::AnyModule => 0,
        };
        pick_best_definition(candidates, context, module_start, expected_kind)
            .map(|definition| definition.type_name.clone())
    }
}

/// Narrows `candidates` by kind, package and module, then picks the definition
/// closest to `context.pos`, preferring definitions that start after it.
fn pick_best_definition<'a>(
    candidates: &'a [SymbolDef],
    context: TypeContext<'_>,
    module_start: u32,
    expected_kind: &str,
) -> Option<&'a SymbolDef> {
    let mut filtered: Vec<&SymbolDef> = candidates.iter().collect();
    let expected_kind = expected_kind.trim();
    if !expected_kind.is_empty() {
        narrow(&mut filtered, |item| item.kind == expected_kind);
    }
    let preferred_package = context.package.trim();
    if !preferred_package.is_empty() {
        narrow(&mut filtered, |item| {
            parse_type_name(&item.type_name).package == preferred_package
        });
    }
    if module_start > 0 {
        narrow(&mut filtered, |item| item.module_start == module_start);
    }
    filtered.into_iter().min_by(|a, b| {
        let distance_a = a.pos.abs_diff(context.pos);
        let distance_b = b.pos.abs_diff(context.pos);
        distance_a
            .cmp(&distance_b)
            .then_with(|| (a.pos > context.pos).cmp(&(b.pos > context.pos)))
    })
}

/// Drops non-matching items unless nothing matches, keeping the wider set as a
/// fallback instead of widening the caller's ranking criteria.
fn narrow(filtered: &mut Vec<&SymbolDef>, keep: impl Fn(&&SymbolDef) -> bool + Copy) {
    if filtered.iter().any(keep) {
        filtered.retain(keep);
    }
}

pub fn well_known_type_name(r#ref: &str) -> Option<String> {
    let last = last_segment(r#ref.trim());
    google_well_known(last).map(str::to_string)
}

pub fn fallback_type_token(r#ref: &str) -> String {
    let token = r#ref.trim();
    if token.is_empty() {
        return token.to_string();
    }
    last_segment(token).to_string()
}

pub fn build_alias_index(aliases: &[(u32, String, String)]) -> AliasIndex {
    let mut direct: HashMap<u32, HashMap<String, String>> = HashMap::new();
    for (module_start, alias, target) in aliases {
        if alias.is_empty() || target.is_empty() || alias == target {
            continue;
        }
        direct
            .entry(*module_start)
            .or_default()
            .insert(alias.clone(), target.clone());
    }
    let mut index = AliasIndex::new();
    for (module_start, mapping) in direct {
        let mut roots: HashMap<String, Vec<String>> = HashMap::new();
        for alias in mapping.keys() {
            let root = resolve_root(&mapping, alias);
            if root != *alias {
                roots.entry(root).or_default().push(alias.clone());
            }
        }
        for aliases in roots.values_mut() {
            aliases.sort();
        }
        if !roots.is_empty() {
            index.insert(module_start, roots);
        }
    }
    index
}

fn resolve_root(direct: &HashMap<String, String>, symbol: &str) -> String {
    let mut seen = Vec::new();
    let mut current = symbol.to_string();
    loop {
        if seen.iter().any(|item| item == &current) {
            return symbol.to_string();
        }
        seen.push(current.clone());
        match direct.get(&current) {
            Some(next) => current.clone_from(next),
            None => return current,
        }
    }
}

fn aliases_for_symbols(
    aliases: Option<&HashMap<String, Vec<String>>>,
    symbols: &[&str],
) -> Vec<String> {
    let Some(aliases) = aliases else {
        return Vec::new();
    };
    let mut seen = Vec::new();
    let mut result = Vec::new();
    for symbol in symbols {
        if let Some(items) = aliases.get(*symbol) {
            for alias in items {
                if alias.is_empty() || seen.iter().any(|item| item == alias) {
                    continue;
                }
                seen.push(alias.clone());
                result.push(alias.clone());
            }
        }
    }
    result.sort();
    result
}

pub fn build_module_imports(
    entries: &[(u32, String, u32)],
    module_starts: &[(u32, u32)],
) -> ModuleImports {
    let id_to_start: HashMap<u32, u32> = module_starts
        .iter()
        .copied()
        .map(|(start, id)| (id, start))
        .collect();
    let mut imports = ModuleImports::new();
    for (module_start, alias, target) in entries {
        if alias.is_empty() {
            continue;
        }
        let target_start = id_to_start.get(target).copied().unwrap_or(*target);
        imports
            .entry(*module_start)
            .or_default()
            .insert(alias.clone(), target_start);
    }
    imports
}
