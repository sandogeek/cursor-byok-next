//! Records provider requests, responses, usage, and timing.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use tokio::sync::{Mutex, mpsc};

use crate::{
    Result,
    provider::{FinishReason, ModelEvent, is_valid_response_event},
};

use super::{
    CallStore, NewLlmCall,
    store::{BufferedLlmChunk, PayloadStorage},
};

#[derive(Clone)]
#[expect(
    missing_debug_implementations,
    reason = "the recorder owns a store and task handles that add no useful Debug detail"
)]
pub struct CallRecorder {
    inner: Arc<Inner>,
}

pub(crate) struct CancelOnDrop {
    recorder: CallRecorder,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.recorder.is_finished() {
            return;
        }
        let recorder = self.recorder.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                call_id = %self.recorder.inner.call_id,
                "unfinished LLM call dropped outside Tokio runtime"
            );
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = recorder.cancelled().await {
                tracing::warn!(call_id = %recorder.inner.call_id, %error, "failed to mark dropped LLM call cancelled");
            }
        });
    }
}

struct Inner {
    store: CallStore,
    call_id: String,
    storage: PayloadStorage,
    started: Instant,
    lifecycle: Mutex<Lifecycle>,
    observations: Mutex<Option<mpsc::Sender<Observation>>>,
    writer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    finished: AtomicBool,
}

#[derive(Default)]
struct Lifecycle {
    first_text_recorded: bool,
    first_valid_response_recorded: bool,
}

struct Observation {
    data: Vec<u8>,
    elapsed_ms: i64,
}

const OBSERVATION_QUEUE: usize = 256;
const MAX_BUFFERED_CHUNKS: usize = 32;
const MAX_BUFFERED_BYTES: usize = 256 * 1024;
const MAX_BUFFER_AGE: std::time::Duration = std::time::Duration::from_millis(50);

struct ObservationWriter {
    store: CallStore,
    call_id: String,
    storage: PayloadStorage,
    started: Instant,
    receiver: mpsc::Receiver<Observation>,
    next_chunk: i64,
    chunks: Vec<BufferedLlmChunk>,
    bytes: usize,
    first_chunk_at: Option<Instant>,
}

impl ObservationWriter {
    fn new(
        store: CallStore,
        call_id: String,
        storage: PayloadStorage,
        started: Instant,
        receiver: mpsc::Receiver<Observation>,
    ) -> Self {
        Self {
            store,
            call_id,
            storage,
            started,
            receiver,
            next_chunk: 0,
            chunks: Vec::new(),
            bytes: 0,
            first_chunk_at: None,
        }
    }

    async fn run(mut self) {
        loop {
            let received = if self.chunks.is_empty() {
                self.receiver.recv().await
            } else {
                let deadline = tokio::time::Instant::from_std(
                    self.first_chunk_at.expect("buffer timestamp") + MAX_BUFFER_AGE,
                );
                tokio::select! {
                    received = self.receiver.recv() => received,
                    () = tokio::time::sleep_until(deadline) => {
                        self.flush().await;
                        continue;
                    }
                }
            };
            let Some(observation) = received else {
                self.flush().await;
                return;
            };
            self.chunks.push(if self.storage.keeps_payloads() {
                BufferedLlmChunk::new(self.next_chunk, observation.elapsed_ms, &observation.data)
            } else {
                BufferedLlmChunk::metrics(
                    self.next_chunk,
                    observation.elapsed_ms,
                    observation.data.len(),
                )
            });
            self.next_chunk += 1;
            self.bytes += observation.data.len();
            self.first_chunk_at.get_or_insert_with(Instant::now);
            if self.chunks.len() >= MAX_BUFFERED_CHUNKS || self.bytes >= MAX_BUFFERED_BYTES {
                self.flush().await;
            }
        }
    }

