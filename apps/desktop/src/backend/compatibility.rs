//! Compatibility stubs for metadata RPCs that Cursor expects to exist.

use axum::{
    body::Body,
    extract::Extension,
    http::{Request, Response},
};
use prost::Message;

use crate::{Result, backend::proxy::CursorProxy, protocol::proto::agent::v1 as agent};

#[derive(Clone, Copy, PartialEq, Message)]
struct EmptyResponse {}

pub async fn available_docs(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    super::local_or_forward(proxy, request, || {
        Ok(super::proto_response(EmptyResponse {}))
    })
    .await
}

pub async fn effective_user_plugins(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    super::local_or_forward(proxy, request, || {
        Ok(super::proto_response(EmptyResponse {}))
    })
    .await
}

pub async fn user_privacy_mode(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    super::local_or_forward(proxy, request, || {
        Ok(super::proto_response(EmptyResponse {}))
    })
    .await
}

pub async fn update_conversation_metadata(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    super::local_or_forward(proxy, request, || {
        Ok(super::proto_response(
            agent::UpdateConversationMetadataResponse {},
        ))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Bytes, to_bytes},
        http::header,
    };
    use futures_util::stream;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test]
    async fn empty_unary_mock_is_bare_protobuf_without_connect_frame() {
        let response = super::super::proto_response(EmptyResponse {});
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn local_response_consumes_request_body_before_replying() {
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let body = Body::from_stream(stream::once(async move {
            observed.store(true, Ordering::SeqCst);
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"request"))
        }));
        super::super::consume_body(Request::new(body))
            .await
            .unwrap();
        assert!(polled.load(Ordering::SeqCst));
    }
}
