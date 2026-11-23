//! Local HTTP backend Cursor talks to after takeover.

mod account;
mod analytics;
mod catalog;
mod compatibility;
mod knowledge;
mod proxy;
mod server_config;

use std::net::SocketAddr;

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::Extension,
    http::{HeaderValue, Request, Response, StatusCode, header},
    routing::{get, post},
};
use bytes::{BufMut, BytesMut};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower_http::decompression::RequestDecompressionLayer;

use crate::{
    Error, Result,
    calls::CallStore,
    harness,
    loop_service::{InferenceRoute, InferenceService, decode_append},
    plugins::PluginHost,
    protocol::{
        connect,
        proto::{agent::v1 as agent, aiserver::v1 as ai, inference},
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
        let knowledge = KnowledgeService::with_root(crate::harness::data_dir()?.join("rules"))?;
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
        .route(
            "/aiserver.v1.InferenceService/RunInferenceSSE",
            post(run_inference_sse),
        )
        .route("/aiserver.v1.BidiService/BidiAppend", post(bidi_append))
}

fn account_routes() -> Router {
    Router::new()
        .route(
            "/aiserver.v1.AuthService/GetEmail",
            post(account::get_email),
        )
        .route(
            "/aiserver.v1.AuthService/GetUserMeta",
            post(account::get_user_meta),
        )
        .route("/aiserver.v1.DashboardService/GetMe", post(account::get_me))
        .route(
            "/aiserver.v1.DashboardService/GetTeams",
            post(account::get_teams),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserProfile",
            post(account::get_user_profile),
        )
        .route(
            "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            post(account::current_period_usage),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants",
            post(account::usage_limit_status),
        )
        .route("/auth/full_stripe_profile", get(account::stripe_profile))
        .route("/auth/stripe_profile", get(account::stripe_profile))
}

fn catalog_routes() -> Router {
    Router::new()
        .route(
            "/aiserver.v1.AiService/AvailableModels",
            post(catalog::available_models),
        )
        .route(
            "/agent.v1.AgentService/GetUsableModels",
            post(catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/GetUsableModels",
            post(catalog::usable_models),
        )
        .route(
            "/agent.v1.AgentService/GetDefaultModelForCli",
            post(catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelForCli",
            post(catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModel",
            post(catalog::default_model),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelNudgeData",
            post(catalog::default_model_nudge),
        )
}

fn knowledge_routes() -> Router {
    Router::new()
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseAdd",
            post(knowledge::add),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseList",
            post(knowledge::list),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseUpdate",
            post(knowledge::update),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseRemove",
            post(knowledge::remove),
        )
}

fn misc_routes() -> Router {
    Router::new()
        .route(
            "/aiserver.v1.AiService/AvailableDocs",
            post(compatibility::available_docs),
        )
        .route(
            "/aiserver.v1.DashboardService/GetEffectiveUserPlugins",
            post(compatibility::effective_user_plugins),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserPrivacyMode",
            post(compatibility::user_privacy_mode),
        )
        .route(
            "/agent.v1.AgentService/UpdateConversationMetadata",
            post(compatibility::update_conversation_metadata),
        )
        .route(
            "/aiserver.v1.AiService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.ServerConfigService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.AiService/WriteGitCommitMessage",
            post(forward_commit_message),
        )
        .route(
            "/aiserver.v1.NetworkService/IsConnected",
            post(is_connected),
        )
        .route(
            analytics::BOOTSTRAP_STATSIG_PATH,
            post(analytics::bootstrap_statsig),
        )
}

async fn is_connected() -> Result<Response<Body>> {
    let payload = connect::encode_message(&ai::IsConnectedResponse {})?;
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

async fn forward_commit_message(
    Extension(proxy): Extension<proxy::CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    proxy::forward(Extension(proxy), request).await
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
            proxy::forward(
                Extension(proxy),
                Request::from_parts(parts, Body::from(body)),
            )
            .await
        }
        InferenceRoute::Local(run) => {
            let (mut receiver, subscription) = inference_service.subscribe(request_id, &run);
            let stream = async_stream::stream! {
                let _subscription = subscription;
                while let Some(frame) = receiver.recv().await {
                    let complete = frame.first().is_some_and(|flag| flag & connect::END_STREAM_FLAG != 0);
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
    let request: ai::BidiAppendRequest = connect::decode_unary(&body)?;
    let request_id = request
        .request_id
        .as_ref()
        .map(|id| id.request_id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| crate::Error::Protocol("BidiAppend request_id is required".into()))?;
    let payload = if request.data_binary.is_empty() {
        hex::decode(&request.data)
            .map_err(|error| crate::Error::Protocol(format!("invalid BidiAppend hex: {error}")))?
    } else {
        request.data_binary.clone()
    };
    let current_route = inference_service.route(request_id);
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
            None => inference_service.begin(request_id, &message).await?,
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
                let response = proxy::forward(
                    Extension(proxy),
                    Request::from_parts(parts, Body::from(body)),
                )
                .await;
                if finished {
                    inference_service.finish(request_id);
                }
                return response;
            }
        }
    }
    proxy::forward(
        Extension(proxy),
        Request::from_parts(parts, Body::from(body)),
    )
    .await
}

async fn buffered(request: Request<Body>) -> Result<(axum::http::request::Parts, Bytes)> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    Ok((parts, body))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "every call site builds the response inline, and prost's encode_to_vec takes &self"
)]
pub(crate) fn proto_response(message: impl prost::Message) -> Response<Body> {
    let body = message.encode_to_vec();
    let length = body.len();
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).expect("body length is valid"),
    );
    response
}

