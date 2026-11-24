//! Buffers, replays, broadcasts, and atomically closes downstream output.

use std::collections::VecDeque;

use bytes::Bytes;
use tokio::sync::mpsc;

/// Frames kept for a subscriber that attaches late. A Cursor run replays from
/// the start, so the history is the whole stream; the cap only guards a run
/// whose consumer never appears.
const MAX_REPLAY_FRAMES: usize = 4_096;

/// Fans one run's frames out to the streams reading them.
#[derive(Debug, Default)]
pub struct OutputHub {
    state: parking_lot::Mutex<OutputState>,
}

#[derive(Debug, Default)]
struct OutputState {
    /// The frames emitted so far, oldest first, capped at [`MAX_REPLAY_FRAMES`].
    history: VecDeque<Bytes>,
    subscribers: Vec<mpsc::UnboundedSender<Bytes>>,
    closed: bool,
}

impl OutputHub {
    /// The empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends one frame to every live subscriber and keeps it for replay.
    ///
    /// Returns whether the frame was accepted; a closed hub drops it, which is
    /// what makes a late write after `close` harmless.
    pub fn emit(&self, frame: Bytes) -> bool {
        let mut state = self.state.lock();
        if state.closed {
            return false;
        }
        state
            .subscribers
            .retain(|subscriber| subscriber.send(frame.clone()).is_ok());
        if state.history.len() == MAX_REPLAY_FRAMES {
            state.history.pop_front();
        }
        state.history.push_back(frame);
        true
    }

    /// A receiver that first replays the frames already emitted, then follows
    /// live output. A closed hub yields the replay and then ends.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Bytes> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut state = self.state.lock();
        for frame in &state.history {
            let _ = sender.send(frame.clone());
        }
        if !state.closed {
            state.subscribers.push(sender);
        }
        receiver
    }

    /// Ends the stream. Returns whether this call closed it.
    pub fn close(&self) -> bool {
        let mut state = self.state.lock();
        if state.closed {
            return false;
        }
        state.closed = true;
        state.subscribers.clear();
        state.history.clear();
        true
    }

    #[cfg(test)]
    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().closed
    }

    #[cfg(test)]
    pub(super) fn has_history(&self) -> bool {
        !self.state.lock().history.is_empty()
    }
}
