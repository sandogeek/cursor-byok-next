//! Local-loop Agent / Inference wire-shape checks.

use regex::Regex;
use std::sync::OnceLock;

pub fn validate_required_agent_shapes(file: &str, body: &str) -> Result<(), String> {
    if body.contains("message ExecClientControlMessage")
        && !shape("message ExecClientControlMessage", "ExecClientStreamClose stream_close = 1;").is_match(body)
    {
        return Err(format!(
            "{file}: ExecClientControlMessage.stream_close must be ExecClientStreamClose"
        ));
    }
    if body.contains("message ShellStream")
        && !shape("message ShellStream", "ShellStreamStdout stdout = 1;").is_match(body)
    {
        return Err(format!("{file}: ShellStream.stdout must be ShellStreamStdout"));
    }
    if body.contains("message RunInferencePromptModelMetadata") {
        if !shape(
            "message RunInferencePromptModelMetadata",
            "repeated RunInferenceAgentModeConfig agent_mode_configs = 46;",
        )
        .is_match(body)
        {
            return Err(format!(
                "{file}: RunInferencePromptModelMetadata.agent_mode_configs must be repeated RunInferenceAgentModeConfig = 46"
            ));
        }
        if !shape(
            "message RunInferencePromptModelMetadata",
            "optional RunInferenceAskQuestionConfig ask_question_config = 48;",
        )
        .is_match(body)
        {
            return Err(format!(
                "{file}: RunInferencePromptModelMetadata.ask_question_config must be optional RunInferenceAskQuestionConfig = 48"
            ));
        }
    }
    if body.contains("message RunInferenceAgentModeConfig")
        && !agent_mode_config().is_match(body)
    {
        return Err(format!(
            "{file}: RunInferenceAgentModeConfig must include mode_id, enable_ask_question_tool, and switch_mode_tool_config"
        ));
    }
    if body.contains("service InferenceService") {
        if !run_inference().is_match(body) {
            return Err(format!("{file}: InferenceService must expose bidi RunInference"));
        }
        if !run_inference_sse().is_match(body) {
            return Err(format!("{file}: InferenceService must expose RunInferenceSSE"));
        }
    }
    Ok(())
}

pub fn has_placeholder(line: &str) -> bool {
    placeholder().is_match(line)
}

fn shape(message: &'static str, field: &'static str) -> Regex {
    Regex::new(&format!(
        r"(?s){message}\s*\{{.*?{field}"
    ))
    .expect("shape regex")
}

fn agent_mode_config() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)message\s+RunInferenceAgentModeConfig\s*\{.*?string\s+mode_id\s*=\s*1\s*;.*?bool\s+enable_ask_question_tool\s*=\s*2\s*;.*?optional\s+RunInferenceSwitchModeToolConfig\s+switch_mode_tool_config\s*=\s*3\s*;")
            .expect("agent mode config")
    })
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
