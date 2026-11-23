//! Generates Cursor protobuf bindings used by the local loop.
use std::{env, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let proto_dir = manifest.join("../../protocols/cursor");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let generated = out_dir.join("protos");
    std::fs::create_dir_all(&generated).expect("create sanitized proto directory");

    let agent_src = proto_dir.join("agent_v1.proto");
    let run_inference_src = proto_dir.join("run_inference.proto");
    let aiserver_src = proto_dir.join("aiserver_v1.proto");
    println!("cargo:rerun-if-changed={}", agent_src.display());
    println!("cargo:rerun-if-changed={}", run_inference_src.display());
    println!("cargo:rerun-if-changed={}", aiserver_src.display());

    let agent = generated.join("agent_v1.proto");
    let run_inference = generated.join("run_inference.proto");
    std::fs::write(
        &agent,
        sanitize(&std::fs::read_to_string(&agent_src).expect("read agent_v1.proto")),
    )
    .expect("write sanitized agent_v1.proto");
    std::fs::write(
        &run_inference,
        sanitize(&std::fs::read_to_string(&run_inference_src).expect("read run_inference.proto")),
    )
    .expect("write sanitized run_inference.proto");

    // SAFETY: `build.rs` runs single-threaded before compilation starts, so no
    // other thread can read the environment while it is mutated here.
    unsafe {
        env::set_var(
            "PROTOC",
            protoc_bin_vendored::protoc_bin_path().expect("vendored protoc"),
        );
    }

    prost_build::Config::new()
        .compile_protos(
            &[agent, run_inference],
            &[
                generated,
                protoc_bin_vendored::include_path().expect("vendored protobuf includes"),
            ],
        )
        .expect("compile Cursor protobuf schema");

    let aiserver_source = std::fs::read_to_string(&aiserver_src).expect("read aiserver_v1.proto");
    for required in [
        "message BidiAppendRequest",
        "string data = 1;",
        "BidiRequestId request_id = 2;",
        "int64 append_seqno = 3;",
        "bytes data_binary = 4;",
        "message BidiAppendResponse",
        "message CustomErrorDetails",
        "optional bool is_retryable = 4;",
        "optional bool show_request_id = 5;",
        "optional bool should_show_immediate_error = 6;",
        "message ErrorDetails",
        "ERROR_PROVIDER_ERROR = 57;",
        "CustomErrorDetails details = 2;",
        "optional bool is_expected = 3;",
    ] {
        assert!(
            aiserver_source.contains(required),
            "aiserver Bidi wire schema changed: missing {required}"
        );
    }
}

fn sanitize(source: &str) -> String {
    rewrite_nested_type_refs(&join_backslash_continuations(source))
}

fn join_backslash_continuations(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut pending = false;
    for line in source.lines() {
        if pending {
            out.push_str(line.trim_start());
            pending = false;
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            if let Some(stripped) = line.strip_suffix('\\') {
                out.push_str(stripped);
                pending = true;
            } else {
                out.push_str(line);
            }
        }
    }
    if source.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn rewrite_nested_type_refs(source: &str) -> String {
    let mut replacements: Vec<(String, String)> = collect_nested_type_paths(source)
        .into_iter()
        .filter(|path| path.len() > 1)
        .map(|path| (path.join("_"), path.join(".")))
        .collect();
    replacements.sort_by_key(|left| std::cmp::Reverse(left.0.len()));
    replacements.dedup();
    let mut out = source.to_string();
    for (from, to) in replacements {
        out = replace_ident(&out, &from, &to);
    }
    out
}

fn collect_nested_type_paths(source: &str) -> Vec<Vec<String>> {
    let mut depth = 0_i32;
    let mut stack: Vec<(i32, String)> = Vec::new();
    let mut nested = Vec::new();
    for line in source.lines() {
        if let Some(name) =
            declaration_name(line, "message").or_else(|| declaration_name(line, "enum"))
        {
            stack.push((depth, name));
            let path: Vec<String> = stack.iter().map(|(_, name)| name.clone()).collect();
            if path.len() > 1 {
                nested.push(path);
            }
        } else if declaration_name(line, "service").is_some() {
            stack.push((depth, String::new()));
        }
        depth += brace_delta(line);
        while stack.last().is_some_and(|(open, _)| *open >= depth) {
            stack.pop();
        }
    }
    nested
}

fn declaration_name(line: &str, keyword: &str) -> Option<String> {
    let trimmed = strip_line_comment(line).trim();
    let rest = trimmed.strip_prefix(keyword)?.trim_start();
    let name: String = rest
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

fn strip_line_comment(line: &str) -> &str {
    line.split("//").next().unwrap_or(line)
}

fn brace_delta(line: &str) -> i32 {
    let text = strip_line_comment(line);
    let opens =
        i32::try_from(text.bytes().filter(|byte| *byte == b'{').count()).unwrap_or(i32::MAX);
    let closes =
        i32::try_from(text.bytes().filter(|byte| *byte == b'}').count()).unwrap_or(i32::MAX);
    opens - closes
}

fn replace_ident(haystack: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0;
    for (idx, _) in haystack.match_indices(from) {
        if idx < last {
            continue;
        }
        let before_ok =
            idx == 0 || !is_ident_char(haystack[..idx].chars().next_back().unwrap_or('\0'));
        let after_idx = idx + from.len();
        let after_ok = after_idx == haystack.len()
            || !is_ident_char(haystack[after_idx..].chars().next().unwrap_or('\0'));
        if before_ok && after_ok {
            out.push_str(&haystack[last..idx]);
            out.push_str(to);
            last = after_idx;
        }
    }
    out.push_str(&haystack[last..]);
    out
}

fn is_ident_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}
