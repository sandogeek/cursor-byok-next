//! The app's Rust code: what `App.tsx` imports from `cursor-byok/native`. Their
//! TypeScript declarations are generated from these types into
//! `.nsh/types/cursor-byok/native.d.ts`.
//!
//! `harness/` is the port of cursor-byok's `local_app`: it is what makes Cursor
//! willing to talk to this app (proxy, CA, local account). The screens that
//! drive it come next; this file is the typed boundary they will call.

pub mod harness;

use std::sync::Arc;

use nsh_base::theme;
use nsh_core::prelude::*;

use self::harness::{
    CaState as HarnessCa, CursorHarness, Error as HarnessError, HarnessPaths, HarnessStatus,
    IntegrationState as HarnessIntegration,
};

/// The proxy port Cursor is pointed at when this app has never bound one.
/// `0` lets the OS pick, and the chosen port is remembered from then on.
const DEFAULT_PROXY_PORT: u16 = 0;

/// The crate's UI set (`nsh.json` `"rust"`).
#[native_module("cursor-byok/native")]
fn native(registry: &mut Registry) {
    registry
        .register::<Badge>()
        .module::<Format>()
        .class::<Counter>()
        .class::<Harness>();
}

/// Whether the system trusts the local CA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PropEnum)]
pub enum CaState {
    /// No CA yet.
    Missing,
    /// Generated, but not installed into the system's trust store.
    Untrusted,
    /// Trusted: takeover may start.
    Ready,
    /// The pair is incomplete, mismatched or expired.
    Invalid,
}

impl From<HarnessCa> for CaState {
    fn from(value: HarnessCa) -> Self {
        match value {
            HarnessCa::Missing => Self::Missing,
            HarnessCa::Untrusted => Self::Untrusted,
            HarnessCa::Ready => Self::Ready,
            HarnessCa::Invalid => Self::Invalid,
        }
    }
}

/// How much of takeover is working right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PropEnum)]
pub enum IntegrationState {
    Disabled,
    Enabled,
    /// The proxy and Cursor's settings disagree: one of them is stale.
    Degraded,
}

impl From<HarnessIntegration> for IntegrationState {
    fn from(value: HarnessIntegration) -> Self {
        match value {
            HarnessIntegration::Disabled => Self::Disabled,
            HarnessIntegration::Enabled => Self::Enabled,
            HarnessIntegration::Degraded => Self::Degraded,
        }
    }
}

impl From<HarnessStatus> for HarnessState {
    fn from(value: HarnessStatus) -> Self {
        Self {
            platform: value.platform,
            ca: value.ca.into(),
            integration: value.integration.into(),
            settings_applied: value.settings_applied,
            proxy_url: value.proxy_url,
            proxy_port: value.proxy_port,
            ca_install_command: value.ca_install_command,
        }
    }
}

/// The takeover state a screen shows.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct HarnessState {
    /// The OS, as Rust names it (`"macos"`, `"windows"`, `"linux"`).
    pub platform: String,
    /// Whether the system trusts the local CA.
    pub ca: CaState,
    /// Whether Cursor is actually routed through this app.
    pub integration: IntegrationState,
    /// Whether Cursor's settings hold exactly this app's proxy.
    pub settings_applied: bool,
    /// The proxy Cursor should use, present only while it runs.
    pub proxy_url: Option<String>,
    /// The port the proxy bound.
    pub proxy_port: Option<u16>,
    /// The command that trusts the CA; the app cannot run it (it needs sudo).
    pub ca_install_command: Option<String>,
}

/// Cursor takeover: points Cursor at the local proxy, signs the certificates it
/// accepts, and signs it in to a local account.
///
/// Create one and keep it for the app's lifetime: it owns the proxy task.
#[class]
pub struct Harness {
    inner: Arc<CursorHarness>,
}

// A class: `new Harness()`, `await harness.status()`, `await harness.setEnabled(true)`.
#[class]
impl Harness {
    /// Restores the proxy port and the on/off intent of the previous run.
    pub fn new() -> Result<Self, HarnessError> {
        let paths = HarnessPaths::managed(DEFAULT_PROXY_PORT)?;
        Ok(Self {
            inner: Arc::new(CursorHarness::new(paths)),
        })
    }

