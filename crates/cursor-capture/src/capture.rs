//! Forward body frames while capturing a limited diagnostic copy.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;

type OnChunk = Box<dyn FnMut(&[u8]) + Send + 'static>;
type OnDone = Box<dyn FnOnce(CapturedBody) + Send + 'static>;

/// How a captured body ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BodyEnd {
    /// The stream reached its end-of-stream marker.
    Complete,
    /// Reading the stream failed. The error is flattened to text because the
    /// body's error type is generic and the debug UI only shows text.
    ReadError(String),
    /// The body was dropped before it finished.
    Cancelled,
}

impl BodyEnd {
    /// The message to show, or `None` when the body finished normally.
    pub(crate) fn error(self) -> Option<String> {
        match self {
            Self::Complete => None,
            Self::ReadError(message) => Some(message),
            Self::Cancelled => Some("正文传输在读完前取消".into()),
        }
    }
}

/// Whether a captured copy is the whole body or the head of a larger one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyCompleteness {
    /// `bytes` holds everything the body carried.
    Complete,
    /// `bytes` stopped at the capture limit before the body did, so a decoder
    /// must not treat the missing tail as a parse error.
    Truncated,
}

/// The diagnostic copy of one body, handed to the store when the stream ends.
#[derive(Debug)]
pub(crate) struct CapturedBody {
    /// At most the configured capture limit of leading bytes.
    pub(crate) bytes: Vec<u8>,
    /// How many bytes the body actually carried.
    pub(crate) size: i64,
    /// Whether `bytes` is the whole body.
    pub(crate) completeness: BodyCompleteness,
    /// How the body ended.
    pub(crate) end: BodyEnd,
}

/// The leading bytes of a body, capped at `limit`.
#[derive(Debug)]
struct CaptureBuffer {
    bytes: Vec<u8>,
    limit: usize,
    size: i64,
    /// Whether a chunk was cut at `limit`; `bytes` then holds only a head.
    hit_limit: bool,
}

impl CaptureBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            size: 0,
            hit_limit: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.size += i64::try_from(chunk.len()).unwrap_or(i64::MAX);
        let captured = self.limit.saturating_sub(self.bytes.len()).min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..captured]);
        if captured < chunk.len() {
            self.hit_limit = true;
        }
    }

    fn take(&mut self, end: BodyEnd) -> CapturedBody {
        CapturedBody {
            bytes: std::mem::take(&mut self.bytes),
            size: self.size,
            completeness: if self.hit_limit {
                BodyCompleteness::Truncated
            } else {
                BodyCompleteness::Complete
            },
            end,
        }
    }
}

pin_project! {
    /// Wraps a body, forwarding every frame and keeping a bounded copy.
    pub struct CaptureBody<B: Body> {
        #[pin]
        inner: B,
        captured: CaptureBuffer,
        done: bool,
        on_chunk: Option<OnChunk>,
        on_done: Option<OnDone>,
    }

    impl<B: Body> PinnedDrop for CaptureBody<B> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if *this.done {
                return;
            }
            // An empty body can be known-complete without ever being polled, so
            // `is_end_stream` decides; a body dropped mid-stream did not finish.
            let end = if this.inner.is_end_stream() {
                BodyEnd::Complete
            } else {
                BodyEnd::Cancelled
            };
            finish(this.captured, this.done, this.on_done, end);
        }
    }
}

impl<B: Body> std::fmt::Debug for CaptureBody<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureBody")
            .field("captured", &self.captured)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl<B: Body> CaptureBody<B> {
    /// Wraps `inner`, forwarding each chunk to `on_chunk`.
    pub fn new(
        inner: B,
        limit: usize,
        on_chunk: impl FnMut(&[u8]) + Send + 'static,
        on_done: impl FnOnce(CapturedBody) + Send + 'static,
    ) -> Self {
        Self {
            inner,
            captured: CaptureBuffer::new(limit),
            done: false,
            on_chunk: Some(Box::new(on_chunk)),
            on_done: Some(Box::new(on_done)),
        }
    }
}

impl<B> Body for CaptureBody<B>
where
    B: Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.captured.push(data);
                    if let Some(on_chunk) = this.on_chunk.as_mut() {
                        on_chunk(data);
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(err))) => {
                finish(
                    this.captured,
                    this.done,
                    this.on_done,
                    BodyEnd::ReadError(err.to_string()),
                );
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                finish(this.captured, this.done, this.on_done, BodyEnd::Complete);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Hands the captured copy to the store exactly once per body.
fn finish(
    captured: &mut CaptureBuffer,
    done: &mut bool,
    on_done: &mut Option<OnDone>,
    end: BodyEnd,
) {
    if *done {
        return;
    }
    *done = true;
    if let Some(on_done) = on_done.take() {
        on_done(captured.take(end));
    }
}
