//! Buffers, replays, broadcasts, and atomically closes downstream output.
//!
//! Every queue here is bounded. A subscriber that stops draining is dropped
//! instead of being buffered forever, and the replay history is capped by both
//! frame count and bytes, so a run cannot grow without limit when its consumer
//! slows down.

use std::collections::VecDeque;

use bytes::Bytes;
use tokio::sync::mpsc;

/// Frames kept for a subscriber that attaches late. A Cursor run replays from
/// the start, so the history is the whole stream; the cap only guards a run
/// whose consumer never appears.
const MAX_REPLAY_FRAMES: usize = 4_096;
/// Bytes kept for the same replay. Frames can be far larger than a few hundred
/// bytes, so the frame cap alone does not bound memory.
const MAX_REPLAY_BYTES: usize = 64 * 1024 * 1024;
/// Frames a single subscriber may fall behind before it is dropped. Big enough
/// for a normal network stall, small enough that a dead reader cannot hold a
/// whole run in memory.
const SUBSCRIBER_QUEUE_FRAMES: usize = 1_024;

/// Fans one run's frames out to the streams reading them.
#[derive(Debug, Default)]
pub struct OutputHub {
    state: parking_lot::Mutex<OutputState>,
}

#[derive(Debug, Default)]
struct OutputState {
    /// The frames emitted so far, oldest first, within both replay caps.
    history: VecDeque<Bytes>,
    /// Total bytes held in `history`, so the byte cap is O(1) to enforce.
    history_bytes: usize,
    subscribers: Vec<Subscriber>,
    closed: bool,
    /// Subscribers dropped because they fell behind. A run that reports this is
    /// not a complete delivery and the caller must not claim success.
    lagged: usize,
}

#[derive(Debug)]
struct Subscriber {
    sender: mpsc::Sender<Bytes>,
}

impl Subscriber {
    /// Accepts `frame` without awaiting. Returns whether the subscriber stays.
    ///
    /// A full queue means the reader has stopped draining, so the subscriber is
    /// dropped: its channel closes and the consumer ends instead of silently
    /// losing frames it would later report as complete.
    fn offer(&mut self, frame: &Bytes) -> bool {
        match self.sender.try_send(frame.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

impl OutputState {
    fn broadcast(&mut self, frame: &Bytes) -> bool {
        if self.closed {
            return false;
        }
        let mut lagged = 0;
        self.subscribers.retain_mut(|subscriber| {
            let kept = subscriber.offer(frame);
            if !kept {
                lagged += 1;
            }
            kept
        });
        self.lagged += lagged;
        true
    }
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
    /// what makes a late write after `close` harmless. A subscriber that has
    /// fallen behind is dropped and counted by [`OutputHub::lagged`].
    pub fn emit(&self, frame: Bytes) -> bool {
        let mut state = self.state.lock();
        if !state.broadcast(&frame) {
            return false;
        }
        let len = frame.len();
        state.history.push_back(frame);
        state.history_bytes += len;
        while state.history.len() > MAX_REPLAY_FRAMES
            || (state.history_bytes > MAX_REPLAY_BYTES && state.history.len() > 1)
        {
            if let Some(dropped) = state.history.pop_front() {
                state.history_bytes -= dropped.len();
            }
        }
        true
    }

    /// Sends one ephemeral frame (such as a server heartbeat) to every live
    /// subscriber without storing it in the replay history.
    pub fn emit_live(&self, frame: &Bytes) -> bool {
        self.state.lock().broadcast(frame)
    }

    /// A receiver that first replays the frames already emitted, then follows
    /// live output. A closed hub yields the replay and then ends.
    ///
    /// The receiver holds at most [`SUBSCRIBER_QUEUE_FRAMES`] frames; a reader
    /// that stops draining is cut off (see [`OutputHub::emit`]).
    pub fn subscribe(&self) -> mpsc::Receiver<Bytes> {
        let (sender, receiver) = mpsc::channel(SUBSCRIBER_QUEUE_FRAMES);
        let mut state = self.state.lock();
        for frame in &state.history {
            if sender.try_send(frame.clone()).is_err() {
                // The replay alone already exceeds the subscriber queue; the
                // subscriber could never catch up to the live stream, so let it
                // read what fit and then end.
                return receiver;
            }
        }
        if !state.closed {
            state.subscribers.push(Subscriber { sender });
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
        state.history_bytes = 0;
        true
    }

    /// How many subscribers were dropped for falling behind. Non-zero means part
    /// of this run's output never reached every reader.
    #[must_use]
    pub fn lagged(&self) -> usize {
        self.state.lock().lagged
    }

    #[cfg(test)]
    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().closed
    }

    #[cfg(test)]
    pub(super) fn has_history(&self) -> bool {
        !self.state.lock().history.is_empty()
    }

    #[cfg(test)]
    pub(super) fn has_lagged(&self) -> bool {
        self.state.lock().lagged > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replays_history_to_a_late_subscriber() {
        let hub = OutputHub::new();
        hub.emit(Bytes::from_static(b"first"));
        let mut receiver = hub.subscribe();
        assert_eq!(receiver.recv().await.unwrap(), Bytes::from_static(b"first"));
    }

    #[tokio::test]
    async fn a_slow_subscriber_is_dropped_instead_of_buffering_forever() {
        let hub = OutputHub::new();
        let mut receiver = hub.subscribe();
        for _ in 0..(SUBSCRIBER_QUEUE_FRAMES * 2) {
            hub.emit(Bytes::from_static(b"frame"));
        }
        assert!(
            hub.has_lagged(),
            "a reader that never drains must be cut off"
        );
        // Draining yields only the queued frames, then the channel closes.
        let mut drained = 0;
        while receiver.recv().await.is_some() {
            drained += 1;
        }
        assert!(drained <= SUBSCRIBER_QUEUE_FRAMES);
    }

    #[tokio::test]
    async fn replay_is_capped_by_bytes_but_keeps_the_newest() {
        let hub = OutputHub::new();
        let frame = Bytes::from(vec![0u8; MAX_REPLAY_BYTES / 4 + 1]);
        for _ in 0..8 {
            hub.emit(frame.clone());
        }
        let mut receiver = hub.subscribe();
        let mut replayed = 0;
        while receiver.try_recv().is_ok() {
            replayed += 1;
        }
        assert!(replayed >= 1);
    }

    #[test]
    fn emit_live_reaches_active_subscribers_without_entering_history() {
        let hub = OutputHub::new();
        let mut live = hub.subscribe();
        assert!(hub.emit_live(&Bytes::from_static(b"heartbeat")));
        assert_eq!(live.try_recv().unwrap(), Bytes::from_static(b"heartbeat"));
        assert!(!hub.has_history());
        let mut late = hub.subscribe();
        assert!(late.try_recv().is_err());
    }
}
