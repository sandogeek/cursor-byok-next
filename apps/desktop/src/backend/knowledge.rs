//! Local Cursor user rules, stored as markdown plus a JSON sidecar.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    body::Body,
    extract::Extension,
    http::{Request, Response},
};
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    protocol::{connect, proto::agent::v1 as pb},
};

const META_FILE: &str = "meta.json";
const RULE_EXTENSION: &str = "md";
const LOCAL_ID_PREFIX: &str = "local-";

#[derive(Clone, Debug, PartialEq)]
pub struct RuleRecord {
    pub id: String,
    pub knowledge: String,
    pub title: String,
    pub created_at: String,
    pub is_generated: bool,
    pub git_origin: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Meta {
    #[serde(default)]
    rules: BTreeMap<String, RuleMeta>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct RuleMeta {
    #[serde(default)]
    title: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    is_generated: bool,
    #[serde(default)]
    git_origin: String,
}

#[derive(Debug)]
pub struct RuleStore {
    root: PathBuf,
}

impl RuleStore {
    /// Opens (and creates) the rules directory.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created.
    pub fn open(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Every stored rule, newest first. Files whose name is not a valid id are
    /// skipped rather than failing the whole read.
    ///
    /// # Errors
    ///
    /// Fails when the directory or a rule file cannot be read.
    pub fn list(&self) -> Result<Vec<RuleRecord>> {
        let meta = self.read_meta();
        let mut records = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some(RULE_EXTENSION) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if validate_id(id).is_err() {
                continue;
            }
            let knowledge = std::fs::read_to_string(&path)?;
            records.push(assemble(id, knowledge, meta.rules.get(id), &path));
        }
        records.sort_by(|left, right| {
            timestamp(&right.created_at)
                .cmp(&timestamp(&left.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(records)
    }

    /// One stored rule, or `None` when no file exists for `id`.
    ///
    /// # Errors
    ///
    /// Fails when `id` is not a valid rule id or the file cannot be read.
    pub fn get(&self, id: &str) -> Result<Option<RuleRecord>> {
        validate_id(id)?;
        let path = self.rule_path(id);
        let knowledge = match std::fs::read_to_string(&path) {
            Ok(knowledge) => knowledge,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let meta = self.read_meta();
        Ok(Some(assemble(id, knowledge, meta.rules.get(id), &path)))
    }

    /// Writes a rule and its metadata.
    ///
    /// # Errors
    ///
    /// Fails when `record.id` is not a valid rule id, or the rule file or the
    /// metadata index cannot be written.
    pub fn upsert(&self, record: &RuleRecord) -> Result<()> {
        validate_id(&record.id)?;
        crate::atomic::write(&self.rule_path(&record.id), record.knowledge.as_bytes())?;
        let mut meta = self.read_meta();
        meta.rules.insert(record.id.clone(), RuleMeta::from(record));
        self.write_meta(&meta)
    }

    /// Deletes one rule and its metadata entry.
    ///
    /// # Errors
    ///
    /// Fails when `id` is not a valid rule id, or the file or metadata index
    /// cannot be written.
    pub fn remove(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        remove_file_if_exists(&self.rule_path(id))?;
        let mut meta = self.read_meta();
        if meta.rules.remove(id).is_some() {
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn rule_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.{RULE_EXTENSION}"))
    }

    fn meta_path(&self) -> PathBuf {
        self.root.join(META_FILE)
    }

    fn read_meta(&self) -> Meta {
        let bytes = match std::fs::read(self.meta_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Meta::default(),
            Err(error) => {
                tracing::warn!(%error, "rules meta.json is unreadable; treating every rule as untitled");
                return Meta::default();
            }
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(%error, "rules meta.json is corrupt; starting from empty metadata");
            Meta::default()
        })
    }

    fn write_meta(&self, meta: &Meta) -> Result<()> {
        crate::atomic::write(&self.meta_path(), &serde_json::to_vec_pretty(meta)?)?;
        Ok(())
    }
}

/// A rule read from disk plus whatever the metadata index knows about it. A
/// rule with no metadata entry falls back to its file name and mtime.
fn assemble(id: &str, knowledge: String, meta: Option<&RuleMeta>, path: &Path) -> RuleRecord {
    let meta = meta.cloned().unwrap_or_else(|| RuleMeta {
        title: id.to_owned(),
        created_at: file_modified_at(path),
        ..RuleMeta::default()
    });
    RuleRecord {
        id: id.to_owned(),
        knowledge,
        title: meta.title,
        created_at: meta.created_at,
        is_generated: meta.is_generated,
        git_origin: meta.git_origin,
    }
}

impl From<&RuleRecord> for RuleMeta {
    fn from(record: &RuleRecord) -> Self {
        Self {
            title: record.title.clone(),
            created_at: record.created_at.clone(),
            is_generated: record.is_generated,
            git_origin: record.git_origin.clone(),
        }
    }
}

fn timestamp(created_at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(created_at).map_or(0, |time| time.timestamp_millis())
}

fn file_modified_at(path: &Path) -> String {
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or_else(|_| std::time::SystemTime::now());
    chrono::DateTime::<chrono::Utc>::from(modified)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Protocol(format!("invalid rule id: {id:?}")));
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KnowledgeBaseAddRequest {
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
pub(crate) struct KnowledgeBaseAddResponse {
    #[prost(bool, tag = "1")]
    success: bool,
    #[prost(string, tag = "2")]
    id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KnowledgeBaseListRequest {
    #[prost(int32, optional, tag = "1")]
    limit: Option<i32>,
    #[prost(string, optional, tag = "2")]
    git_origin: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KnowledgeBaseListResponse {
    #[prost(bool, tag = "1")]
    success: bool,
    #[prost(message, repeated, tag = "2")]
    all_results: Vec<KnowledgeBaseListItem>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KnowledgeBaseListItem {
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
pub(crate) struct KnowledgeBaseUpdateRequest {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    knowledge: String,
    #[prost(string, tag = "3")]
    title: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KnowledgeBaseUpdateResponse {
    #[prost(bool, tag = "1")]
    success: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KnowledgeBaseRemoveRequest {
    #[prost(string, tag = "1")]
    id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KnowledgeBaseRemoveResponse {
    #[prost(bool, tag = "1")]
    success: bool,
}

/// The rule store behind every knowledge-base RPC.
#[derive(Clone)]
pub struct KnowledgeService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for KnowledgeService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KnowledgeService")
            .finish_non_exhaustive()
    }
}

struct Inner {
    store: RuleStore,
    lock: tokio::sync::Mutex<()>,
}

impl KnowledgeService {
    /// Opens the rule store under `root`.
    ///
    /// # Errors
    ///
    /// Fails when the rules directory cannot be created.
    pub fn with_root(root: std::path::PathBuf) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                store: RuleStore::open(root)?,
                lock: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// The rules as Cursor sends them, read under the same lock as writes so a
    /// rule file and its metadata cannot be observed half-updated.
    pub async fn local_cursor_rules(&self) -> Vec<pb::CursorRule> {
        let _guard = self.inner.lock.lock().await;
        let records = match self.inner.store.list() {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "cannot read local rules; continuing without them");
                return Vec::new();
            }
        };
        records
            .into_iter()
            .filter(|record| !record.knowledge.trim().is_empty())
            .map(|record| pb::CursorRule {
                content: record.knowledge,
                source: i32::from(pb::CursorRuleSource::User),
                ..Default::default()
            })
            .collect()
    }
}

/// Stores one rule under a fresh local id and answers with it.
pub async fn add(
    Extension(service): Extension<KnowledgeService>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let body = super::read_body(request.into_body()).await?;
    let message: KnowledgeBaseAddRequest = connect::decode_unary(&body)?;
    let _guard = service.inner.lock.lock().await;
    let id = format!("{LOCAL_ID_PREFIX}{}", uuid::Uuid::new_v4());
    service.inner.store.upsert(&RuleRecord {
        id: id.clone(),
        knowledge: message.knowledge,
        title: message.title,
        created_at: now(),
        is_generated: false,
        git_origin: message.git_origin,
    })?;
    Ok(proto(&KnowledgeBaseAddResponse { success: true, id }))
}

/// Lists stored rules newest first, filtered by git origin and truncated to the requested limit.
pub async fn list(
    Extension(service): Extension<KnowledgeService>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let body = super::read_body(request.into_body()).await?;
    let message: KnowledgeBaseListRequest = connect::decode_unary(&body)?;
    let _guard = service.inner.lock.lock().await;
    let mut records = service.inner.store.list()?;
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
    Ok(proto(&KnowledgeBaseListResponse {
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
    let message: KnowledgeBaseUpdateRequest = connect::decode_unary(&body)?;
    let _guard = service.inner.lock.lock().await;
    let Some(mut record) = service.inner.store.get(&message.id)? else {
        return Ok(proto(&KnowledgeBaseUpdateResponse { success: false }));
    };
    record.knowledge = message.knowledge;
    record.title = message.title;
    service.inner.store.upsert(&record)?;
    Ok(proto(&KnowledgeBaseUpdateResponse { success: true }))
}

/// Deletes a rule and its metadata entry; removing an absent id still succeeds.
pub async fn remove(
    Extension(service): Extension<KnowledgeService>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let body = super::read_body(request.into_body()).await?;
    let message: KnowledgeBaseRemoveRequest = connect::decode_unary(&body)?;
    let _guard = service.inner.lock.lock().await;
    service.inner.store.remove(&message.id)?;
    Ok(proto(&KnowledgeBaseRemoveResponse { success: true }))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn proto(message: &impl Message) -> Response<Body> {
    super::proto_response(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, knowledge: &str, created_at: &str) -> RuleRecord {
        RuleRecord {
            id: id.into(),
            knowledge: knowledge.into(),
            title: format!("title-{id}"),
            created_at: created_at.into(),
            is_generated: false,
            git_origin: String::new(),
        }
    }

    #[test]
    fn upserts_lists_and_removes_rules() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("100", "older", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        store
            .upsert(&record("200", "newer", "2026-02-01T00:00:00.000Z"))
            .unwrap();
        let listed = store.list().unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            ["200", "100"]
        );
        store.remove("200").unwrap();
        assert!(store.get("200").unwrap().is_none());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        assert!(store.get("../escape").is_err());
        assert!(store.get("a/b").is_err());
        assert!(store.get("").is_err());
    }
}
