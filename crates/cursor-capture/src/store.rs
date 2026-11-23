//! In-memory capture window, SQLite persistence, and SSE notifications.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::broadcast;

use crate::decode::hydrate_stored_exchange;
use crate::types::{
    ConversationSummary, Error, Exchange, ExchangeSummary, Header, Result, StoreEvent,
};

const EVENT_CAPACITY: usize = 256;

/// Whether an update should also be written through to SQLite.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Persistence {
    /// Persist the exchange after the update (finished state).
    Write,
    /// Leave the last persisted row alone (streaming progress).
    Skip,
}

/// `inner.max` holds a validated positive count; SQLite wants it as `i64`.
fn row_limit(max: usize) -> i64 {
    i64::try_from(max).unwrap_or(i64::MAX)
}

#[derive(Clone)]
#[expect(
    missing_debug_implementations,
    reason = "the store owns a rusqlite Connection, which has no Debug impl"
)]
pub struct ExchangeStore {
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<StoreEvent>,
}

#[derive(Clone)]
pub(crate) struct CaptureId {
    generation: u64,
    number: String,
}

struct Inner {
    generation: u64,
    next_number: u64,
    max: usize,
    order: Vec<String>,
    exchanges: HashMap<String, Exchange>,
    db: Option<Connection>,
    database_path: String,
    last_error: String,
}

