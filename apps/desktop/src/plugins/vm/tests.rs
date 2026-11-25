//! Behavioral probes against the exact engine used by the plugin runtime.
use super::{loader::Modules, runtime};
use std::sync::{Arc, atomic::AtomicBool, mpsc};

fn probe(source: &'static str) -> (crate::Result<()>, Vec<serde_json::Value>) {
    let modules = Modules(Box::leak(
        vec![("sdk/collect.js", source)].into_boxed_slice(),
    ));
    let (_sender, receiver) = mpsc::channel();
    let (output, mut frames) = tokio::sync::mpsc::channel(4096);
    let result = runtime::run(
        modules,
        "probe/main.js",
        "sdk/collect.js",
        &receiver,
        &output,
        &Arc::new(AtomicBool::new(false)),
        None,
    );
    let values = std::iter::from_fn(|| frames.try_recv().ok())
        .map(|frame| serde_json::from_slice(&frame).unwrap())
        .collect();
    (result, values)
}
#[test]
fn engine_intrinsics_and_capability_absence() {
    let (result, values) = probe(
        r"__send(JSON.stringify({types: [typeof fetch, typeof process, typeof require, typeof WebSocket, typeof structuredClone], intrinsic: [typeof JSON,typeof Promise,typeof queueMicrotask,typeof WeakRef,typeof atob,typeof btoa,typeof Symbol.asyncIterator], meta: typeof import.meta.url, base64: atob(btoa('hello'))}));",
    );
    result.unwrap();
    assert_eq!(
        values[0]["types"],
        serde_json::json!([
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined"
        ])
    );
    assert_eq!(
        values[0]["intrinsic"],
        serde_json::json!([
            "object", "function", "function", "function", "function", "function", "symbol"
        ])
    );
    assert_eq!(values[0]["meta"], "undefined");
    assert_eq!(values[0]["base64"], "hello");
}
#[test]
fn url_abort_and_timers_have_the_required_semantics() {
    let (result, values) = probe(
        r"
const url = new URL('https://alice:pass@EXAMPLE.com:443/a?q=b');
const controller = new AbortController(); let aborted = 0;
controller.signal.addEventListener('abort', () => aborted++);
controller.abort(); controller.abort();
let ticks = 0; const interval = setInterval(() => ticks++, 1);
await new Promise(resolve => setTimeout(resolve, 8)); clearInterval(interval);
__send(JSON.stringify({url:[url.protocol,url.username,url.password,url.hostname,url.origin], aborted, flag:controller.signal.aborted, ticks: ticks > 0}));
",
    );
    result.unwrap();
    assert_eq!(
        values[0]["url"],
        serde_json::json!([
            "https:",
            "alice",
            "pass",
            "example.com",
            "https://example.com"
        ])
    );
    assert_eq!(values[0]["aborted"], 1);
    assert_eq!(values[0]["flag"], true);
    assert_eq!(values[0]["ticks"], true);
}
#[test]
fn loader_is_fail_closed_for_static_and_dynamic_imports() {
    let modules = Modules::embedded();
    for denied in [
        "fs",
        "node:fs",
        "node:process",
        "/etc/passwd",
        "../../sdk/plugin.js",
        "https://example.com/x.js",
        "unknown",
    ] {
        assert!(
            modules.resolve("magpie/main.js", denied).is_none(),
            "{denied}"
        );
    }
    assert_eq!(
        modules.resolve("magpie/main.js", "./provider.js").unwrap(),
        "magpie/provider.js"
    );
    for source in [
        "import 'node:fs';",
        "await import('fs');",
        "throw new Error('private response text');",
        "await Promise.reject(new Error('private response text'));",
    ] {
        let (result, _) = probe(source);
        let error = result.unwrap_err().to_string();
        assert!(!error.contains("private response text"));
    }
}
#[test]
fn vm_namespaces_are_independent() {
    for _ in 0..2 {
        let (result, values) =
            probe("globalThis.count = (globalThis.count ?? 0) + 1; __send(JSON.stringify(count));");
        result.unwrap();
        assert_eq!(values[0], 1);
    }
}
#[test]
fn cpu_interrupt_is_uncatchable_and_a_new_vm_still_works() {
    let (result, values) = probe("try { while (true) {} } catch { __send('true'); }");
    assert!(result.is_err());
    assert_eq!(values, Vec::<serde_json::Value>::new());
    let (result, values) = probe("__send('true');");
    result.unwrap();
    assert_eq!(values, vec![serde_json::json!(true)]);
}
#[test]
fn recursive_microtasks_share_one_cpu_budget() {
    let (result, _) =
        probe("function spin() { queueMicrotask(spin); } spin(); await new Promise(() => {});");
    assert!(result.is_err());
}
#[test]
fn heap_exhaustion_discards_vm_without_crashing_the_host() {
    let (result, values) = probe(
        "const held=[]; try { for (;;) held.push(new Array(100000).fill(42)); } catch { held.length=0; __send('true'); }",
    );
    assert!(result.is_err());
    assert_eq!(values, Vec::<serde_json::Value>::new());
    probe("__send('true');").0.unwrap();
}
#[test]
fn oversized_output_is_a_fatal_session_error() {
    let (result, _) = probe("__send('x'.repeat(16 * 1024 * 1024 + 1));");
    assert!(result.is_err());
}
