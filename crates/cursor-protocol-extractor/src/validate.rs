//! Local-loop Agent / Inference wire-shape checks.

use regex::Regex;
use std::sync::OnceLock;

pub fn validate_required_agent_shapes(file: &str, body: &str) -> Result<(), String> {
    if let Some(block) = proto_block(body, "message", "ExecClientControlMessage") {
        if !block.contains("ExecClientStreamClose stream_close = 1;") {
            return Err(format!(
                "{file}: ExecClientControlMessage.stream_close must be ExecClientStreamClose"
            ));
        }
    }
    if let Some(block) = proto_block(body, "message", "ShellStream") {
        if !block.contains("ShellStreamStdout stdout = 1;") {
            return Err(format!(
                "{file}: ShellStream.stdout must be ShellStreamStdout"
            ));
        }
    }
    if let Some(block) = proto_block(body, "message", "RunInferencePromptModelMetadata") {
        if !block.contains("repeated RunInferenceAgentModeConfig agent_mode_configs = 46;") {
            return Err(format!(
                "{file}: RunInferencePromptModelMetadata.agent_mode_configs must be repeated RunInferenceAgentModeConfig = 46"
            ));
        }
        if !block.contains("optional RunInferenceAskQuestionConfig ask_question_config = 48;") {
            return Err(format!(
                "{file}: RunInferencePromptModelMetadata.ask_question_config must be optional RunInferenceAskQuestionConfig = 48"
            ));
        }
    }
    if let Some(block) = proto_block(body, "message", "RunInferenceAgentModeConfig") {
        if !(block.contains("string mode_id = 1;")
            && block.contains("bool enable_ask_question_tool = 2;")
            && block
                .contains("optional RunInferenceSwitchModeToolConfig switch_mode_tool_config = 3;"))
        {
            return Err(format!(
                "{file}: RunInferenceAgentModeConfig must include mode_id, enable_ask_question_tool, and switch_mode_tool_config"
            ));
        }
    }
    if let Some(block) = proto_block(body, "service", "InferenceService") {
        if !run_inference().is_match(block) {
            return Err(format!(
                "{file}: InferenceService must expose bidi RunInference"
            ));
        }
        if !run_inference_sse().is_match(block) {
            return Err(format!(
                "{file}: InferenceService must expose RunInferenceSSE"
            ));
        }
    }
    Ok(())
}

pub fn has_placeholder(line: &str) -> bool {
    placeholder().is_match(line)
}

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
            .expect("run inference")
    })
}

fn run_inference_sse() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"rpc\s+RunInferenceSSE\s*\(\s*BidiRequestId\s*\)\s+returns\s+\(\s*stream\s+RunInferenceServerMessage\s*\)")
            .expect("run inference sse")
    })
}

fn placeholder() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(optional\s+|repeated\s+)?[A-Za-z_][A-Za-z0-9_.<>]*\s+(field_\d+|unknown)\s*=\s*\d+\s*;")
            .expect("placeholder")
    })
}