    async fn flush(&mut self) {
        if self.chunks.is_empty() {
            return;
        }
        let chunks = std::mem::take(&mut self.chunks);
        self.bytes = 0;
        self.first_chunk_at = None;
        let first_event_at_ms = started_at_ms(self.started, chunks[0].elapsed_ms);
        if let Err(error) = self
            .store
            .record_llm_chunks(&self.call_id, &chunks, self.storage, first_event_at_ms)
            .await
        {
            self.store.mark_degraded();
            tracing::warn!(call_id = %self.call_id, %error, "cannot record LLM response chunks");
        }
    }
}

impl CallRecorder {
    pub async fn start(store: CallStore, mut call: NewLlmCall) -> Self {
        let storage = store.payload_storage().await.unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot read LLM detailed logging");
            store.mark_degraded();
            PayloadStorage::Metrics
        });
        call.detailed = storage.keeps_payloads();
        if let Err(error) = store.start_llm_call(&call).await {
            tracing::warn!(call_id = %call.call_id, %error, "cannot start LLM call record");
            store.mark_degraded();
        }
        let (sender, receiver) = mpsc::channel(OBSERVATION_QUEUE);
        let started = Instant::now();
        let writer = tokio::spawn(
            ObservationWriter::new(
                store.clone(),
                call.call_id.clone(),
                storage,
                started,
                receiver,
            )
            .run(),
        );
        Self {
            inner: Arc::new(Inner {
                store,
                call_id: call.call_id,
                storage,
                started,
                lifecycle: Mutex::default(),
                observations: Mutex::new(Some(sender)),
                writer: Mutex::new(Some(writer)),
                finished: AtomicBool::new(false),
            }),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.inner.finished.load(Ordering::Acquire)
    }

    pub(crate) fn cancel_on_drop(&self) -> CancelOnDrop {
        CancelOnDrop {
            recorder: self.clone(),
        }
    }

    pub async fn request(
        &self,
        headers: serde_json::Value,
        body: &serde_json::Value,
    ) -> Result<()> {
        if self.is_finished() {
            return Ok(());
        }
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.is_finished() {
            return Ok(());
        }
        self.observe(
            self.inner
                .store
                .record_llm_request(&self.inner.call_id, &headers, body, self.inner.storage)
                .await,
            "cannot record LLM request",
        );
        Ok(())
    }

    pub async fn response_headers(&self, status: u16) -> Result<()> {
        if self.is_finished() {
            return Ok(());
        }
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.is_finished() {
            return Ok(());
        }
        self.observe(
            self.inner
                .store
                .record_llm_response_headers(
                    &self.inner.call_id,
                    elapsed_ms(self.inner.started),
                    status,
                )
                .await,
            "cannot record LLM response headers",
        );
        Ok(())
    }

    pub async fn response_chunk(&self, data: &[u8]) -> Result<()> {
        if self.is_finished() {
            return Ok(());
        }
        let sender = self.inner.observations.lock().await.clone();
        let Some(sender) = sender else {
            return Ok(());
        };
        if sender
            .send(Observation {
                data: data.to_vec(),
                elapsed_ms: elapsed_ms(self.inner.started),
            })
            .await
            .is_err()
        {
            self.inner.store.mark_degraded();
            tracing::warn!(call_id = %self.inner.call_id, "LLM response recorder stopped");
        }
        Ok(())
    }

    pub async fn event(&self, event: &ModelEvent) -> Result<()> {
        if matches!(event, ModelEvent::Done(_))
            && let ModelEvent::Done(reason) = event
        {
            return self.completed(*reason).await;
        }
        if self.is_finished() {
            return Ok(());
        }
        let mut lifecycle = self.inner.lifecycle.lock().await;
        if self.is_finished() {
            return Ok(());
        }
        if is_valid_response_event(event) && !lifecycle.first_valid_response_recorded {
            match self
                .inner
                .store
                .record_llm_first_valid_response(
                    &self.inner.call_id,
                    elapsed_ms(self.inner.started),
                )
                .await
            {
                Ok(()) => lifecycle.first_valid_response_recorded = true,
                Err(error) => {
                    self.inner.store.mark_degraded();
                    tracing::warn!(call_id = %self.inner.call_id, %error, "cannot record first valid LLM response");
                }
            }
        }
        if let ModelEvent::TextDelta(delta) = event
            && !delta.trim().is_empty()
            && !lifecycle.first_text_recorded
        {
            match self
                .inner
                .store
                .record_llm_first_text(&self.inner.call_id, elapsed_ms(self.inner.started))
                .await
            {
                Ok(()) => lifecycle.first_text_recorded = true,
                Err(error) => {
                    self.inner.store.mark_degraded();
                    tracing::warn!(call_id = %self.inner.call_id, %error, "cannot record first LLM text");
                }
            }
        }
        drop(lifecycle);
        if let ModelEvent::Usage(usage) = event {
            self.usage(*usage).await?;
        }
        Ok(())
    }

    pub async fn usage(&self, usage: crate::model::Usage) -> Result<()> {
        if self.is_finished() {
            return Ok(());
        }
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.is_finished() {
            return Ok(());
        }
        self.observe(
            self.inner
                .store
                .record_llm_usage(&self.inner.call_id, usage)
                .await,
            "cannot record LLM usage",
        );
        Ok(())
    }

    pub async fn completed(&self, reason: FinishReason) -> Result<()> {
        self.finish("completed", Some(finish_reason(reason)), None, None)
            .await
    }

    pub async fn failed(&self, error: &crate::Error) -> Result<()> {
        self.finish(
            "error",
            None,
            Some(error_kind(error)),
            Some(&error.to_string()),
        )
        .await
    }

    pub async fn cancelled(&self) -> Result<()> {
        self.finish("cancelled", None, None, None).await
    }

    async fn finish(
        &self,
        status: &str,
        reason: Option<&str>,
        error_kind: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        if self.is_finished() {
            return Ok(());
        }
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.is_finished() {
            return Ok(());
        }
        self.inner.observations.lock().await.take();
        if let Some(writer) = self.inner.writer.lock().await.take()
            && let Err(error) = writer.await
        {
            self.inner.store.mark_degraded();
            tracing::warn!(call_id = %self.inner.call_id, %error, "LLM response recorder stopped unexpectedly");
        }
        if let Err(error) = self
            .inner
            .store
            .finish_llm_call(
                &self.inner.call_id,
                status,
                reason,
                elapsed_ms(self.inner.started),
                error_kind,
                error_message,
            )
            .await
        {
            self.inner.store.mark_degraded();
            tracing::warn!(call_id = %self.inner.call_id, %error, "cannot finish LLM call record");
        }
        self.inner.finished.store(true, Ordering::Release);
        Ok(())
    }

    /// A failed record keeps the call going: the store is degraded, the
    /// response is unaffected.
    fn observe(&self, result: Result<()>, message: &str) {
        if let Err(error) = result {
            self.inner.store.mark_degraded();
            tracing::warn!(call_id = %self.inner.call_id, %error, "{message}");
        }
    }
}

