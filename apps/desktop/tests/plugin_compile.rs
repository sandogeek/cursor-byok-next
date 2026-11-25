//! Probes the same fail-closed TS compiler that build.rs uses.
#[expect(
    dead_code,
    clippy::print_stdout,
    reason = "integration probes invoke compile, not build-script filesystem orchestration"
)]
#[path = "../plugin_compile.rs"]
mod compiler;

#[test]
fn erases_types_preserving_modern_syntax_and_relative_imports() {
    let source = r"
import type { Thing } from './types.ts';
import { helper } from './helper.ts';
export { other } from './other.ts';
export * from './all.ts';
export enum Kind { First = 2, Second }
export class Counter { #value: number = 0; value(): number { return this.#value; } }
export const text: string = './literal.ts';
export const useHelper = helper;
export const module = await import('./dynamic.ts');
";
    let js = compiler::compile(std::path::Path::new("probe.ts"), source).unwrap();
    assert!(js.contains("./helper.js"));
    assert!(js.contains("./other.js"));
    assert!(js.contains("./all.js"));
    assert!(js.contains("./dynamic.js"));
    assert!(
        js.contains("./literal.ts"),
        "ordinary string data must not be rewritten"
    );
    assert!(js.contains("#value"));
    assert!(!js.contains("import type"));
    assert!(!js.contains(": number"));
    assert!(!js.contains("require("));
    assert!(!js.contains("@oxc"));
}
#[test]
fn malformed_source_is_not_embedded() {
    assert!(compiler::compile(std::path::Path::new("bad.ts"), "export const x: = ;").is_err());
}
#[test]
fn type_checks_are_separate_from_syntax_erasure() {
    // This is deliberately not a claim that oxc performs the full tsc type checker.
    assert!(
        compiler::compile(
            std::path::Path::new("typed.ts"),
            "export const x: number = 'not a number';"
        )
        .is_ok()
    );
}

#[test]
fn lockfile_has_one_pinned_quickjs_sys() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock = std::fs::read_to_string(root).unwrap();
    let marker = "name = \"quickjs-jit-sys\"";
    assert_eq!(lock.matches(marker).count(), 1);
    assert!(
        lock.split_once(marker)
            .unwrap()
            .1
            .starts_with("\nversion = \"0.14.2\"")
    );
}