    /// The current state. Also heals a takeover whose proxy went away.
    pub async fn status(this: Instance<Self>) -> Result<HarnessState, HarnessError> {
        Ok(this
            .with(|harness| harness.inner.clone())
            .status()
            .await
            .into())
    }

    /// Turns takeover on or off. Enabling fails, without touching Cursor, when
    /// the CA is not trusted yet; check `ca` in the status first.
    pub async fn set_enabled(
        this: Instance<Self>,
        enabled: bool,
    ) -> Result<HarnessState, HarnessError> {
        this.with(|harness| harness.inner.clone())
            .set_enabled(enabled)
            .await
            .map(Into::into)
    }

    /// Connects the local service after it starts listening. Existing takeover
    /// starts routing locally and receives its local account at the same time.
    pub async fn set_backend_addr(this: Instance<Self>, addr: String) -> Result<(), HarnessError> {
        let addr = addr.parse().map_err(|error| {
            HarnessError::Config(format!("invalid local backend address: {error}"))
        })?;
        this.with(|harness| harness.inner.clone())
            .set_backend_addr(addr)
            .await
    }

    /// Generates the CA when it is missing. Trusting it is the user's step:
    /// copy `caInstallCommand` into a terminal.
    pub async fn initialize_ca(this: Instance<Self>) -> Result<HarnessState, HarnessError> {
        let harness = this.with(|harness| harness.inner.clone());
        let worker = harness.clone();
        blocking(move || worker.initialize_ca())
            .await
            .map_err(|error| {
                HarnessError::Process(format!("CA initialization task failed: {error}"))
            })??;
        Ok(harness.status().await.into())
    }

    /// Drops the proxy a crashed run left in Cursor's settings. `true` when it
    /// removed something.
    pub async fn cleanup_stale_settings(this: Instance<Self>) -> Result<bool, HarnessError> {
        this.with(|harness| harness.inner.clone())
            .cleanup_stale_settings()
            .await
    }

    /// Quits Cursor so a freshly written proxy setting applies at once. `false`
    /// when Cursor was not running.
    pub async fn quit_cursor() -> Result<bool, HarnessError> {
        harness::quit_cursor().await
    }
}

// A component: `<Badge label="…" onPress={…} />`. The function's doc comment
// is the component's in TS, the fields' its props'.
/// The props of [`Badge`].
#[derive(Debug, Clone, PartialEq, Props)]
pub struct BadgeProps {
    /// The text.
    pub label: Prop<SharedString>,
}

/// The events of a [`Badge`].
#[derive(Debug, Clone, PartialEq, Event)]
pub enum BadgeEvent {
    /// The badge was pressed.
    Press,
}

/// A pill with a label; pressing it reports `onPress`.
#[component]
pub fn Badge(props: BadgeProps, cx: Scope<BadgeEvent>) -> impl IntoElement {
    view()
        .px_4()
        .py_2()
        .rounded_full()
        .bg(move || theme().primary)
        .active(|s| s.bg(move || theme().primary.opacity(0.9)))
        .on_press(move || cx.emit(BadgeEvent::Press))
        .child(text(props.label).text_color(move || theme().primary_foreground))
}

/// Text helpers.
#[module]
pub struct Format;

// A module: `Format.times(3)`. The struct's doc comment is the module's in
// TS (`#[module]` on it and on this block).
#[module]
impl Format {
    /// "once", "twice", "3 times".
    pub fn times(count: u32) -> String {
        match count {
            1 => "once".to_owned(),
            2 => "twice".to_owned(),
            n => format!("{n} times"),
        }
    }
}

/// A count kept in Rust.
#[derive(Default)]
#[class]
pub struct Counter {
    count: u32,
}

// A class: `new Counter()`, `counter.increment()`, `counter.count`.
#[class]
impl Counter {
    /// A counter at 0.
    pub fn new() -> Self {
        Counter { count: 0 }
    }

    /// Adds one; the new count.
    pub fn increment(&mut self) -> u32 {
        self.count += 1;
        self.count
    }

    /// The count.
    #[getter]
    pub fn count(&self) -> u32 {
        self.count
    }
}