fn started_at_ms(started: Instant, offset_ms: i64) -> i64 {
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let epoch_ms = i64::try_from(epoch_ms).unwrap_or(i64::MAX);
    epoch_ms.saturating_sub(elapsed_ms(started).saturating_sub(offset_ms))
}

fn elapsed_ms(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

fn finish_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolUse => "tool_use",
    }
}

fn error_kind(error: &crate::Error) -> &'static str {
    match error {
        crate::Error::Provider(_) | crate::Error::Http(_) => "provider",
        crate::Error::Cancelled => "cancelled",
        crate::Error::Storage(_) => "store",
        _ => "internal",
    }
}

#[cfg(test)]
mod tests {
    use crate::calls::{CallStore, NewLlmCall};

    use super::*;

    fn sample(call_id: &str) -> NewLlmCall {
        NewLlmCall {
            call_id: call_id.into(),
            run_id: "run".into(),
            conversation_id: "conversation".into(),
            provider_call_index: 0,
            model_hash: "plugin:test/provider/model".into(),
            provider_type: "plugin".into(),
            provider_url: "plugin://test/provider".into(),
            request_type: "plugin".into(),
            request_url: "plugin://test/provider".into(),
            model_id: "model".into(),
            display_name: "Model".into(),
            reasoning_effort: None,
            fast: false,
            message_count: 1,
            tool_count: 0,
            detailed: false,
        }
    }

