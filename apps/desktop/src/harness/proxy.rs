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

/// The proxy's lifecycle. `url()` and `port()` report nothing once the task
/// has ended, so a crashed proxy never looks alive.
#[derive(Default)]
pub struct ProxyRuntime {
    url: Option<String>,
    port: Option<u16>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl ProxyRuntime {
    pub fn running(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    pub fn url(&self) -> Option<String> {
        self.running().then(|| self.url.clone()).flatten()
    }

    pub fn port(&self) -> Option<u16> {
        self.running().then_some(self.port).flatten()
    }

    /// Starts the proxy on `requested_port`, falling back to a free port when
    /// it is taken. Returns the URL Cursor needs and the port actually bound.
    pub async fn start(
        &mut self,
        routing: Routing,
        ca: LoadedCa,
        requested_port: u16,
    ) -> Result<(String, u16)> {
        if let Some(url) = self.url() {
            return Ok((url, self.port.unwrap_or_default()));
        }
        let listener = bind_listener(requested_port).await?;
        let address = listener.local_addr()?;
        let (stop, done) = oneshot::channel();
        let authority = RcgenAuthority::new(ca.issuer, 1_000, aws_lc_rs::default_provider());
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
        self.stop = Some(stop);
        self.url = Some(format!("http://{address}"));
        self.port = Some(address.port());
        self.task = Some(tokio::spawn(async move {
            if let Err(error) = proxy.start().await {
                tracing::error!(%error, "the Cursor proxy stopped unexpectedly");
            }
        }));
        Ok((self.url.clone().expect("just set"), address.port()))
    }

    /// Stops the proxy and reclaims its task, even if open connections prevent
    /// a graceful shutdown. Dropping a live task would leak its listening port.
    pub async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
        }
        self.url = None;
        self.port = None;
    }
}

impl Drop for ProxyRuntime {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(task) = self.task.take() {
            task.abort();
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
    pub fn set_backend_addr(&self, addr: SocketAddr) {
        *self.backend.write() = Some(addr);
    }

    fn backend(&self) -> Option<SocketAddr> {
        self.backend.read().filter(|addr| addr.ip().is_loopback())
    }

    pub fn has_backend(&self) -> bool {
        self.backend().is_some()
    }

    fn routes(&self, host: &str, path: &str) -> Option<SocketAddr> {
        (is_cursor_host(host) && is_local_path(path))
            .then(|| self.backend())
            .flatten()
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
            let Some(upstream) = original.to_string().parse().ok() else {
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
    matches!(
        path,
        "/aiserver.v1.InferenceService/RunInferenceSSE"
            | "/aiserver.v1.BidiService/BidiAppend"
            | "/aiserver.v1.AiService/AvailableDocs"
            | "/aiserver.v1.DashboardService/GetEffectiveUserPlugins"
            | "/aiserver.v1.DashboardService/GetUserPrivacyMode"
            | "/agent.v1.AgentService/UpdateConversationMetadata"
            | "/aiserver.v1.AiService/GetServerConfig"
            | "/aiserver.v1.ServerConfigService/GetServerConfig"
            | "/aiserver.v1.AiService/AvailableModels"
            | "/agent.v1.AgentService/GetUsableModels"
            | "/aiserver.v1.AiService/GetUsableModels"
            | "/agent.v1.AgentService/GetDefaultModelForCli"
            | "/aiserver.v1.AiService/GetDefaultModelForCli"
            | "/aiserver.v1.AiService/GetDefaultModel"
            | "/aiserver.v1.AiService/GetDefaultModelNudgeData"
            | "/aiserver.v1.AuthService/GetEmail"
            | "/aiserver.v1.AuthService/GetUserMeta"
            | "/aiserver.v1.DashboardService/GetMe"
            | "/aiserver.v1.DashboardService/GetTeams"
            | "/aiserver.v1.DashboardService/GetUserProfile"
            | "/aiserver.v1.DashboardService/GetCurrentPeriodUsage"
            | "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants"
            | "/aiserver.v1.AiService/KnowledgeBaseAdd"
            | "/aiserver.v1.AiService/KnowledgeBaseList"
            | "/aiserver.v1.AiService/KnowledgeBaseUpdate"
            | "/aiserver.v1.AiService/KnowledgeBaseRemove"
            | "/aiserver.v1.AiService/WriteGitCommitMessage"
            | "/aiserver.v1.NetworkService/IsConnected"
            | "/aiserver.v1.AnalyticsService/BootstrapStatsig"
            | "/auth/full_stripe_profile"
            | "/auth/stripe_profile"
    )
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

        let (url, port) = runtime
            .start(Routing::default(), test_ca(dir.path()), 0)
            .await
            .unwrap();
        assert!(port != 0);
        assert_eq!(runtime.url().as_deref(), Some(url.as_str()));
        assert_eq!(runtime.port(), Some(port));

        let (again, again_port) = runtime
            .start(Routing::default(), test_ca(dir.path()), 0)
            .await
            .unwrap();
        assert_eq!((again.as_str(), again_port), (url.as_str(), port));

        runtime.stop().await;
        assert_eq!(runtime.url(), None);
        assert_eq!(runtime.port(), None);
        assert!(!runtime.running());
        let listener = bind_again(port).await;
        drop(listener);
    }

    #[tokio::test]
    async fn a_taken_port_falls_back_to_a_free_one() {
        let dir = tempfile::tempdir().unwrap();
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = occupied.local_addr().unwrap().port();
        let mut runtime = ProxyRuntime::default();

        let (_, port) = runtime
            .start(Routing::default(), test_ca(dir.path()), taken)
            .await
            .unwrap();

        assert_ne!(port, taken);
        runtime.stop().await;
    }

    #[tokio::test]
    async fn dropping_releases_the_listening_port() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = ProxyRuntime::default();
        let (_, port) = runtime
            .start(Routing::default(), test_ca(dir.path()), 0)
            .await
            .unwrap();

        drop(runtime);
        tokio::task::yield_now().await;

        let listener = bind_again(port).await;
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
