//! Forward body frames while capturing a limited diagnostic copy.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};

type OnChunk = Box<dyn FnMut(&[u8]) + Send + 'static>;
type OnDone = Box<dyn FnOnce(Vec<u8>, i64, bool, BodyEnd) + Send + 'static>;

pub(crate) enum BodyEnd {
    Complete,
    ReadError(String),
    Cancelled,
}

impl BodyEnd {
    pub(crate) fn error(self) -> Option<String> {
        match self {
            Self::Complete => None,
            Self::ReadError(error) => Some(error),
            Self::Cancelled => Some("正文传输在读完前取消".into()),
        }
    }
}

pub struct CaptureBody<B: Body> {
    inner: B,
    buffer: Vec<u8>,
    limit: usize,
    size: i64,
    truncated: bool,
    done: bool,
    on_chunk: Option<OnChunk>,
    on_done: Option<OnDone>,
}

impl<B: Body> CaptureBody<B> {
    pub fn new(
        inner: B,
        limit: usize,
        on_chunk: impl FnMut(&[u8]) + Send + 'static,
        on_done: impl FnOnce(Vec<u8>, i64, bool, BodyEnd) + Send + 'static,
    ) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            limit,
            size: 0,
            truncated: false,
            done: false,
            on_chunk: Some(Box::new(on_chunk)),
            on_done: Some(Box::new(on_done)),
        }
    }

    fn capture_chunk(&mut self, chunk: &[u8]) {
        self.size += chunk.len() as i64;
        let remaining = self.limit.saturating_sub(self.buffer.len());
        if remaining > 0 {
            let captured = remaining.min(chunk.len());
            self.buffer.extend_from_slice(&chunk[..captured]);
        }
        if self.buffer.len() >= self.limit && self.size > self.buffer.len() as i64 {
            self.truncated = true;
        }
        if let Some(on_chunk) = self.on_chunk.as_mut() {
            on_chunk(chunk);
        }
    }

    fn finish(&mut self, end: BodyEnd) {
        if self.done {
            return;
        }
        self.done = true;
        let captured = std::mem::take(&mut self.buffer);
        if let Some(on_done) = self.on_done.take() {
            on_done(captured, self.size, self.truncated, end);
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
        // CaptureBody is structurally pinned only through `inner`; other fields
        // are never pinned, so projecting them as `&mut` is sound.
        let this = unsafe { self.get_unchecked_mut() };
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };
        match inner.poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.capture_chunk(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.finish(BodyEnd::ReadError(err.to_string()));
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                this.finish(BodyEnd::Complete);
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

impl<B: Body> Drop for CaptureBody<B> {
    fn drop(&mut self) {
        let end = if self.inner.is_end_stream() {
            BodyEnd::Complete
        } else {
            BodyEnd::Cancelled
        };
        self.finish(end);
    }
}
