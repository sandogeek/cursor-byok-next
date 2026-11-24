//! Local HTTP backend Cursor talks to after takeover.

mod account;
mod analytics;
mod catalog;
mod compatibility;
mod knowledge;
mod proxy;
mod server_config;

use std::net::SocketAddr;

/// Most a buffered Cursor RPC may carry. Cursor's own `BidiAppend` and
/// knowledge-base bodies are tens of kilobytes; anything near this is either a
/// bug or an attempt to exhaust memory before the local loop sees the frames.
pub(crate) const MAX_BUFFERED_BYTES: usize = 64 * 1024 * 1024;

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::Extension,
    http::{HeaderValue, Request, Response, header},
    routing::{get, post},
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower_http::decompression::RequestDecompressionLayer;

use crate::{
    Result,
    calls::CallStore,
    harness,
    loop_service::{InferenceRoute, InferenceService, decode_append},
    plugins::PluginHost,
    protocol::{
        connect,
        proto::{agent::v1 as agent, aiserver::v1 as ai, inference::v1 as inference},
    },
};

pub use knowledge::KnowledgeService;

pub struct Backend {
    addr: SocketAddr,
    _task: JoinHandle<()>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Backend")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl Backend {
    pub async fn start(plugins: PluginHost, calls: CallStore) -> Result<Self> {
        let knowledge = KnowledgeService::with_root(crate::storage::data_subdir("rules")?)?;
        let inference = InferenceService::new(plugins.clone(), knowledge.clone(), calls);
        let proxy = proxy::CursorProxy::default();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = router(plugins, inference, proxy, knowledge);
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, "local loop backend stopped unexpectedly");
            }
        });
        Ok(Self { addr, _task: task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// Paths served by [`inference_routes`], in route order.
pub(crate) const INFERENCE_PATHS: [&str; 2] = [
    "/aiserver.v1.InferenceService/RunInferenceSSE",
    "/aiserver.v1.BidiService/BidiAppend",
];

/// Paths served by [`account_routes`], in route order.
pub(crate) const ACCOUNT_PATHS: [&str; 9] = [
    "/aiserver.v1.AuthService/GetEmail",
    "/aiserver.v1.AuthService/GetUserMeta",
    "/aiserver.v1.DashboardService/GetMe",
    "/aiserver.v1.DashboardService/GetTeams",
    "/aiserver.v1.DashboardService/GetUserProfile",
    "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
    "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants",
    "/auth/full_stripe_profile",
    "/auth/stripe_profile",
];

/// Paths served by [`catalog_routes`], in route order.
pub(crate) const CATALOG_PATHS: [&str; 7] = [
    "/aiserver.v1.AiService/AvailableModels",
    "/agent.v1.AgentService/GetUsableModels",
    "/aiserver.v1.AiService/GetUsableModels",
    "/agent.v1.AgentService/GetDefaultModelForCli",
    "/aiserver.v1.AiService/GetDefaultModelForCli",
    "/aiserver.v1.AiService/GetDefaultModel",
    "/aiserver.v1.AiService/GetDefaultModelNudgeData",
];

/// Paths served by [`knowledge_routes`], in route order.
pub(crate) const KNOWLEDGE_PATHS: [&str; 4] = [
    "/aiserver.v1.AiService/KnowledgeBaseAdd",
    "/aiserver.v1.AiService/KnowledgeBaseList",
    "/aiserver.v1.AiService/KnowledgeBaseUpdate",
    "/aiserver.v1.AiService/KnowledgeBaseRemove",
];

/// Paths served by [`misc_routes`], in route order.
pub(crate) const MISC_PATHS: [&str; 9] = [
    "/aiserver.v1.AiService/AvailableDocs",
    "/aiserver.v1.DashboardService/GetEffectiveUserPlugins",
    "/aiserver.v1.DashboardService/GetUserPrivacyMode",
    "/agent.v1.AgentService/UpdateConversationMetadata",
    "/aiserver.v1.AiService/GetServerConfig",
    "/aiserver.v1.ServerConfigService/GetServerConfig",
    "/aiserver.v1.AiService/WriteGitCommitMessage",
    "/aiserver.v1.NetworkService/IsConnected",
    analytics::BOOTSTRAP_STATSIG_PATH,
];

/// Every Cursor RPC this backend answers. The MITM layer rewrites exactly these
/// paths to the local port; a route added below without an entry here would
/// still reach Cursor's own host.
///
/// Each path lives in the group constant next to the handler that serves it, so
/// the route table and this list cannot drift apart.
pub(crate) fn provided_paths() -> impl Iterator<Item = &'static str> {
    const GROUPS: [&[&str]; 5] = [
        &INFERENCE_PATHS,
        &ACCOUNT_PATHS,
        &CATALOG_PATHS,
        &KNOWLEDGE_PATHS,
        &MISC_PATHS,
    ];
    GROUPS.into_iter().flatten().copied()
}

/// Whether this backend owns `path`, which is exactly what the MITM layer checks
/// before rewriting a request to the local port.
pub(crate) fn provides(path: &str) -> bool {
    provided_paths().any(|owned| owned == path)
}

fn router(
    plugins: PluginHost,
    inference: InferenceService,
    proxy: proxy::CursorProxy,
    knowledge: KnowledgeService,
) -> Router {
    inference_routes(Router::new())
        .merge(account_routes())
        .merge(catalog_routes())
        .merge(knowledge_routes())
        .merge(misc_routes())
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(RequestDecompressionLayer::new())
        .layer(Extension(proxy))
        .layer(Extension(knowledge))
        .layer(Extension(inference))
        .layer(Extension(plugins))
}

fn inference_routes(router: Router) -> Router {
    router
        .route(INFERENCE_PATHS[0], post(run_inference_sse))
        .route(INFERENCE_PATHS[1], post(bidi_append))
}

fn account_routes() -> Router {
    Router::new()
        .route(ACCOUNT_PATHS[0], post(account::get_email))
        .route(ACCOUNT_PATHS[1], post(account::get_user_meta))
        .route(ACCOUNT_PATHS[2], post(account::get_me))
        .route(ACCOUNT_PATHS[3], post(account::get_teams))
        .route(ACCOUNT_PATHS[4], post(account::get_user_profile))
        .route(ACCOUNT_PATHS[5], post(account::current_period_usage))
        .route(ACCOUNT_PATHS[6], post(account::usage_limit_status))
        .route(ACCOUNT_PATHS[7], get(account::stripe_profile))
        .route(ACCOUNT_PATHS[8], get(account::stripe_profile))
}

fn catalog_routes() -> Router {
    Router::new()
        .route(CATALOG_PATHS[0], post(catalog::available_models))
        .route(CATALOG_PATHS[1], post(catalog::usable_models))
        .route(CATALOG_PATHS[2], post(catalog::usable_models))
        .route(CATALOG_PATHS[3], post(catalog::default_model_for_cli))
        .route(CATALOG_PATHS[4], post(catalog::default_model_for_cli))
        .route(CATALOG_PATHS[5], post(catalog::default_model))
        .route(CATALOG_PATHS[6], post(catalog::default_model_nudge))
}

fn knowledge_routes() -> Router {
    Router::new()
        .route(KNOWLEDGE_PATHS[0], post(knowledge::add))
        .route(KNOWLEDGE_PATHS[1], post(knowledge::list))
        .route(KNOWLEDGE_PATHS[2], post(knowledge::update))
        .route(KNOWLEDGE_PATHS[3], post(knowledge::remove))
}

fn misc_routes() -> Router {
    Router::new()
        .route(MISC_PATHS[0], post(compatibility::empty_metadata))
        .route(MISC_PATHS[1], post(compatibility::empty_metadata))
        .route(MISC_PATHS[2], post(compatibility::empty_metadata))
        .route(
            MISC_PATHS[3],
            post(compatibility::update_conversation_metadata),
        )
        .route(MISC_PATHS[4], post(server_config::get))
        .route(MISC_PATHS[5], post(server_config::get))
        .route(MISC_PATHS[6], post(proxy::forward))
        .route(MISC_PATHS[7], post(is_connected))
        .route(MISC_PATHS[8], post(analytics::bootstrap_statsig))
}

async fn is_connected() -> Result<Response<Body>> {
    Ok(proto_bytes_response(connect::encode_message(
        &ai::IsConnectedResponse {},
    )?))
}

async fn run_inference_sse(
    Extension(inference_service): Extension<InferenceService>,
    Extension(proxy): Extension<proxy::CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = buffered(request).await?;
    let request: agent::BidiRequestId = connect::decode_unary(&body)?;
    let request_id = request.request_id;
    match inference_service.wait_route(&request_id).await {
        InferenceRoute::Upstream => {
            proxy::forward_request(&proxy, Request::from_parts(parts, Body::from(body))).await
        }
        InferenceRoute::Local(run) => {
            let (mut receiver, subscription) = inference_service.subscribe(request_id, run);
            let stream = async_stream::stream! {
                let _subscription = subscription;
                while let Some(frame) = receiver.recv().await {
                    let complete = connect::decode_frames(&frame)
                        .is_ok_and(|frames| frames.last().is_some_and(connect::Frame::is_end_stream));
                    yield Ok::<Bytes, std::convert::Infallible>(frame);
                    if complete {
                        return;
                    }
                }
            };
            let mut response = Response::new(Body::from_stream(stream));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            response
                .headers_mut()
                .insert("connect-protocol-version", HeaderValue::from_static("1"));
            Ok(response)
        }
    }
}

async fn bidi_append(
    Extension(proxy): Extension<proxy::CursorProxy>,
    Extension(inference_service): Extension<InferenceService>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = buffered(request).await?;
    let mut request: ai::BidiAppendRequest = connect::decode_unary(&body)?;
    // Owned, so the payload can be moved out of the request below without a
    // copy and without holding a borrow of `request`.
    let request_id = request
        .request_id
        .take()
        .map(|id| id.request_id)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| crate::Error::Protocol("BidiAppend request_id is required".into()))?;
    let payload = if request.data_binary.is_empty() {
        hex::decode(&request.data)
            .map_err(|error| crate::Error::Protocol(format!("invalid BidiAppend hex: {error}")))?
    } else {
        std::mem::take(&mut request.data_binary)
    };
    let current_route = inference_service.route(&request_id);
    let candidate = decode_append(&payload).ok();
    let starts_inference = candidate.as_ref().is_some_and(|message| {
        matches!(
            message.message.as_ref(),
            Some(inference::run_inference_client_message::Message::RunRequest(run))
                if !run.conversation_id.is_empty()
                    && run.requested_model.as_ref().is_some_and(|model| !model.model_id.is_empty())
        )
    });
    if current_route.is_some() || starts_inference {
        let message = candidate
            .ok_or_else(|| crate::Error::Protocol("invalid RunInference client message".into()))?;
        let route = match current_route {
            Some(route) => route,
            None => inference_service.begin(&request_id, &message).await?,
        };
        match route {
            InferenceRoute::Local(run) => {
                run.append(request.append_seqno, message).await?;
                let mut response = Response::new(Body::empty());
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/proto"),
                );
                return Ok(response);
            }
            InferenceRoute::Upstream => {
                let finished = matches!(
                    message.message,
                    Some(inference::run_inference_client_message::Message::FinishRun(
                        _
                    ))
                );
                let response =
                    proxy::forward_request(&proxy, Request::from_parts(parts, Body::from(body)))
                        .await;
                if finished {
                    inference_service.finish(&request_id);
                }
                return response;
            }
        }
    }
    proxy::forward_request(&proxy, Request::from_parts(parts, Body::from(body))).await
}