/// Whether an upstream Connect unary body carries the 5-byte envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    Enveloped,
    Bare,
}

impl Framing {
    pub(crate) fn is_enveloped(self) -> bool {
        self == Self::Enveloped
    }
}

/// Splits an optional Connect unary envelope off `body`.
pub(crate) fn unary_payload<'a>(body: &'a [u8], label: &str) -> Result<(Framing, &'a [u8])> {
    if body.len() < 5 {
        return Ok((Framing::Bare, body));
    }
    let flags = body[0];
    let length = usize::try_from(u32::from_be_bytes([body[1], body[2], body[3], body[4]]))
        .unwrap_or(usize::MAX);
    if length != body.len() - 5 {
        return Ok((Framing::Bare, body));
    }
    if flags != 0 {
        return Err(Error::Protocol(format!(
            "cannot patch compressed or terminal {label} frame: flags={flags}"
        )));
    }
    Ok((Framing::Enveloped, &body[5..]))
}

/// Wraps an encoded unary payload in a Connect envelope when `framing` asks for it.
pub(crate) fn encode_unary(payload: Vec<u8>, framing: Framing) -> Result<Bytes> {
    if !framing.is_enveloped() {
        return Ok(Bytes::from(payload));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| Error::Protocol("Connect payload exceeds 4 GiB".into()))?;
    let mut output = BytesMut::with_capacity(5 + payload.len());
    output.put_u8(0);
    output.put_u32(length);
    output.extend_from_slice(&payload);
    Ok(output.freeze())
}

async fn consume_body(request: Request<Body>) -> Result<()> {
    to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    Ok(())
}

pub(crate) async fn local_or_forward(
    proxy: proxy::CursorProxy,
    request: Request<Body>,
    local: impl FnOnce() -> Result<Response<Body>>,
) -> Result<Response<Body>> {
    if harness::request_uses_local_cursor_token(request.headers()) {
        consume_body(request).await?;
        return local();
    }
    proxy::forward(Extension(proxy), request).await
}

#[cfg(test)]
pub(crate) fn router_for_test(
    plugins: PluginHost,
    knowledge: KnowledgeService,
    proxy: proxy::CursorProxy,
) -> Router {
    let directory = tempfile::tempdir().expect("temporary call store");
    let inference = InferenceService::new(
        plugins.clone(),
        knowledge.clone(),
        CallStore::at(directory.path().join("calls.db")),
    );
    std::mem::forget(directory);
    router(plugins, inference, proxy, knowledge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        connect,
        proto::{agent::v1 as agent, aiserver::v1 as ai, inference as pb},
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
        let ready = pb::RunInferenceServerMessage::decode(frames[0].1.clone()).unwrap();
        assert!(matches!(
            ready.message,
            Some(pb::run_inference_server_message::Message::RunReady(_))
        ));
    }
}
