//! One background thread that performs capture work off the forwarding path.
//!
//! `CaptureBody` runs inside `http_body::Body::poll_frame` and inside `Drop`, on
//! the connection's task. Decoding frames and writing SQLite there would stall
//! forwarding and do recursive work in a destructor. Instead those callbacks
//! only hand over a bounded job, and this thread runs it: it owns the decode
//! state and the store write, and it is the only place that blocks.

use std::sync::mpsc::{SyncSender, TrySendError};

/// How many capture jobs may queue before the forwarding path stops capturing.
///
/// The queue is bounded so a slow database cannot grow memory without limit. A
/// full queue marks the capture as incomplete rather than blocking the proxy or
/// silently pretending the capture is whole.
const CAPTURE_QUEUE_JOBS: usize = 256;

/// One unit of capture work, run on the sink thread.
pub(super) type CaptureJob = Box<dyn FnOnce() + Send + 'static>;

/// Hands capture work to the background thread.
#[derive(Clone, Debug)]
pub(super) struct CaptureSink {
    jobs: SyncSender<CaptureJob>,
}

impl CaptureSink {
    /// Starts the sink thread; it stops when every [`CaptureSink`] clone is gone.
    pub(super) fn start() -> Self {
        let (jobs, receiver) = std::sync::mpsc::sync_channel::<CaptureJob>(CAPTURE_QUEUE_JOBS);
        std::thread::Builder::new()
            .name("cursor-capture-store".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    job();
                }
            })
            .expect("spawning the capture store thread");
        Self { jobs }
    }

    /// Queues one job without blocking. Returns `false` when the queue is full,
    /// which the caller reports as an incomplete capture.
    pub(super) fn submit(&self, job: CaptureJob) -> bool {
        match self.jobs.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }

    /// Blocks until every job submitted so far has run. Tests use this to observe
    /// work that the forwarding path hands over asynchronously.
    #[cfg(test)]
    pub(super) fn flush(&self) {
        let (done, finished) = std::sync::mpsc::channel();
        let _ = self.submit(Box::new(move || {
            let _ = done.send(());
        }));
        let _ = finished.recv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_jobs_on_another_thread_without_blocking_the_caller() {
        let sink = CaptureSink::start();
        let (done, received) = std::sync::mpsc::channel();
        assert!(sink.submit(Box::new(move || {
            let _ = done.send(std::thread::current().id());
        })));
        let worker = received.recv().expect("the sink ran the job");
        assert_ne!(worker, std::thread::current().id());
    }

    #[test]
    fn a_full_queue_is_reported_instead_of_blocking() {
        let sink = CaptureSink::start();
        let (gate, closed) = std::sync::mpsc::channel::<()>();
        // Occupy the queue: the first job parks the thread, the rest fill it.
        let _ = sink.submit(Box::new(move || {
            let _ = closed.recv();
        }));
        let mut submitted = 0;
        for _ in 0..(CAPTURE_QUEUE_JOBS * 2) {
            if !sink.submit(Box::new(|| {})) {
                break;
            }
            submitted += 1;
        }
        assert!(
            submitted < CAPTURE_QUEUE_JOBS * 2,
            "a full queue must refuse rather than block"
        );
        drop(gate);
    }
}
