//! The HTTP proxy Cursor is pointed at.
//!
//! It decrypts only `*.cursor.sh` (every other host stays an opaque tunnel),
//! and routes a request to the local backend only when one is configured and
//! the path is on the allowlist. Everything else is forwarded to Cursor
//! untouched, so an app with no backend yet still leaves Cursor fully working.

use std::{net::SocketAddr, sync::Arc};

use hudsucker::{
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
    certificate_authority::RcgenAuthority,
    hyper::{Request, Response, StatusCode, Uri},
    rustls::crypto::aws_lc_rs,
};
use parking_lot::RwLock;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

use super::{Error, Result, ca::LoadedCa};

/// The header the local backend reads to know where the request was going.
pub const UPSTREAM_URL_HEADER: &str = "x-server-upstream-url";

/// Leaf certificates hudsucker may keep cached per host. Cursor opens many
/// connections to a handful of `*.cursor.sh` hosts.
const RCGEN_CACHE_SIZE: u64 = 1_000;
/// How long a proxy task may take to finish after being asked to stop before
/// it is aborted.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Where a freshly started proxy listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedProxy {
    /// The `http://host:port` value Cursor's `http.proxy` setting needs.
    pub url: String,
    /// The port actually bound, which may differ from the requested one.
    pub port: u16,
}

/// The proxy's lifecycle. The address and the task live in one value, so a
/// stopped proxy cannot report a stale address, and a crashed task cannot look
/// alive.
#[derive(Default)]
pub struct ProxyRuntime {
    listener: Option<LiveProxy>,
}

struct LiveProxy {
    started: StartedProxy,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl ProxyRuntime {
    /// Whether the proxy task is still running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.listener
            .as_ref()
            .is_some_and(|live| !live.task.is_finished())
    }

    /// The proxy's URL, present only while it is running.
    #[must_use]
    pub fn url(&self) -> Option<String> {
        self.live().map(|live| live.started.url.clone())
    }

    /// The port actually bound, present only while the proxy is running.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.live().map(|live| live.started.port)
    }

    fn live(&self) -> Option<&LiveProxy> {
        self.listener
            .as_ref()
            .filter(|live| !live.task.is_finished())
    }

    /// Starts the proxy on `requested_port`, falling back to a free port when
    /// it is taken. Starting an already running proxy returns its current
    /// address untouched.
    ///
    /// # Errors
    ///
    /// Fails when no loopback port can be bound or the proxy cannot be built
    /// with the given CA.
    pub async fn start(
        &mut self,
        routing: Routing,
        ca: LoadedCa,
        requested_port: u16,
    ) -> Result<StartedProxy> {
        if let Some(live) = self.live() {
            return Ok(live.started.clone());
        }
        let listener = bind_listener(requested_port).await?;
        let address = listener.local_addr()?;
        let (stop, done) = oneshot::channel();
        let authority = RcgenAuthority::new(
            ca.into_issuer(),
            RCGEN_CACHE_SIZE,
            aws_lc_rs::default_provider(),
        );
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(authority)
            .with_rustls_connector(aws_lc_rs::default_provider())
            .with_http_handler(CursorRelay { routing })
            .with_graceful_shutdown(async move {
                let _ = done.await;
            })
            .build()
            .map_err(|error| Error::Proxy(format!("build the Cursor proxy: {error}")))?;
        let started = StartedProxy {
            url: format!("http://{address}"),
            port: address.port(),
        };
        let task = tokio::spawn(async move {
            if let Err(error) = proxy.start().await {
                tracing::error!(%error, "the Cursor proxy stopped unexpectedly");
            }
        });
        self.listener = Some(LiveProxy {
            started: started.clone(),
            stop,
            task,
        });
        Ok(started)
    }

    /// Stops the proxy and reclaims its task, even if open connections prevent
    /// a graceful shutdown. Dropping a live task would leak its listening port.
    pub async fn stop(&mut self) {
        let Some(LiveProxy {
            stop,
            mut task,
            started: _,
        }) = self.listener.take()
        else {
            return;
        };
        let _ = stop.send(());
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for ProxyRuntime {
    fn drop(&mut self) {
        if let Some(live) = self.listener.take() {
            live.task.abort();
        }
    }
}

async fn bind_listener(requested_port: u16) -> Result<TcpListener> {
    let requested = SocketAddr::from(([127, 0, 0, 1], requested_port));
    match TcpListener::bind(requested).await {
        Ok(listener) => Ok(listener),
        Err(error) if requested_port != 0 => {
            tracing::warn!(%requested, %error, "the configured proxy port is unavailable; using a free one");
            Ok(TcpListener::bind("127.0.0.1:0").await?)
        }
        Err(error) => Err(error.into()),
    }
}

/// Which requests go to the local backend. The relay reads this shared value
/// for every request, so a backend can arrive after the proxy has started.
#[derive(Clone, Debug, Default)]
pub struct Routing {
    backend: Arc<RwLock<Option<SocketAddr>>>,
}

impl Routing {
    /// Points the relay at a freshly started local backend. Every request
    /// reads this value, so the backend may arrive after the proxy started.
    pub fn set_backend_addr(&self, addr: SocketAddr) {
        *self.backend.write() = Some(addr);
    }

    /// The backend the relay should use, or `None` when none is connected.
    fn backend(&self) -> Option<SocketAddr> {
        self.backend.read().filter(|addr| addr.ip().is_loopback())
    }

    /// Whether a local backend is connected.
    #[must_use]
    pub fn has_backend(&self) -> bool {
        self.backend().is_some()
    }

    fn routes(&self, host: &str, path: &str) -> Option<SocketAddr> {
        if is_cursor_host(host) && is_local_path(path) {
            self.backend()
        } else {
            None
        }
    }
}

#[derive(Clone)]
struct CursorRelay {
    routing: Routing,
}

impl HttpHandler for CursorRelay {
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "hudsucker's HttpHandler trait declares every hook async"
    )]
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        mut request: Request<Body>,
    ) -> RequestOrResponse {
        let original = request.uri().clone();
        if let Some(backend) = self
            .routing
            .routes(original.host().unwrap_or_default(), original.path())
        {
            let Ok(upstream) = original.to_string().parse() else {
                return bad_gateway("the original Cursor URL is not a valid header");
            };
            let path = original
                .path_and_query()
                .map_or("/", axum::http::uri::PathAndQuery::as_str);
            let Ok(uri) = format!("http://{backend}{path}").parse::<Uri>() else {
                return bad_gateway("could not build the local Cursor backend URL");
            };
            request.headers_mut().insert(UPSTREAM_URL_HEADER, upstream);
            *request.uri_mut() = uri;
        }
        request.into()
    }

    /// Only Cursor's own hosts are decrypted; every other connection is
    /// tunneled byte for byte.
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "hudsucker's HttpHandler trait declares every hook async"
    )]
    async fn should_intercept_connect(
        &mut self,
        _ctx: &HttpContext,
        request: &Request<Body>,
    ) -> bool {
        request
            .uri()
            .authority()
            .is_some_and(|authority| is_cursor_host(authority.host()))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "hudsucker's HttpHandler trait declares every hook async"
    )]
    async fn should_intercept_tls(
        &mut self,
        _ctx: &HttpContext,
        hello: hudsucker::rustls::server::ClientHello<'_>,
    ) -> bool {
        hello.server_name().is_some_and(is_cursor_host)
    }
}