    #[tokio::test]
    async fn dropping_an_unfinished_call_guard_marks_the_call_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        let recorder = CallRecorder::start(store.clone(), sample("cancel-on-drop")).await;
        drop(recorder.cancel_on_drop());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let status = store
                .llm_call("cancel-on-drop")
                .await
                .unwrap()
                .map(|call| call.status)
                .unwrap_or_default();
            if status == "cancelled" {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "unfinished recorder stayed running after its stream was dropped"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn events_record_timing_usage_and_completion() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        let recorder = CallRecorder::start(store.clone(), sample("events")).await;
        recorder.event(&ModelEvent::ThinkingStart).await.unwrap();
        recorder
            .event(&ModelEvent::TextDelta("hello".into()))
            .await
            .unwrap();
        recorder
            .event(&ModelEvent::Usage(crate::model::Usage {
                input_tokens: Some(8),
                output_tokens: Some(2),
                total_tokens: Some(10),
                ..Default::default()
            }))
            .await
            .unwrap();
        recorder
            .event(&ModelEvent::Done(FinishReason::Stop))
            .await
            .unwrap();
        recorder
            .event(&ModelEvent::TextDelta("late".into()))
            .await
            .unwrap();
        recorder
            .event(&ModelEvent::Usage(crate::model::Usage {
                input_tokens: Some(99),
                ..Default::default()
            }))
            .await
            .unwrap();
        recorder
            .event(&ModelEvent::Done(FinishReason::Length))
            .await
            .unwrap();
        let call = store.llm_call("events").await.unwrap().unwrap();
        assert_eq!(call.status, "completed");
        assert_eq!(call.finish_reason.as_deref(), Some("stop"));
        assert_eq!(call.input_tokens, Some(8));
        assert!(call.ttfr_ms.is_some());
        assert!(call.ttft_ms.is_some());
    }

    #[tokio::test]
    async fn standard_mode_keeps_metrics_without_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        let recorder = CallRecorder::start(store.clone(), sample("metrics")).await;
        recorder
            .request(
                serde_json::json!({"content-type": "application/json"}),
                &serde_json::json!({"model": "test"}),
            )
            .await
            .unwrap();
        recorder.response_headers(200).await.unwrap();
        recorder.response_chunk(b"data: [DONE]\n\n").await.unwrap();
        recorder.completed(FinishReason::Stop).await.unwrap();
        assert!(store.llm_call_request("metrics").await.unwrap().is_none());
        assert_eq!(
            store.llm_call_chunks("metrics").await.unwrap(),
            Vec::<crate::calls::LlmCallResponseChunk>::new()
        );
        let call = store.llm_call("metrics").await.unwrap().unwrap();
        assert_eq!(call.http_status, Some(200));
        assert_eq!(call.response_bytes, 14);
        assert_eq!(call.response_chunk_count, 1);
        assert!(!call.detailed);
    }

    #[tokio::test]
    async fn failed_and_cancelled_are_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        let recorder = CallRecorder::start(store.clone(), sample("failed")).await;
        recorder
            .failed(&crate::Error::Provider("boom".into()))
            .await
            .unwrap();
        recorder.cancelled().await.unwrap();
        let call = store.llm_call("failed").await.unwrap().unwrap();
        assert_eq!(call.status, "error");
        assert_eq!(call.error_kind.as_deref(), Some("provider"));
        assert_eq!(call.error_message.as_deref(), Some("boom"));
    }
}