impl ExchangeStore {
    pub fn memory(max: i32) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                generation: 0,
                next_number: 1,
                max: usize::try_from(max.max(1)).unwrap_or(usize::MAX),
                order: Vec::new(),
                exchanges: HashMap::new(),
                db: None,
                database_path: String::new(),
                last_error: String::new(),
            })),
            events,
        }
    }

    pub fn persistent(path: impl AsRef<Path>, max: i32) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| Error::Database(format!("创建 SQLite 数据目录失败: {err}")))?;
        }
        let db = Connection::open(path)
            .map_err(|err| Error::Database(format!("打开 SQLite 数据库失败: {err}")))?;
        db.busy_timeout(Duration::from_millis(5_000))
            .map_err(|err| Error::Database(err.to_string()))?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA secure_delete = ON;
             CREATE TABLE IF NOT EXISTS exchanges (
                id TEXT PRIMARY KEY,
                started_at_ms INTEGER NOT NULL,
                conversation_id TEXT NOT NULL DEFAULT '',
                request_id TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL DEFAULT '',
                request_bytes INTEGER NOT NULL DEFAULT 0,
                response_bytes INTEGER NOT NULL DEFAULT 0,
                payload_json BLOB NOT NULL,
                updated_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS exchanges_conversation_started_idx
                ON exchanges(conversation_id, started_at_ms DESC);
             CREATE INDEX IF NOT EXISTS exchanges_request_idx ON exchanges(request_id);",
        )
        .map_err(|err| Error::Database(format!("初始化 SQLite 数据库失败: {err}")))?;

        let store = Self::memory(max);
        {
            let mut inner = store.inner.lock();
            let maximum: i64 = db
                .query_row(
                    "SELECT COALESCE(MAX(CAST(id AS INTEGER)), 0) FROM exchanges",
                    [],
                    |row| row.get(0),
                )
                .map_err(|err| Error::Database(err.to_string()))?;
            inner.next_number = u64::try_from(maximum.max(0)).unwrap_or(u64::MAX) + 1;
            inner.db = Some(db);
            inner.database_path = path.display().to_string();
            Self::backfill_decoded_exchanges(&mut inner)?;
            Self::load_recent(&mut inner)?;
        }
        Ok(store)
    }

    fn backfill_decoded_exchanges(inner: &mut Inner) -> Result<()> {
        let Some(db) = inner.db.as_mut() else {
            return Ok(());
        };
        let mut stmt = db
            .prepare("SELECT payload_json FROM exchanges")
            .map_err(|err| Error::Database(format!("读取待回填的 SQLite 抓包记录失败: {err}")))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|err| Error::Database(err.to_string()))?;
        let mut exchanges = Vec::new();
        for payload in rows {
            let payload = payload.map_err(|err| Error::Database(err.to_string()))?;
            let mut exchange: Exchange = serde_json::from_slice(&payload).map_err(|err| {
                Error::Database(format!("解析待回填的 SQLite 抓包记录失败: {err}"))
            })?;
            if hydrate_stored_exchange(&mut exchange) {
                exchanges.push(exchange);
            }
        }
        drop(stmt);
        if exchanges.is_empty() {
            return Ok(());
        }
        let tx = db
            .transaction()
            .map_err(|err| Error::Database(err.to_string()))?;
        for exchange in &exchanges {
            let payload =
                serde_json::to_vec(exchange).map_err(|err| Error::Database(err.to_string()))?;
            tx.execute(
                "UPDATE exchanges SET payload_json = ?, conversation_id = ?, request_id = ?, updated_at_ms = ? WHERE id = ?",
                params![
                    payload,
                    exchange.summary.conversation_id,
                    exchange.summary.request_id,
                    Utc::now().timestamp_millis(),
                    exchange.summary.id
                ],
            )
            .map_err(|err| Error::Database(err.to_string()))?;
        }
        tx.commit()
            .map_err(|err| Error::Database(err.to_string()))?;
        Ok(())
    }

    fn load_recent(inner: &mut Inner) -> Result<()> {
        let Some(db) = inner.db.as_mut() else {
            return Ok(());
        };
        let mut stmt = db
            .prepare(
                "SELECT payload_json, conversation_id FROM exchanges
                 ORDER BY started_at_ms DESC, id DESC LIMIT ?",
            )
            .map_err(|err| Error::Database(format!("读取 SQLite 抓包记录失败: {err}")))?;
        let rows = stmt
            .query_map([row_limit(inner.max)], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| Error::Database(err.to_string()))?;
        let mut loaded = Vec::new();
        for row in rows {
            let (payload, conversation_id) = row.map_err(|err| Error::Database(err.to_string()))?;
            let mut exchange: Exchange = serde_json::from_slice(&payload)
                .map_err(|err| Error::Database(format!("解析 SQLite 抓包记录失败: {err}")))?;
            exchange.summary.conversation_id = conversation_id;
            loaded.push(exchange);
        }
        for exchange in loaded {
            inner
                .exchanges
                .insert(exchange.summary.id.clone(), exchange.clone());
            inner.order.push(exchange.summary.id);
        }
        Ok(())
    }

    pub(crate) fn create(&self, mut exchange: Exchange) -> CaptureId {
        let mut inner = self.inner.lock();
        let id = CaptureId {
            generation: inner.generation,
            number: inner.next_number.to_string(),
        };
        inner.next_number += 1;
        exchange.summary.id.clone_from(&id.number);
        cache_exchange(&mut inner, exchange);
        persist_locked(&mut inner, &id.number);
        self.publish(StoreEvent {
            kind: "created".into(),
            id: Some(id.number.clone()),
        });
        id
    }

    pub(crate) fn update(&self, id: &CaptureId, apply: impl FnOnce(&mut Exchange)) {
        self.apply_update(id, apply, Persistence::Write);
    }

    pub(crate) fn update_transient(&self, id: &CaptureId, apply: impl FnOnce(&mut Exchange)) {
        self.apply_update(id, apply, Persistence::Skip);
    }

    fn apply_update(
        &self,
        id: &CaptureId,
        apply: impl FnOnce(&mut Exchange),
        persistence: Persistence,
    ) {
        let mut inner = self.inner.lock();
        if id.generation != inner.generation {
            return;
        }
        let id = id.number.as_str();
        if !inner.exchanges.contains_key(id) {
            match load_persisted_locked(&mut inner, id) {
                Ok(Some(loaded)) => cache_exchange(&mut inner, loaded),
                Ok(None) => return,
                Err(err) => {
                    inner.last_error = err.to_string();
                    return;
                }
            }
        }
        let Some(exchange) = inner.exchanges.get_mut(id) else {
            return;
        };
        let previous_request_id = exchange.summary.request_id.clone();
        let previous_conversation_id = exchange.summary.conversation_id.clone();
        apply(exchange);
        if exchange.summary.request_id != previous_request_id
            || exchange.summary.conversation_id != previous_conversation_id
        {
            let request_id = exchange.summary.request_id.clone();
            let conversation_id = exchange.summary.conversation_id.clone();
            associate_conversation_locked(&mut inner, id, &request_id, conversation_id);
        }
        if persistence == Persistence::Write {
            persist_locked(&mut inner, id);
        }
        self.publish(StoreEvent {
            kind: "updated".into(),
            id: Some(id.to_string()),
        });
    }

    pub fn summaries(&self, conversation_id: &str) -> Result<Vec<ExchangeSummary>> {
        let mut inner = self.inner.lock();
        if inner.db.is_none() {
            return Ok(inner
                .order
                .iter()
                .filter_map(|id| inner.exchanges.get(id).map(|item| item.summary.clone()))
                .collect());
        }
        let rows = {
            let Some(db) = inner.db.as_mut() else {
                return Ok(Vec::new());
            };
            let mut collected = Vec::new();
            if conversation_id.is_empty() {
                let mut stmt = db
                    .prepare(
                        "SELECT payload_json, conversation_id FROM exchanges
                         ORDER BY started_at_ms DESC, id DESC",
                    )
                    .map_err(|err| Error::Database(err.to_string()))?;
                let mapped = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|err| Error::Database(err.to_string()))?;
                for row in mapped {
                    collected.push(row.map_err(|err| Error::Database(err.to_string()))?);
                }
            } else {
                let mut stmt = db
                    .prepare(
                        "SELECT payload_json, conversation_id FROM exchanges
                         WHERE conversation_id = ? ORDER BY started_at_ms DESC, id DESC",
                    )
                    .map_err(|err| Error::Database(err.to_string()))?;
                let mapped = stmt
                    .query_map([conversation_id], |row| {
                        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|err| Error::Database(err.to_string()))?;
                for row in mapped {
                    collected.push(row.map_err(|err| Error::Database(err.to_string()))?);
                }
            }
            collected
        };
        let mut result = Vec::new();
        for (payload, persisted_conversation_id) in rows {
            let mut exchange: Exchange =
                serde_json::from_slice(&payload).map_err(|err| Error::Database(err.to_string()))?;
            exchange.summary.conversation_id = persisted_conversation_id;
            if let Some(current) = inner.exchanges.get(&exchange.summary.id) {
                result.push(current.summary.clone());
            } else {
                result.push(exchange.summary);
            }
        }
        Ok(result)
    }

    pub fn get(&self, id: &str) -> Result<Option<Exchange>> {
        let mut inner = self.inner.lock();
        if let Some(exchange) = inner.exchanges.get(id) {
            return Ok(Some(exchange.clone()));
        }
        if inner.db.is_none() {
            return Ok(None);
        }
        load_persisted_locked(&mut inner, id)
    }

    pub fn clear(&self) -> Result<()> {
        {
            let mut inner = self.inner.lock();
            if let Some(db) = inner.db.as_mut()
                && let Err(err) = db.execute("DELETE FROM exchanges", [])
            {
                inner.last_error = err.to_string();
                return Err(Error::Database(err.to_string()));
            }
            inner.order.clear();
            inner.exchanges.clear();
            inner.last_error.clear();
            inner.generation += 1;
            inner.next_number = 1;
            self.publish(StoreEvent {
                kind: "cleared".into(),
                id: None,
            });
        }
        Ok(())
    }

    pub fn conversations(&self) -> Result<Vec<ConversationSummary>> {
        let mut inner = self.inner.lock();
        if inner.db.is_some() {
            let rows = {
                let Some(db) = inner.db.as_mut() else {
                    return Ok(Vec::new());
                };
                let mut stmt = db
                    .prepare(
                        "SELECT conversation_id, COUNT(*), MAX(started_at_ms),
                                COALESCE(SUM(request_bytes), 0), COALESCE(SUM(response_bytes), 0)
                         FROM exchanges GROUP BY conversation_id ORDER BY MAX(started_at_ms) DESC",
                    )
                    .map_err(|err| Error::Database(err.to_string()))?;
                let mapped = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i32>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    })
                    .map_err(|err| Error::Database(err.to_string()))?;
                let mut collected = Vec::new();
                for row in mapped {
                    collected.push(row.map_err(|err| Error::Database(err.to_string()))?);
                }
                collected
            };
            let mut result = Vec::new();
            for (conversation_id, exchange_count, started_at_ms, request_bytes, response_bytes) in
                rows
            {
                result.push(ConversationSummary {
                    conversation_id,
                    exchange_count,
                    last_started_at: DateTime::from_timestamp_millis(started_at_ms)
                        .unwrap_or_else(Utc::now),
                    request_bytes,
                    response_bytes,
                });
            }
            return Ok(result);
        }
        let mut groups: HashMap<String, ConversationSummary> = HashMap::new();
        for exchange in inner.exchanges.values() {
            let group = groups
                .entry(exchange.summary.conversation_id.clone())
                .or_insert_with(|| ConversationSummary {
                    conversation_id: exchange.summary.conversation_id.clone(),
                    exchange_count: 0,
                    last_started_at: exchange.summary.started_at,
                    request_bytes: 0,
                    response_bytes: 0,
                });
            group.exchange_count += 1;
            group.request_bytes += exchange.summary.request_bytes;
            group.response_bytes += exchange.summary.response_bytes;
            if exchange.summary.started_at > group.last_started_at {
                group.last_started_at = exchange.summary.started_at;
            }
        }
        let mut result: Vec<_> = groups.into_values().collect();
        result.sort_by_key(|left| std::cmp::Reverse(left.last_started_at));
        Ok(result)
    }

    pub fn status(&self) -> StoreStatus {
        let inner = self.inner.lock();
        StoreStatus {
            database_path: inner.database_path.clone(),
            last_error: inner.last_error.clone(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StoreEvent> {
        self.events.subscribe()
    }

    fn publish(&self, event: StoreEvent) {
        let _ = self.events.send(event);
    }

    pub fn database_path(&self) -> PathBuf {
        PathBuf::from(self.inner.lock().database_path.clone())
    }
}

/// Where the store keeps its database and the last write failure, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreStatus {
    /// Absolute path of the SQLite file; empty for the in-memory store.
    pub database_path: String,
    /// Last persistence error, cleared by the next successful write.
    pub last_error: String,
}

fn cache_exchange(inner: &mut Inner, exchange: Exchange) {
    let id = exchange.summary.id.clone();
    inner.exchanges.insert(id.clone(), exchange);
    inner.order.insert(0, id);
    while inner.order.len() > inner.max {
        if let Some(oldest) = inner.order.pop() {
            inner.exchanges.remove(&oldest);
        }
    }
}

fn persist_locked(inner: &mut Inner, id: &str) {
    let (Some(db), Some(exchange)) = (inner.db.as_mut(), inner.exchanges.get(id)) else {
        return;
    };
    match serde_json::to_vec(exchange) {
        Ok(payload) => {
            match db.execute(
                "INSERT INTO exchanges (
                    id, started_at_ms, conversation_id, request_id, state, request_bytes,
                    response_bytes, payload_json, updated_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(id) DO UPDATE SET
                    started_at_ms = excluded.started_at_ms,
                    conversation_id = excluded.conversation_id,
                    request_id = excluded.request_id,
                    state = excluded.state,
                    request_bytes = excluded.request_bytes,
                    response_bytes = excluded.response_bytes,
                    payload_json = excluded.payload_json,
                    updated_at_ms = excluded.updated_at_ms",
                params![
                    exchange.summary.id,
                    exchange.summary.started_at.timestamp_millis(),
                    exchange.summary.conversation_id,
                    exchange.summary.request_id,
                    exchange.summary.state,
                    exchange.summary.request_bytes,
                    exchange.summary.response_bytes,
                    payload,
                    Utc::now().timestamp_millis()
                ],
            ) {
                Ok(_) => inner.last_error.clear(),
                Err(err) => inner.last_error = err.to_string(),
            }
        }
        Err(err) => inner.last_error = err.to_string(),
    }
}

fn load_persisted_locked(inner: &mut Inner, id: &str) -> Result<Option<Exchange>> {
    let Some(db) = inner.db.as_mut() else {
        return Ok(None);
    };
    let row = db
        .query_row(
            "SELECT payload_json, conversation_id FROM exchanges WHERE id = ?",
            [id],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|err| Error::Database(err.to_string()))?;
    let Some((payload, conversation_id)) = row else {
        return Ok(None);
    };
    let mut persisted: Exchange =
        serde_json::from_slice(&payload).map_err(|err| Error::Database(err.to_string()))?;
    persisted.summary.conversation_id = conversation_id;
    Ok(Some(persisted))
}

fn associate_conversation_locked(
    inner: &mut Inner,
    id: &str,
    request_id: &str,
    mut conversation_id: String,
) {
    if request_id.is_empty() {
        return;
    }
    if conversation_id.is_empty() {
        for candidate in inner.exchanges.values() {
            if candidate.summary.request_id == request_id
                && !candidate.summary.conversation_id.is_empty()
            {
                conversation_id.clone_from(&candidate.summary.conversation_id);
                break;
            }
        }
    }
    if conversation_id.is_empty()
        && let Some(db) = inner.db.as_mut()
    {
        let _ = db.query_row(
            "SELECT conversation_id FROM exchanges
             WHERE request_id = ? AND conversation_id != ''
             ORDER BY started_at_ms DESC LIMIT 1",
            [request_id],
            |row| {
                conversation_id = row.get(0)?;
                Ok(())
            },
        );
    }
    if conversation_id.is_empty() {
        return;
    }
    if let Some(current) = inner.exchanges.get_mut(id) {
        current.summary.conversation_id.clone_from(&conversation_id);
    }
    let mut related = Vec::new();
    for candidate in inner.exchanges.values_mut() {
        if candidate.summary.request_id == request_id
            && candidate.summary.conversation_id.is_empty()
        {
            candidate
                .summary
                .conversation_id
                .clone_from(&conversation_id);
            related.push(candidate.summary.id.clone());
        }
    }
    for id in related {
        persist_locked(inner, &id);
    }
    if let Some(db) = inner.db.as_mut()
        && let Err(err) = db.execute(
            "UPDATE exchanges SET conversation_id = ?, updated_at_ms = ?
             WHERE request_id = ? AND conversation_id = ''",
            params![conversation_id, Utc::now().timestamp_millis(), request_id],
        )
    {
        inner.last_error = err.to_string();
    }
}

pub fn sorted_headers(headers: &http::HeaderMap) -> Vec<Header> {
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in headers {
        grouped
            .entry(name.as_str().to_string())
            .or_default()
            .push(value.to_str().unwrap_or_default().to_string());
    }
    let mut result: Vec<Header> = grouped
        .into_iter()
        .map(|(name, values)| {
            let mut value = values.join(", ");
            if is_sensitive_header(&name) && !value.is_empty() {
                value = "[已隐藏]".into();
            }
            Header { name, value }
        })
        .collect();
    result.sort_by(|left, right| left.name.cmp(&right.name));
    result
}

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "set-cookie" | "proxy-authorization" | "x-api-key"
    )
}
