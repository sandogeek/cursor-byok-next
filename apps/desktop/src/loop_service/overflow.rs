//! Whether a provider failure means the prompt did not fit.

pub(crate) fn is_context_overflow(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("prompt is too long")
        || lowered.contains("context window exceeded")
        || lowered.contains("model_context_window_exceeded")
        || lowered.contains("context_length_exceeded")
        || (lowered.contains("maximum context length") && lowered.contains("token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_overflow_messages() {
        assert!(is_context_overflow(
            "prompt is too long: 200001 tokens > 200000 maximum"
        ));
        assert!(!is_context_overflow("401 Unauthorized: invalid api key"));
    }
}
