//! Tracks in-flight plugin invocations, concurrency budgets, and stream state.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
    },
};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::{Error, Result};
use crate::{calls::CallRecorder, model::ModelInvocation, provider::ModelEvent};

/// Events one invocation may have queued for its consumer. Bounded so a plugin
/// that emits faster than the model pipeline drains cannot grow memory without
/// limit; a full queue ends the invocation instead of dropping events silently.
pub(super) const EVENT_QUEUE_ITEMS: usize = 4_096;
/// Live invocations one worker serves at once. Each costs a task plus its event
/// queue, so a burst is capped rather than spawned without bound.
const MAX_ACTIVE_INVOCATIONS: usize = 16;
/// `host_call`s one invocation may have in flight. A plugin that fans out calls
/// without awaiting them is rejected past this instead of spawning forever.
pub(super) const MAX_HOST_CALLS_PER_INVOCATION: usize = 8;

/// One item a plugin invocation produces: a streamed model event, or the
/// terminal result that ends the invocation.
#[derive(Debug)]
pub enum WorkerStreamItem {
    Event(ModelEvent),
    Result(Result<serde_json::Value>),
}

/// Per-invocation concurrency, so a plugin cannot spawn without bound.
pub(super) struct InvocationBudget {
    host_calls: AtomicUsize,
}

impl InvocationBudget {
    fn new() -> Self {
        Self {
            host_calls: AtomicUsize::new(0),
        }
    }