pub(crate) async fn buffered(
    request: Request<Body>,
) -> Result<(axum::http::request::Parts, Bytes)> {
    let (parts, body) = request.into_parts();
    Ok((parts, read_body(body).await?))
}

/// Reads a whole request body, refusing anything past [`MAX_BUFFERED_BYTES`].
pub(crate) async fn read_body(body: Body) -> Result<Bytes> {
    to_bytes(body, MAX_BUFFERED_BYTES)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))
}

/// Encodes `message` as an `application/proto` response.
pub(crate) fn proto_response(message: &impl prost::Message) -> Response<Body> {
    proto_bytes_response(message.encode_to_vec())
}

/// Wraps an already-encoded protobuf body as an `application/proto` response.
pub(crate) fn proto_bytes_response(body: impl Into<Bytes>) -> Response<Body> {
    let body = body.into();
    let length = body.len();
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    response
}

/// Resolves a local-vs-forward decision for an account RPC. `local` runs only
/// when the request carries this app's own Cursor token; the body is drained so
/// the client sees a complete request/response pair either way.
pub(crate) async fn local_or_forward(
    proxy: &proxy::CursorProxy,
    request: Request<Body>,
    local: impl FnOnce() -> Response<Body>,
) -> Result<Response<Body>> {
    if harness::request_uses_local_cursor_token(request.headers()) {
        read_body(request.into_body()).await?;
        return Ok(local());
    }
    proxy::forward_request(proxy, request).await
}

