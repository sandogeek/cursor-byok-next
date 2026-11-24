//! Accumulates one streamed tool call from provider-specific deltas.
//!
//! `OpenAI` Chat and `OpenAI` Responses deliver the same three fields in
//! different shapes, so the "when do I emit `ToolCallStart` and the argument
//! delta" bookkeeping lives here once.

use std::collections::BTreeMap;

use crate::{Error, Result, provider::ModelEvent};

use super::ToolNames;

/// How much of one tool call has been emitted. A pair of `bool`s would allow
/// the impossible `ended && !started` state to be read as two independent facts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ToolPhase {
    /// Neither `ToolCallStart` nor `ToolCallEnd` emitted yet.
    #[default]
    Pending,
    /// `ToolCallStart` emitted; `ToolCallEnd` still owed.
    Started,
    /// Both emitted.
    Ended,
}

impl ToolPhase {
    fn is_started(self) -> bool {
        self != Self::Pending
    }

    fn is_ended(self) -> bool {
        self == Self::Ended
    }
}

/// The parts of a tool call that stream in separately.
#[derive(Clone, Debug, Default)]
pub(super) struct ToolState {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    /// Bytes of `arguments` already sent as a delta.
    emitted: usize,
    phase: ToolPhase,
}

impl ToolState {
    /// Records a `call_id` fragment. Cursor may repeat or extend the value, so
    /// a fragment that does not extend what arrived is dropped.
    pub(super) fn merge_call_id(&mut self, fragment: &str) {
        merge_fragment(&mut self.call_id, fragment);
    }

    /// Records a `name` fragment, with the same rule as the id.
    pub(super) fn merge_name(&mut self, fragment: &str) {
        merge_fragment(&mut self.name, fragment);
    }

    /// Takes an identity a provider sends once, keeping the first value.
    pub(super) fn fill_call_id(&mut self, call_id: &str) {
        if self.call_id.is_none() {
            self.call_id = Some(call_id.to_owned());
        }
    }

    /// Takes a name a provider sends once, keeping the first value.
    pub(super) fn fill_name(&mut self, name: &str) {
        if self.name.is_none() {
            self.name = Some(name.to_owned());
        }
    }

    /// Takes the `call_id` and `name` a provider sends once, keeping the first
    /// value of each.
    pub(super) fn fill_identity(&mut self, call_id: Option<&str>, name: Option<&str>) {
        if let Some(call_id) = call_id {
            self.fill_call_id(call_id);
        }
        if let Some(name) = name {
            self.fill_name(name);
        }
    }

    /// Appends streamed argument bytes.
    pub(super) fn push_arguments(&mut self, delta: &str) {
        self.arguments.push_str(delta);
    }

    /// Appends the full argument snapshot a provider sends at the end.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Provider`] when the snapshot does not extend what was
    /// streamed, because the two disagree about the call's arguments.
    pub(super) fn extend_to_snapshot(&mut self, snapshot: &str) -> Result<()> {
        if snapshot == self.arguments {
            return Ok(());
        }
        if !snapshot.starts_with(self.arguments.as_str()) {
            return Err(Error::Provider(
                "OpenAI Responses final tool arguments do not match streamed arguments".into(),
            ));
        }
        self.arguments.push_str(&snapshot[self.arguments.len()..]);
        Ok(())
    }

    /// Fills a missing `call_id` with a deterministic fallback.
    pub(super) fn default_call_id(&mut self, index: usize) {
        if self.call_id.is_none() {
            self.call_id = Some(format!("call-{index}"));
        }
    }

    /// Whether the `ToolCallStart` for this call has been emitted.
    pub(super) fn is_started(&self) -> bool {
        self.phase.is_started()
    }

    /// The identity the `ToolCallStart` reports, if both parts arrived.
    fn has_identity(&self) -> bool {
        self.call_id_text().is_some() && self.name_text().is_some()
    }

    /// The call name, treating a missing or empty name as absent.
    pub(super) fn name_text(&self) -> Option<&str> {
        self.name.as_deref().filter(|name| !name.is_empty())
    }

