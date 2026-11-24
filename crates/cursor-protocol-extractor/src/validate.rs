//! Local-loop Agent / Inference wire-shape checks.
//!
//! Two different questions live here:
//!
//! - does a fragment that *is* present still have the field shape the local loop
//!   decodes (`proto_block` + field checks)?
//! - does the finished package contain every declaration the local loop needs at
//!   all (the per-package requirements below)?
//!
//! Both are answered for a rendered file, so a bundle without any protobuf
//! declarations cannot pass as a complete extraction.

use regex::Regex;
use std::sync::OnceLock;

use crate::error::Error;

/// Checks one rendered local-loop proto file.
///
/// A package is complete only when every declaration the local loop decodes is
/// present *and* has the expected field shape; a missing declaration is a
/// failure, not a skipped check.
///
/// # Errors
///
/// Returns [`Error::Shape`] naming the file and the declaration that is missing
/// or drifted.
pub fn validate_required_shapes(file: &str, package: &str, body: &str) -> Result<(), Error> {
    match package {
        "agent.v1" => validate_agent_shapes(file, body),
        "aiserver.v1" => validate_inference_shapes(file, body),
        // The extractor only renders `LOCAL_LOOP_PACKAGES`; anything else has no
        // required shape.
        _ => Ok(()),
    }
}

/// `agent.v1` control messages the local loop decodes while streaming a tool.
fn validate_agent_shapes(file: &str, body: &str) -> Result<(), Error> {
    let block = require_message(file, body, "ExecClientControlMessage")?;
    require_field(
        file,
        block,
        "ExecClientStreamClose stream_close = 1;",
        "ExecClientControlMessage.stream_close must be ExecClientStreamClose",
    )?;
    let block = require_message(file, body, "ShellStream")?;
    require_field(
        file,
        block,
        "ShellStreamStdout stdout = 1;",
        "ShellStream.stdout must be ShellStreamStdout",
    )
}

/// `aiserver.v1` inference shapes the local loop reads.
fn validate_inference_shapes(file: &str, body: &str) -> Result<(), Error> {
    let block = require_message(file, body, "RunInferencePromptModelMetadata")?;
    require_field(
        file,
        block,
        "repeated RunInferenceAgentModeConfig agent_mode_configs = 46;",
        "RunInferencePromptModelMetadata.agent_mode_configs must be repeated RunInferenceAgentModeConfig = 46",
    )?;
    require_field(
        file,
        block,
        "optional RunInferenceAskQuestionConfig ask_question_config = 48;",
        "RunInferencePromptModelMetadata.ask_question_config must be optional RunInferenceAskQuestionConfig = 48",
    )?;
    let block = require_message(file, body, "RunInferenceAgentModeConfig")?;
    for field in [
        "string mode_id = 1;",
        "bool enable_ask_question_tool = 2;",
        "optional RunInferenceSwitchModeToolConfig switch_mode_tool_config = 3;",
    ] {
        require_field(
            file,
            block,
            field,
            "RunInferenceAgentModeConfig must include mode_id, enable_ask_question_tool, and switch_mode_tool_config",
        )?;
    }
    let block = proto_block(body, "service", "InferenceService")
        .ok_or_else(|| Error::Shape(format!("{file}: missing service InferenceService")))?;
    if !run_inference().is_match(block) {
        return Err(Error::Shape(format!(
            "{file}: InferenceService must expose bidi RunInference"
        )));
    }
    if !run_inference_sse().is_match(block) {
        return Err(Error::Shape(format!(
            "{file}: InferenceService must expose RunInferenceSSE"
        )));
    }
    Ok(())
}

/// The braced body of `message name`, or a shape error when it is absent.
fn require_message<'a>(file: &str, body: &'a str, name: &str) -> Result<&'a str, Error> {
    proto_block(body, "message", name)
        .ok_or_else(|| Error::Shape(format!("{file}: missing message {name}")))
}

/// Fails unless `block` declares `field` verbatim.
fn require_field(file: &str, block: &str, field: &str, message: &str) -> Result<(), Error> {
    if block.contains(field) {
        Ok(())
    } else {
        Err(Error::Shape(format!("{file}: {message}")))
    }
}

/// Whether a rendered proto line is a placeholder the extractor could not fill.
#[must_use]
pub fn has_placeholder(line: &str) -> bool {
    placeholder().is_match(line)
}

/// Returns the braced body of `kind name` in a rendered proto file.
#[must_use]
pub fn proto_block<'a>(body: &'a str, kind: &str, name: &str) -> Option<&'a str> {
    let needle = format!("{kind} {name} ");
    let start = body
        .find(&needle)
        .or_else(|| body.find(&format!("{kind} {name}{{")))?;
    let brace = body[start..].find('{')? + start;
    let end = matching_brace(body, brace)?;
    Some(&body[brace..=end])
}

fn matching_brace(body: &str, open: usize) -> Option<usize> {
    let bytes = body.as_bytes();
    if open >= bytes.len() || bytes[open] != b'{' {
        return None;
    }
    let mut depth = 0i32;
    for (offset, ch) in bytes[open..].iter().enumerate() {
        match ch {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn run_inference() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"rpc\s+RunInference\s*\(\s*stream\s+RunInferenceClientMessage\s*\)\s+returns\s+\(\s*stream\s+RunInferenceServerMessage\s*\)")
            .expect("the RunInference pattern is a compile-time constant")
    })
}

fn run_inference_sse() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"rpc\s+RunInferenceSSE\s*\(\s*BidiRequestId\s*\)\s+returns\s+\(\s*stream\s+RunInferenceServerMessage\s*\)")
            .expect("the RunInferenceSSE pattern is a compile-time constant")
    })
}

fn placeholder() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(optional\s+|repeated\s+)?[A-Za-z_][A-Za-z0-9_.<>]*\s+(field_\d+|unknown)\s*=\s*\d+\s*;")
            .expect("the placeholder pattern is a compile-time constant")
    })
}