#[cfg(test)]
pub(crate) fn router_for_test(
    plugins: PluginHost,
    knowledge: KnowledgeService,
    proxy: proxy::CursorProxy,
) -> Router {
    // The store owns the file for the test's lifetime, so the directory has to
    // outlive this function.
    let directory = tempfile::tempdir().expect("temporary call store").keep();
    let inference = InferenceService::new(
        plugins.clone(),
        knowledge.clone(),
        CallStore::at(directory.join("calls.db")),
    );
    router(plugins, inference, proxy, knowledge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        connect,
        proto::{agent::v1 as agent, aiserver::v1 as ai, inference::v1 as pb},
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use futures_util::StreamExt;
    use prost::Message;
    use tower::ServiceExt;

    fn plugin_id() -> String {
        "plugin:dev.cursorbyoknext.plugins.magpie/magpie/missing/model".into()
    }

    #[test]
    fn every_provided_path_has_a_route_and_no_path_is_listed_twice() {
        let owned: Vec<&str> = provided_paths().collect();
        assert_eq!(owned.len(), 2 + 9 + 7 + 4 + 9, "route count changed");
        for (index, path) in owned.iter().enumerate() {
            assert!(
                path.starts_with('/') && !path.contains("//"),
                "{path} is not a request path"
            );
            assert!(
                !owned[..index].contains(path),
                "{path} is listed twice; `route` would panic on a duplicate"
            );
        }
    }

    #[tokio::test]
    async fn every_provided_path_is_actually_served_locally() {
        let root = tempfile::tempdir().unwrap();
        let plugins = PluginHost::for_test(root.path()).unwrap();
        let knowledge = KnowledgeService::with_root(root.path().join("rules")).unwrap();
        let router = router_for_test(plugins, knowledge, proxy::CursorProxy::default());
        for path in provided_paths() {
            // `DELETE` matches no route, so axum answers 405 for a registered path
            // and 404 for an unregistered one without running any handler.
            let response = router
                .clone()
                .oneshot(Request::delete(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{path} is advertised to the MITM layer but has no local route"
            );
        }
    }

    async fn append(
        router: &Router,
        request_id: &str,
        seqno: i64,
        message: pb::RunInferenceClientMessage,
    ) {
        let payload = message.encode_to_vec();
        let body = connect::encode_message(&ai::BidiAppendRequest {
            data: String::new(),
            request_id: Some(ai::BidiRequestId {
                request_id: request_id.into(),
            }),
            append_seqno: seqno,
            data_binary: payload,
        })
        .unwrap();
        let response = router
            .clone()
            .oneshot(
                Request::post("/aiserver.v1.BidiService/BidiAppend")
                    .header(header::CONTENT_TYPE, "application/proto")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn plugin_model_starts_a_local_inference_run() {
        let root = tempfile::tempdir().unwrap();
        let plugins = PluginHost::for_test(root.path()).unwrap();
        let knowledge = KnowledgeService::with_root(root.path().join("rules")).unwrap();
        let router = router_for_test(plugins, knowledge, proxy::CursorProxy::default());
        append(
            &router,
            "inference-test",
            0,
            pb::RunInferenceClientMessage {
                message: Some(pb::run_inference_client_message::Message::RunRequest(
                    pb::RunInferenceRunRequest {
                        conversation_id: "conversation-test".into(),
                        requested_model: Some(pb::InferenceRequestedModel {
                            model_id: plugin_id(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
            },
        )
        .await;
        let response = router
            .clone()
            .oneshot(
                Request::post("/aiserver.v1.InferenceService/RunInferenceSSE")
                    .header(header::CONTENT_TYPE, "application/proto")
                    .body(Body::from(
                        connect::encode_message(&agent::BidiRequestId {
                            request_id: "inference-test".into(),
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .expect("run ready must arrive")
            .expect("stream started")
            .unwrap();
        let frames = connect::decode_frames(&frame).unwrap();
        let ready = pb::RunInferenceServerMessage::decode(frames[0].payload.clone()).unwrap();
        assert!(matches!(
            ready.message,
            Some(pb::run_inference_server_message::Message::RunReady(_))
        ));
    }
}
