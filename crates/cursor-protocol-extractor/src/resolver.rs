//! Resolve local symbols, aliases and short names to fully-qualified types.

use std::collections::HashMap;

use crate::ast_util::last_segment;
use crate::types::{
    google_well_known, looks_like_full_type_name, parse_type_name, Enum, Message, SymbolDef,
};

pub type AliasIndex = HashMap<u32, HashMap<String, Vec<String>>>;
pub type ModuleImports = HashMap<u32, HashMap<String, u32>>;

#[derive(Default)]
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
                resolver.add_alias(
                    &alias,
                    &item.type_name,
                    item.pos,
                    item.module_start,
                    "enum",
                );
            }
            for alias in
                aliases_for_symbols(export_aliases.get(&item.module_start), &[&item.var_name])
            {
                resolver.add_alias(
                    &alias,
                    &item.type_name,
                    item.pos,
                    item.module_start,
                    "enum",
                );
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
        let (_, short_name) = parse_type_name(type_name);
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

    fn add_alias(&mut self, symbol: &str, type_name: &str, pos: u32, module_start: u32, kind: &str) {
        let symbol = symbol.trim();
        let type_name = type_name.trim();
        if symbol.is_empty() || type_name.is_empty() {
            return;
        }
        self.by_alias.entry(symbol.to_string()).or_default().push(SymbolDef {
            type_name: type_name.to_string(),
            pos,
            kind: kind.to_string(),
            module_start,
        });
    }

    pub fn resolve_type_name(
        &self,
        r#ref: &str,
        context_pos: u32,
        context_module_start: u32,
        preferred_pkg: &str,
        expected_kind: &str,
    ) -> Option<String> {
        let trimmed = r#ref.trim();
        if trimmed.is_empty() {
            return None;
        }
        if looks_like_full_type_name(trimmed) {
            return Some(trimmed.to_string());
        }
        if trimmed.contains('.') {
            if let Some(type_name) = well_known_type_name(trimmed) {
                return Some(type_name);
            }
        }
        let prefer_same = !trimmed.contains('.');
        if let Some(name) =
            self.resolve_by_symbol(trimmed, context_pos, context_module_start, preferred_pkg, expected_kind, prefer_same)
        {
            return Some(name);
        }
        if let Some(name) =
            self.resolve_by_alias(trimmed, context_pos, 0, preferred_pkg, expected_kind)
        {
            return Some(name);
        }
        if let Some(name) =
            self.resolve_by_short(trimmed, context_pos, context_module_start, preferred_pkg, expected_kind, prefer_same)
        {
            return Some(name);
        }
        if trimmed.contains('.') {
            let first = trimmed.split('.').next().unwrap_or("");
            let last = last_segment(trimmed);
            let target_module = self
                .module_imports
                .get(&context_module_start)
                .and_then(|imports| imports.get(first))
                .copied()
                .unwrap_or(0);
            if let Some(name) =
                self.resolve_by_alias(last, context_pos, target_module, preferred_pkg, expected_kind)
            {
                return Some(name);
            }
            if let Some(name) =
                self.resolve_by_symbol(last, context_pos, context_module_start, preferred_pkg, expected_kind, false)
            {
                return Some(name);
            }
            if let Some(name) =
                self.resolve_by_short(last, context_pos, context_module_start, preferred_pkg, expected_kind, false)
            {
                return Some(name);
            }
            if let Some(name) =
                self.resolve_by_symbol(first, context_pos, context_module_start, preferred_pkg, expected_kind, false)
            {
                return Some(name);
            }
            if let Some(name) = well_known_type_name(last) {
                return Some(name);
            }
        }
        well_known_type_name(trimmed)
    }
}

impl TypeResolver {
    fn resolve_by_symbol(
        &self,
        symbol: &str,
        context_pos: u32,
        context_module_start: u32,
        preferred_pkg: &str,
        expected_kind: &str,
        prefer_same_module: bool,
    ) -> Option<String> {
        let candidates = self.by_symbol.get(symbol)?;
        let module_start = if prefer_same_module {
            context_module_start
        } else {
            0
        };
        pick_best_definition(candidates, context_pos, module_start, preferred_pkg, expected_kind)
            .map(|def| def.type_name.clone())
    }

    fn resolve_by_alias(
        &self,
        symbol: &str,
        context_pos: u32,
        target_module_start: u32,
        preferred_pkg: &str,
        expected_kind: &str,
    ) -> Option<String> {
        let candidates = self.by_alias.get(symbol)?;
        pick_best_definition(
            candidates,
            context_pos,
            target_module_start,
            preferred_pkg,
            expected_kind,
        )
        .map(|def| def.type_name.clone())
    }

    fn resolve_by_short(
        &self,
        symbol: &str,
        context_pos: u32,
        context_module_start: u32,
        preferred_pkg: &str,
        expected_kind: &str,
        prefer_same_module: bool,
    ) -> Option<String> {
        let candidates = self.by_short.get(symbol)?;
        let module_start = if prefer_same_module {
            context_module_start
        } else {
            0
        };
        pick_best_definition(candidates, context_pos, module_start, preferred_pkg, expected_kind)
            .map(|def| def.type_name.clone())
    }
}

pub fn pick_best_definition<'a>(
    candidates: &'a [SymbolDef],
    context_pos: u32,
    context_module_start: u32,
    preferred_pkg: &str,
    expected_kind: &str,
) -> Option<&'a SymbolDef> {
    if candidates.is_empty() {
        return None;
    }
    let mut filtered: Vec<&SymbolDef> = candidates.iter().collect();
    if !expected_kind.trim().is_empty() {
        let kind_matched: Vec<_> = filtered
            .iter()
            .copied()
            .filter(|item| item.kind == expected_kind)
            .collect();
        if !kind_matched.is_empty() {
            filtered = kind_matched;
        }
    }
    if !preferred_pkg.trim().is_empty() {
        let pkg_matched: Vec<_> = filtered
            .iter()
            .copied()
            .filter(|item| parse_type_name(&item.type_name).0 == preferred_pkg)
            .collect();
        if !pkg_matched.is_empty() {
            filtered = pkg_matched;
        }
    }
    if context_module_start > 0 {
        let module_matched: Vec<_> = filtered
            .iter()
            .copied()
            .filter(|item| item.module_start == context_module_start)
            .collect();
        if !module_matched.is_empty() {
            filtered = module_matched;
        }
    }
    filtered.into_iter().min_by(|a, b| {
        let da = a.pos.abs_diff(context_pos);
        let db = b.pos.abs_diff(context_pos);
        da.cmp(&db).then_with(|| {
            let a_future = a.pos > context_pos;
            let b_future = b.pos > context_pos;
            a_future.cmp(&b_future)
        })
    })
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
            Some(next) => current = next.clone(),
            None => return current,
        }
    }
}

fn aliases_for_symbols(aliases: Option<&HashMap<String, Vec<String>>>, symbols: &[&str]) -> Vec<String> {
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

pub fn build_module_imports(entries: &[(u32, String, u32)], module_starts: &[(u32, u32)]) -> ModuleImports {
    let id_to_start: HashMap<u32, u32> = module_starts.iter().copied().map(|(start, id)| (id, start)).collect();
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
