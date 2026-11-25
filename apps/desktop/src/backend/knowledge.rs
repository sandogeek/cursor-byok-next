//! Axum handlers for Cursor's local knowledge-base RPCs.

use axum::{
    body::Body,
    extract::Extension,
    http::{Request, Response},
};
use prost::Message;

use crate::{
    Result,
    loop_service::{KnowledgeService, RuleRecord, RuleStore},
    protocol,
};

const LOCAL_ID_PREFIX: &str = "local-";

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseAddRequest {
    #[prost(string, tag = "1")]
    knowledge: String,
    #[prost(string, tag = "2")]
    title: String,
    #[prost(string, tag = "3")]
    git_origin: String,
    #[prost(string, optional, tag = "4")]
    composer_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseAddResponse {
    #[prost(bool, tag = "1")]
    success: bool,
    #[prost(string, tag = "2")]
    id: String,
}

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseListRequest {
    #[prost(int32, optional, tag = "1")]
    limit: Option<i32>,
    #[prost(string, optional, tag = "2")]
    git_origin: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseListResponse {
    #[prost(bool, tag = "1")]
    success: bool,
    #[prost(message, repeated, tag = "2")]
    all_results: Vec<KnowledgeBaseListItem>,
}

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseListItem {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    knowledge: String,
    #[prost(string, tag = "3")]
    title: String,
    #[prost(string, tag = "4")]
    created_at: String,
    #[prost(bool, tag = "5")]
    is_generated: bool,
}

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseUpdateRequest {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    knowledge: String,
    #[prost(string, tag = "3")]
    title: String,
}

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseUpdateResponse {
    #[prost(bool, tag = "1")]
    success: bool,
}

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseRemoveRequest {
    #[prost(string, tag = "1")]
    id: String,
}

#[derive(Clone, PartialEq, Message)]
struct KnowledgeBaseRemoveResponse {
    #[prost(bool, tag = "1")]
    success: bool,
}

/// Stores one rule under a fresh local id and answers with it.
pub async fn add(
    Extension(service): Extension<KnowledgeService>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let body = super::read_body(request.into_body()).await?;
    let message: KnowledgeBaseAddRequest = protocol::decode_unary(&body)?;
    let id = format!("{LOCAL_ID_PREFIX}{}", uuid::Uuid::new_v4());
    let record = RuleRecord {
        id: id.clone(),
        knowledge: message.knowledge,
        title: message.title,
        created_at: now(),
        is_generated: false,
        git_origin: message.git_origin,
    };
    service.run(move |store| store.upsert(&record)).await?;
    Ok(super::proto_response(&KnowledgeBaseAddResponse {
        success: true,
        id,
    }))
}

/// Lists stored rules newest first, filtered by git origin and truncated to the requested limit.
pub async fn list(
    Extension(service): Extension<KnowledgeService>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let body = super::read_body(request.into_body()).await?;
    let message: KnowledgeBaseListRequest = protocol::decode_unary(&body)?;
    let mut records = service.run(RuleStore::list).await?;
    if let Some(git_origin) = message.git_origin.filter(|origin| !origin.is_empty()) {
        records.retain(|record| record.git_origin == git_origin);
    }
    if let Some(limit) = message.limit.filter(|limit| *limit >= 0) {
        // A limit that does not fit this platform's `usize` simply keeps every
        // record, which is what a limit larger than the list means.
        if let Ok(limit) = usize::try_from(limit) {
            records.truncate(limit);
        }
    }
    Ok(super::proto_response(&KnowledgeBaseListResponse {
        success: true,
        all_results: records
            .into_iter()
            .map(|record| KnowledgeBaseListItem {
                id: record.id,
                knowledge: record.knowledge,
                title: record.title,
                created_at: record.created_at,
                is_generated: record.is_generated,
            })
            .collect(),
    }))
}

/// Rewrites a rule's text and title; a missing id answers `success: false` rather than failing.
pub async fn update(
    Extension(service): Extension<KnowledgeService>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let body = super::read_body(request.into_body()).await?;
    let message: KnowledgeBaseUpdateRequest = protocol::decode_unary(&body)?;
    let updated = service
        .run(move |store| {
            let Some(mut record) = store.get(&message.id)? else {
                return Ok(false);
            };
            record.knowledge = message.knowledge;
            record.title = message.title;
            store.upsert(&record)?;
            Ok(true)
        })
        .await?;
    Ok(super::proto_response(&KnowledgeBaseUpdateResponse {
        success: updated,
    }))
}

/// Deletes a rule and its metadata entry; removing an absent id still succeeds.
pub async fn remove(
    Extension(service): Extension<KnowledgeService>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let body = super::read_body(request.into_body()).await?;
    let message: KnowledgeBaseRemoveRequest = protocol::decode_unary(&body)?;
    let id = message.id;
    service.run(move |store| store.remove(&id)).await?;
    Ok(super::proto_response(&KnowledgeBaseRemoveResponse {
        success: true,
    }))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