    /// Takes one `host_call` slot, or reports the invocation is saturated.
    pub(super) fn try_acquire_host_call(&self) -> bool {
        let mut current = self.host_calls.load(Ordering::Relaxed);
        loop {
            if current >= MAX_HOST_CALLS_PER_INVOCATION {
                return false;
            }
            match self.host_calls.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    pub(super) fn release_host_call(&self) {
        self.host_calls.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct Pending(parking_lot::Mutex<PendingInvocationMap>);

/// The invocation registry while the worker runs, or `None` once it stopped.
type PendingInvocationMap = Option<HashMap<String, PendingInvocation>>;

struct PendingInvocation {
    items: mpsc::Sender<WorkerStreamItem>,
    active: watch::Sender<()>,
    state: Arc<InvocationState>,
    budget: Arc<InvocationBudget>,
}

pub(super) struct InvocationGuard {
    id: String,
    pending: Arc<Pending>,
}

impl InvocationGuard {
    pub(super) fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        self.pending.remove(&self.id);
    }
}

/// A registered invocation's handles: deregistration guard, output receiver,
/// liveness sender/watch pair for the VM bridge.
pub(super) type RegisteredInvocation = (
    InvocationGuard,
    mpsc::Receiver<WorkerStreamItem>,
    mpsc::Sender<WorkerStreamItem>,
    watch::Receiver<()>,
);

impl Pending {
    pub(super) fn open() -> Self {
        Self(parking_lot::Mutex::new(Some(HashMap::new())))
    }

    /// Registers one in-flight invocation. `self` stays an `Arc` at the call
    /// site because the returned guard must keep the map alive.
    pub(super) fn register(
        self: &Arc<Self>,
        mut state: InvocationState,
    ) -> Result<RegisteredInvocation> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut pending = self.0.lock();
        let map = pending
            .as_mut()
            .ok_or_else(|| Error::Provider("plugin worker stopped".into()))?;
        if map.len() >= MAX_ACTIVE_INVOCATIONS {
            return Err(Error::Provider(format!(
                "plugin worker is already running {MAX_ACTIVE_INVOCATIONS} invocations"
            )));
        }
        let (items, receiver) = state.bind_channel();
        let (active, active_rx) = watch::channel(());
        map.insert(
            id.clone(),
            PendingInvocation {
                items: items.clone(),
                active,
                state: Arc::new(state),
                budget: Arc::new(InvocationBudget::new()),
            },
        );
        Ok((
            InvocationGuard {
                id,
                pending: Arc::clone(self),
            },
            receiver,
            items,
            active_rx,
        ))
    }

    pub(super) fn sender(&self, id: &str) -> Option<mpsc::Sender<WorkerStreamItem>> {
        self.0
            .lock()
            .as_ref()?
            .get(id)
            .map(|invocation| invocation.items.clone())
    }

    pub(super) fn remove_and_fail(&self, id: &str, error: Error) {
        if let Some(PendingInvocation { items, active, .. }) = self.remove(id) {
            drop(active);
            let _ = items.try_send(WorkerStreamItem::Result(Err(error)));
        }
    }

    /// Delivers a terminal `Result` frame, waking the consumer with the invocation's
    /// own failure message when the plugin reported an error.
    pub(super) fn complete_result(&self, id: &str, result: serde_json::Value, error: Option<&str>) {
        if let Some(PendingInvocation {
            items,
            active,
            state,
            budget: _,
        }) = self.remove(id)
        {
            drop(active);
            let value = if error.is_some() {
                Err(state.failure())
            } else {
                Ok(result)
            };
            let _ = items.try_send(WorkerStreamItem::Result(value));
        }
    }

    fn remove(&self, id: &str) -> Option<PendingInvocation> {
        let invocation = self.0.lock().as_mut()?.remove(id)?;
        invocation.state.cancel();
        Some(invocation)
    }

    /// The state and liveness watch one `host_call` runs under, plus the budget
    /// the spawned call must hold a slot in.
    pub(super) fn authorization(
        &self,
        id: &str,
    ) -> Option<(
        Arc<InvocationState>,
        watch::Receiver<()>,
        Arc<InvocationBudget>,
    )> {
        let pending = self.0.lock();
        let invocation = pending.as_ref()?.get(id)?;
        Some((
            Arc::clone(&invocation.state),
            invocation.active.subscribe(),
            Arc::clone(&invocation.budget),
        ))
    }

    pub(super) fn is_closed(&self) -> bool {
        self.0.lock().is_none()
    }

    pub(super) fn close(&self, message: &str) {
        let pending = self.0.lock().take();
        for (
            _,
            PendingInvocation {
                items,
                active,
                state,
                ..
            },
        ) in pending.into_iter().flatten()
        {
            state.cancel();
            drop(active);
            // A full queue cannot take the terminal error; the consumer ends when
            // the sender drops with this map, so nothing is silently "completed".
            let _ = items.try_send(WorkerStreamItem::Result(Err(Error::Provider(
                message.into(),
            ))));
        }
    }
}

pub(super) struct InvocationState {
    allowed_origins: HashSet<String>,
    /// Set only for Magpie's `models.list`, and only its own HTTP status is
    /// recorded: the app reports that status as `catalogError`.
    magpie_catalog_status: Option<AtomicU16>,
    cancellation: CancellationToken,
    recorder: Option<CallRecorder>,
    recorder_claimed: AtomicBool,
    model: Option<Arc<ModelInvocation>>,
    events: parking_lot::Mutex<Option<mpsc::Sender<WorkerStreamItem>>>,
}

impl InvocationState {
    pub(super) fn new(
        allowed_origins: HashSet<String>,
        cancellation: CancellationToken,
        recorder: Option<CallRecorder>,
        model: Option<Arc<ModelInvocation>>,
    ) -> Self {
        Self {
            allowed_origins,
            magpie_catalog_status: None,
            cancellation,
            recorder,
            recorder_claimed: AtomicBool::new(false),
            model,
            events: parking_lot::Mutex::new(None),
        }
    }

    pub(super) fn with_magpie_catalog_tracking(mut self) -> Self {
        self.magpie_catalog_status = Some(AtomicU16::default());
        self
    }

    fn bind_channel(
        &mut self,
    ) -> (
        mpsc::Sender<WorkerStreamItem>,
        mpsc::Receiver<WorkerStreamItem>,
    ) {
        let (items, receiver) = mpsc::channel(EVENT_QUEUE_ITEMS);
        *self.events.lock() = Some(items.clone());
        (items, receiver)
    }

    pub(super) fn is_allowed_origin(&self, origin: &str) -> bool {
        self.allowed_origins.contains(origin)
    }

    /// Tracks HTTP status for Magpie's `models.list` discovery call; returns
    /// `true` when the call is that tracked catalog request so the host can
    /// treat non-2xx replies as an empty catalog with a recorded `catalogError`.
    pub(super) fn observe_catalog_response(
        &self,
        origin: &str,
        method: &reqwest::Method,
        path: &str,
        status: u16,
    ) -> bool {
        let Some(slot) = &self.magpie_catalog_status else {
            return false;
        };
        if !self.allowed_origins.contains(origin)
            || *method != reqwest::Method::GET
            || path != "/v1/models"
        {
            return false;
        }
        slot.store(status, Ordering::Relaxed);
        true
    }

    pub(super) async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(super) fn child_cancellation(&self) -> CancellationToken {
        self.cancellation.child_token()
    }

    pub(super) fn cancel(&self) {
        self.cancellation.cancel();
    }

    #[cfg(test)]
    pub(super) fn set_model(&mut self, model: Arc<ModelInvocation>) {
        self.model = Some(model);
    }

    pub(super) fn claim_recorder(&self) -> Option<CallRecorder> {
        self.recorder.as_ref().and_then(|recorder| {
            self.recorder_claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .ok()
                .map(|_| recorder.clone())
        })
    }

    pub(super) fn emit(&self, event: ModelEvent) {
        let Some(events) = self.events.lock().as_ref().cloned() else {
            return;
        };
        if events.try_send(WorkerStreamItem::Event(event)).is_err() {
            self.cancellation.cancel();
            // The consumer fell behind its budget. End the invocation with an
            // explicit error instead of dropping an event and continuing as
            // if the stream were still complete.
            let _ = events.try_send(WorkerStreamItem::Result(Err(Error::Provider(
                "plugin event queue overflowed; the invocation was ended".into(),
            ))));
        }
    }

    pub(super) fn model(&self) -> Result<&ModelInvocation> {
        self.model.as_deref().ok_or_else(|| {
            Error::Protocol("protocol.stream requires a registered model request".into())
        })
    }

    pub(super) fn failure(&self) -> Error {
        let status = self
            .magpie_catalog_status
            .as_ref()
            .map(|status| status.load(Ordering::Relaxed));
        let message = match status {
            Some(status @ (100..=199 | 300..=599)) => {
                format!("Magpie model discovery failed (HTTP {status})")
            }
            _ => "plugin invocation failed".into(),
        };
        Error::Provider(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn empty_invocation() -> InvocationState {
        InvocationState::new(HashSet::new(), CancellationToken::new(), None, None)
    }

    #[tokio::test]
    async fn pending_authorization_ends_on_completion_and_drop() {
        let pending = Arc::new(Pending::open());
        let (completed, mut receiver, items, _) = pending.register(empty_invocation()).unwrap();
        let (_, mut active, _) = pending.authorization(completed.id()).unwrap();
        let removed = pending.remove(completed.id()).unwrap();
        drop(removed.active);
        drop(items);
        removed
            .items
            .try_send(WorkerStreamItem::Result(Ok(serde_json::Value::Null)))
            .unwrap();
        match receiver.recv().await {
            Some(WorkerStreamItem::Result(Ok(value))) => assert!(value.is_null()),
            other => panic!("expected result, got {other:?}"),
        }
        assert!(active.changed().await.is_err());
        assert!(pending.authorization(completed.id()).is_none());

        let (cancelled, mut receiver, _, _) = pending.register(empty_invocation()).unwrap();
        let id = cancelled.id().to_owned();
        let (_, mut active, _) = pending.authorization(&id).unwrap();
        drop(cancelled);
        assert!(receiver.recv().await.is_none());
        assert!(active.changed().await.is_err());
        assert!(pending.authorization(&id).is_none());
    }

    #[tokio::test]
    async fn invocation_timeout_drops_its_pending_entry() {
        let pending = Arc::new(Pending::open());
        let mut id = None;
        let timed_out = tokio::time::timeout(Duration::from_millis(1), async {
            let (request, mut result, _, _) = pending.register(empty_invocation()).unwrap();
            id = Some(request.id().to_owned());
            result.recv().await
        })
        .await;
        assert!(timed_out.is_err());
        assert!(pending.authorization(&id.unwrap()).is_none());
    }

    #[tokio::test]
    async fn closing_one_generation_revokes_only_its_invocations() {
        let old = Arc::new(Pending::open());
        let (old_request, mut failed, _, _) = old.register(empty_invocation()).unwrap();
        let (_, mut active, _) = old.authorization(old_request.id()).unwrap();
        let new = Arc::new(Pending::open());
        let (new_request, mut unaffected, _, _) = new.register(empty_invocation()).unwrap();
        old.close("plugin worker exited");
        match failed.recv().await {
            Some(WorkerStreamItem::Result(Err(error))) => {
                assert!(error.to_string().contains("worker exited"));
            }
            other => panic!("expected failed result, got {other:?}"),
        }
        assert!(active.changed().await.is_err());
        assert!(old.authorization(old_request.id()).is_none());
        assert!(old.register(empty_invocation()).is_err());
        assert!(new.authorization(new_request.id()).is_some());
        assert!(unaffected.try_recv().is_err());
    }

    #[test]
    fn invoke_handles_are_taken_before_the_request_is_sent() {
        let pending = Arc::new(Pending::open());
        let (guard, mut receiver, items, active) = pending.register(empty_invocation()).unwrap();
        pending.remove(guard.id());
        let _ = items.try_send(WorkerStreamItem::Result(Ok(
            serde_json::json!({"ok": true}),
        )));
        drop(active);
        let Some(WorkerStreamItem::Result(Ok(value))) = receiver.try_recv().ok() else {
            panic!("pre-send handles must still receive the worker result");
        };
        assert_eq!(value["ok"], true);
        drop(guard);
    }

    #[tokio::test]
    async fn register_rejects_more_than_the_active_invocation_budget() {
        let pending = Arc::new(Pending::open());
        let mut guards = Vec::new();
        for _ in 0..MAX_ACTIVE_INVOCATIONS {
            guards.push(pending.register(empty_invocation()).unwrap());
        }
        let overflow = pending.register(empty_invocation());
        assert!(overflow.is_err(), "a burst must be capped");
        drop(guards);
    }
}
