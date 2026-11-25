//! Build-time TypeScript erasure for the embedded plugin module table.
use oxc::{
    allocator::Allocator,
    ast::ast::{
        ExportAllDeclaration, ExportFromDeclaration, Expression, ImportDeclaration,
        ImportExpression, StringLiteral,
    },
    ast_visit::{VisitMut, walk_mut},
    codegen::{Codegen, CodegenOptions},
    parser::Parser,
    semantic::SemanticBuilder,
    span::SourceType,
    transformer::{TransformOptions, Transformer},
};
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

pub fn compile(path: &Path, source: &str) -> Result<String, String> {
    let arena = Allocator::default();
    let parsed = Parser::new(&arena, source, SourceType::ts()).parse();
    if !parsed.diagnostics.is_empty() {
        return Err(format!(
            "parse diagnostics in {}: {:?}",
            path.display(),
            parsed.diagnostics
        ));
    }
    let mut program = parsed.program;
    let semantic = SemanticBuilder::new().with_enum_eval(true).build(&program);
    if !semantic.diagnostics.is_empty() {
        return Err(format!(
            "semantic diagnostics in {}: {:?}",
            path.display(),
            semantic.diagnostics
        ));
    }
    let options = TransformOptions::default();
    let transformed = Transformer::new(&arena, path, &options)
        .build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if !transformed.diagnostics.is_empty() {
        return Err(format!(
            "transform diagnostics in {}: {:?}",
            path.display(),
            transformed.diagnostics
        ));
    }
    Imports(&arena).visit_program(&mut program);
    Ok(Codegen::new()
        .with_options(CodegenOptions {
            comments: oxc::codegen::CommentOptions::disabled(),
            ..CodegenOptions::default()
        })
        .build(&program)
        .code)
}
struct Imports<'a>(&'a Allocator);
impl<'a> Imports<'a> {
    fn rewrite(&self, source: &mut StringLiteral<'a>) {
        let Some(name) = source.value.as_str() else {
            return;
        };
        if name.starts_with('.')
            && let Some(stem) = name.strip_suffix(".ts")
        {
            source.value = self.0.alloc_str(&format!("{stem}.js")).into();
            source.raw = None;
        }
    }
}
impl<'a> VisitMut<'a> for Imports<'a> {
    fn visit_import_declaration(&mut self, node: &mut ImportDeclaration<'a>) {
        self.rewrite(&mut node.source);
        walk_mut::walk_import_declaration(self, node);
    }
    fn visit_export_from_declaration(&mut self, node: &mut ExportFromDeclaration<'a>) {
        self.rewrite(&mut node.source);
        walk_mut::walk_export_from_declaration(self, node);
    }
    fn visit_export_all_declaration(&mut self, node: &mut ExportAllDeclaration<'a>) {
        self.rewrite(&mut node.source);
        walk_mut::walk_export_all_declaration(self, node);
    }
    fn visit_import_expression(&mut self, node: &mut ImportExpression<'a>) {
        if let Expression::StringLiteral(source) = &mut node.source {
            self.rewrite(source);
        }
        walk_mut::walk_import_expression(self, node);
    }
}
fn files(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    println!("cargo:rerun-if-changed={}", root.display());
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "ts") {
            out.push(path);
        }
    }
    Ok(())
}
pub fn build() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let mut table = String::from("pub(super) const MODULES: &[(&str, &str)] = &[\n");
    for (directory, prefix) in [
        ("plugins/build-in/magpie", "magpie"),
        ("src/plugins/sdk", "sdk"),
    ] {
        let source = root.join(directory);
        let mut inputs = Vec::new();
        files(&source, &mut inputs)?;
        inputs.sort();
        for input in inputs {
            let id = format!(
                "{prefix}/{}",
                input
                    .strip_prefix(&source)?
                    .with_extension("js")
                    .to_string_lossy()
                    .replace('\\', "/")
            );
            let target = out.join("plugins").join(&id);
            fs::create_dir_all(target.parent().ok_or("missing output parent")?)?;
            fs::write(&target, compile(&input, &fs::read_to_string(&input)?)?)?;
            let target = target.to_string_lossy();
            writeln!(table, "({id:?}, include_str!({target:?})),")?;
        }
    }
    table.push_str("];\n");
    fs::write(out.join("plugin_modules.rs"), table)?;
    Ok(())
}