    /// The `ToolCallStart` this call still owes, if its identity is known.
    pub(super) fn take_start(
        &mut self,
        index: usize,
        tool_names: &ToolNames,
    ) -> Option<ModelEvent> {
        if self.phase.is_started() || !self.has_identity() {
            return None;
        }
        let call_id = super::truncated_call_id(self.call_id_text()?).to_owned();
        let name = tool_names.restore(self.name_text()?);
        self.phase = ToolPhase::Started;
        Some(ModelEvent::ToolCallStart {
            index,
            call_id,
            name,
        })
    }

    /// The argument delta this call still owes, once it has started.
    pub(super) fn take_arguments_delta(&mut self, index: usize) -> Option<ModelEvent> {
        if !self.phase.is_started() || self.emitted >= self.arguments.len() {
            return None;
        }
        let delta = self.arguments[self.emitted..].to_owned();
        self.emitted = self.arguments.len();
        Some(ModelEvent::ToolCallArgumentsDelta { index, delta })
    }

    /// Whether the `ToolCallEnd` for this call has been emitted.
    pub(super) fn is_ended(&self) -> bool {
        self.phase.is_ended()
    }

    /// The `ToolCallEnd` this call still owes.
    pub(super) fn take_end(&mut self, index: usize) -> Option<ModelEvent> {
        if self.phase.is_ended() {
            return None;
        }
        self.phase = ToolPhase::Ended;
        Some(ModelEvent::ToolCallEnd { index })
    }

    fn call_id_text(&self) -> Option<&str> {
        self.call_id
            .as_deref()
            .filter(|call_id| !call_id.is_empty())
    }
}

/// One streamed tool call per provider index.
pub(super) type ToolCalls = BTreeMap<usize, ToolState>;

/// Keeps `target` and `fragment` consistent when a provider repeats or re-sends
/// an identity it already streamed.
fn merge_fragment(target: &mut Option<String>, fragment: &str) {
    match target {
        None => *target = Some(fragment.to_owned()),
        Some(existing) => {
            if existing.as_str() == fragment || existing.ends_with(fragment) {
                return;
            }
            if fragment.starts_with(existing.as_str()) {
                fragment.clone_into(existing);
            } else {
                existing.push_str(fragment);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_id_fragment_is_not_duplicated() {
        let mut tool = ToolState::default();
        tool.merge_call_id("call-1");
        tool.merge_call_id("call-1");
        tool.merge_call_id("-tail");
        assert_eq!(tool.call_id.as_deref(), Some("call-1-tail"));
    }

    #[test]
    fn a_longer_id_replaces_the_prefix_it_extends() {
        let mut tool = ToolState::default();
        tool.merge_call_id("call");
        tool.merge_call_id("call-1");
        assert_eq!(tool.call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn arguments_stream_once_after_the_call_starts() {
        let tool_names = ToolNames::default();
        let mut tool = ToolState::default();
        tool.merge_call_id("call-1");
        tool.push_arguments("{\"a\"");
        assert!(tool.take_start(0, &tool_names).is_none(), "name is missing");
        tool.merge_name("read");
        assert!(matches!(
            tool.take_start(0, &tool_names),
            Some(ModelEvent::ToolCallStart { .. })
        ));
        assert!(matches!(
            tool.take_arguments_delta(0),
            Some(ModelEvent::ToolCallArgumentsDelta { .. })
        ));
        assert!(tool.take_arguments_delta(0).is_none());
        assert!(tool.take_end(0).is_some());
        assert!(tool.take_end(0).is_none());
    }

    #[test]
    fn a_snapshot_extends_what_streamed_but_cannot_rewrite_it() {
        let mut tool = ToolState::default();
        tool.push_arguments("{\"a\":1");

        assert!(
            tool.extend_to_snapshot("{\"a\":2}").is_err(),
            "a snapshot that contradicts the streamed bytes is a provider bug"
        );
        assert_eq!(
            tool.arguments, "{\"a\":1",
            "a rejected snapshot changes nothing"
        );

        assert!(
            tool.extend_to_snapshot("{\"a\":1").is_ok(),
            "an exact repeat is a no-op"
        );
        assert!(tool.extend_to_snapshot("{\"a\":1,\"b\":2}").is_ok());
        assert_eq!(tool.arguments, "{\"a\":1,\"b\":2}");
    }
}