fn bad_gateway(message: &'static str) -> RequestOrResponse {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(Body::from(message))
        .expect("a static bad gateway response is valid")
        .into()
}

/// The hosts Cursor talks to for AI, account and dashboard traffic.
pub fn is_cursor_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(host.as_str(), "api2.cursor.sh" | "api3.cursor.sh") || host.ends_with(".cursor.sh")
}

/// The requests a local backend owns once it exists. Adding a path here widens
/// what takeover can serve; every entry needs a local handler behind it.
///
/// `AgentService/Run`, `RunSSE`, and `RunPoll` stay off this list: they serve
/// the backend loop, which needs a local `RunEngine` this app does not have.
/// The agent-host still routes turns that the local loop refuses (unsupported
/// mode or action, harness override, tool restriction) to that loop, so
/// claiming them locally would turn working turns into a 404.
fn is_local_path(path: &str) -> bool {
    crate::backend::provides(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::ca::CaManager;

    fn test_ca(dir: &std::path::Path) -> LoadedCa {
        let ca = CaManager::at(dir.join("ca"));
        ca.initialize().unwrap();
        ca.load().unwrap()
    }

    /// Binds `port` again, retrying while the kernel finishes releasing it.
    ///
    /// `stop` returns when the listener is gone from this process, but another
    /// test binding loopback ports concurrently can still win the race for it,
    /// and a just-released port can stay reserved for a moment.
    async fn bind_again(port: u16) -> TcpListener {
        for _ in 0..50 {
            match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => return listener,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
        panic!("port {port} was never released");
    }

    #[tokio::test]
    async fn starting_twice_returns_the_same_port_and_stopping_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = ProxyRuntime::default();

        let started = runtime
            .start(Routing::default(), test_ca(dir.path()), 0)
            .await
            .unwrap();
        assert!(started.port != 0);
        assert_eq!(runtime.url().as_deref(), Some(started.url.as_str()));
        assert_eq!(runtime.port(), Some(started.port));

        let again = runtime
            .start(Routing::default(), test_ca(dir.path()), 0)
            .await
            .unwrap();
        assert_eq!(again, started);

        runtime.stop().await;
        assert_eq!(runtime.url(), None);
        assert_eq!(runtime.port(), None);
        assert!(!runtime.running());
        let listener = bind_again(started.port).await;
        drop(listener);
    }

    #[tokio::test]
    async fn a_taken_port_falls_back_to_a_free_one() {
        let dir = tempfile::tempdir().unwrap();
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = occupied.local_addr().unwrap().port();
        let mut runtime = ProxyRuntime::default();

        let started = runtime
            .start(Routing::default(), test_ca(dir.path()), taken)
            .await
            .unwrap();

        assert_ne!(started.port, taken);
        runtime.stop().await;
    }

    #[tokio::test]
    async fn dropping_releases_the_listening_port() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = ProxyRuntime::default();
        let started = runtime
            .start(Routing::default(), test_ca(dir.path()), 0)
            .await
            .unwrap();

        drop(runtime);
        tokio::task::yield_now().await;

        let listener = bind_again(started.port).await;
        drop(listener);
    }

    #[test]
    fn only_cursor_hosts_are_cursor_hosts() {
        for host in [
            "api2.cursor.sh",
            "api3.cursor.sh",
            "repo.cursor.sh",
            "API2.CURSOR.SH.",
        ] {
            assert!(is_cursor_host(host), "{host}");
        }
        for host in ["cursor.com", "notcursor.sh", "example.com", ""] {
            assert!(!is_cursor_host(host), "{host}");
        }
    }

    #[test]
    fn a_backendless_takeover_routes_nothing() {
        let routing = Routing::default();
        assert_eq!(
            routing.routes("api2.cursor.sh", "/aiserver.v1.BidiService/BidiAppend"),
            None
        );
    }

    /// The backend loop belongs to Cursor. Running it locally would need a
    /// `RunEngine`; without one, those turns must keep reaching the upstream.
    #[test]
    fn backend_loop_rpcs_stay_with_cursor_upstream() {
        let routing = Routing::default();
        routing.set_backend_addr(SocketAddr::from(([127, 0, 0, 1], 3000)));
        for path in [
            "/agent.v1.AgentService/Run",
            "/agent.v1.AgentService/RunSSE",
            "/agent.v1.AgentService/RunPoll",
        ] {
            assert_eq!(routing.routes("api2.cursor.sh", path), None, "{path}");
        }
    }

    /// The local loop only needs the client-owned inference read/write pair.
    #[test]
    fn the_local_loop_pair_is_routed_locally() {
        let routing = Routing::default();
        let backend = SocketAddr::from(([127, 0, 0, 1], 3000));
        routing.set_backend_addr(backend);
        for path in [
            "/aiserver.v1.InferenceService/RunInferenceSSE",
            "/aiserver.v1.BidiService/BidiAppend",
        ] {
            assert_eq!(
                routing.routes("api2.cursor.sh", path),
                Some(backend),
                "{path}"
            );
        }
    }

    #[test]
    fn a_configured_backend_routes_only_allowlisted_cursor_paths() {
        let routing = Routing::default();
        let backend = SocketAddr::from(([127, 0, 0, 1], 3000));
        routing.set_backend_addr(backend);
        assert_eq!(
            routing.routes("api2.cursor.sh", "/aiserver.v1.BidiService/BidiAppend"),
            Some(backend)
        );
        assert_eq!(
            routing.routes("api2.cursor.sh", "/auth/stripe_profile"),
            Some(backend)
        );
        assert_eq!(
            routing.routes("api2.cursor.sh", "/aiserver.v1.AiService/SomeOtherMethod"),
            None
        );
        assert_eq!(routing.routes("example.com", "/auth/stripe_profile"), None);
    }

    #[test]
    fn a_backend_arriving_after_start_updates_the_shared_route() {
        let routing = Routing::default();
        let relay = CursorRelay {
            routing: routing.clone(),
        };
        assert_eq!(
            relay
                .routing
                .routes("api2.cursor.sh", "/auth/stripe_profile"),
            None
        );

        let backend = SocketAddr::from(([127, 0, 0, 1], 3000));
        routing.set_backend_addr(backend);

        assert_eq!(
            relay
                .routing
                .routes("api2.cursor.sh", "/auth/stripe_profile"),
            Some(backend)
        );
    }

    #[test]
    fn a_non_loopback_backend_is_not_used() {
        let routing = Routing::default();
        routing.set_backend_addr(SocketAddr::from(([192, 0, 2, 1], 3000)));
        assert_eq!(
            routing.routes("api2.cursor.sh", "/auth/stripe_profile"),
            None
        );
    }
}
