//! In-memory capture window, SQLite persistence, and SSE notifications.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::broadcast;

use crate::decode::hydrate_stored_exchange;
use crate::types::{
    ConversationSummary, Error, Exchange, ExchangeSummary, Header, Result, StoreEvent,
    StoreEventKind,
};

const EVENT_CAPACITY: usize = 256;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

thread_local! {
    /// How many [`ExchangeStore::apply_update`] closures are running on this
    /// thread. The store lock is not reentrant, so a callback that reached back
    /// into the store would deadlock; this turns that into a reported failure.
    static UPDATE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Marks the scope in which a caller's update closure runs under the lock.
struct UpdateScope;

impl UpdateScope {
    fn enter() -> Self {
        UPDATE_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for UpdateScope {
    fn drop(&mut self) {
        UPDATE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Whether this thread is inside an update closure, and so must not take the
/// store lock again.
fn inside_update() -> bool {
    UPDATE_DEPTH.with(std::cell::Cell::get) > 0
}

/// Why a reentrant call was refused.
///
/// Kept apart from [`Inner::last_error`] because the write that usually follows
/// a successful update clears that field, which would hide the violation, and
/// because reporting it must not touch the lock the caller already holds.
#[derive(Debug, Default)]
struct Refusals(Mutex<Option<String>>);

impl Refusals {
    /// Records a refused call without touching the store lock.
    fn record(&self, operation: &str) {
        *self.0.lock() = Some(format!(
            "{operation} was called from inside an update closure and was refused"
        ));
    }
}

/// Reports a refused reentrant call without blocking on the lock it would
/// deadlock on. Returns `true` when the call must be refused.
fn refuse_reentrancy(store: &ExchangeStore, operation: &'static str) -> bool {
    if !inside_update() {
        return false;
    }
    store.refusals.record(operation);
    true
}

/// How many exchanges the startup backfill hydrates and rewrites per batch. Small
/// enough that a long history never sits fully decoded in memory.
const BACKFILL_BATCH: usize = 64;

/// Attaches the failed operation to a wrapped error.
trait Context<T> {
    fn context(self, operation: &'static str) -> Result<T>;
}

impl<T> Context<T> for std::result::Result<T, rusqlite::Error> {
    fn context(self, operation: &'static str) -> Result<T> {
        self.map_err(|source| Error::database(operation, source))
    }
}

impl<T> Context<T> for std::result::Result<T, serde_json::Error> {
    fn context(self, operation: &'static str) -> Result<T> {
        self.map_err(|source| Error::json(operation, source))
    }
}

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

/// A new exchange, as returned by `ExchangeStore::create`.
#[derive(Clone)]
#[must_use]
pub(crate) struct CaptureId {
    generation: u64,
    number: String,
}

#[derive(Clone)]
pub struct ExchangeStore {
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<StoreEvent>,
    /// Names of calls refused because they ran inside an update closure. Separate
    /// from `Inner` so recording one never takes the lock the caller holds.
    refusals: Arc<Refusals>,
}

impl std::fmt::Debug for ExchangeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never take the store lock here: `Debug` is reachable from callbacks
        // that already hold it, and a re-entrant lock would deadlock.
        f.debug_struct("ExchangeStore")
            .field("subscribers", &self.events.receiver_count())
            .finish_non_exhaustive()
    }
}

struct Inner {
    generation: u64,
    next_number: u64,
    max: usize,
    order: Vec<String>,
    exchanges: HashMap<String, Exchange>,
    db: Option<Connection>,
    database_path: String,
    /// The most recent persistence failure, cleared by the next success.
    last_error: Option<String>,
}

impl ExchangeStore {
    /// Creates a store that keeps at most `max` exchanges in memory.
    #[must_use]
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
                last_error: None,
            })),
            events,
            refusals: Arc::new(Refusals::default()),
        }
    }

    /// Creates a store backed by the SQLite file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the database's directory cannot be created,
    /// and [`Error::Database`] when the database cannot be opened, migrated or
    /// read.
    pub fn persistent(path: impl AsRef<Path>, max: i32) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| Error::io("creating the SQLite directory", source))?;
        }
        let db = Connection::open(path).context("opening the database")?;
        db.busy_timeout(SQLITE_BUSY_TIMEOUT)
            .context("setting the busy timeout")?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
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
                summary_json BLOB NOT NULL DEFAULT '{}',
                updated_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS exchanges_conversation_started_idx
                ON exchanges(conversation_id, started_at_ms DESC);
             CREATE INDEX IF NOT EXISTS exchanges_request_idx ON exchanges(request_id);",
        )
        .context("initializing the schema")?;

        let store = Self::memory(max);
        {
            let mut inner = store.inner.lock();
            let maximum: i64 = db
                .query_row(
                    "SELECT COALESCE(MAX(CAST(id AS INTEGER)), 0) FROM exchanges",
                    [],
                    |row| row.get(0),
                )
                .context("reading the highest exchange id")?;
            inner.next_number = u64::try_from(maximum.max(0)).unwrap_or(u64::MAX) + 1;
            inner.db = Some(db);
            inner.database_path = path.display().to_string();
            Self::backfill_decoded_exchanges(&mut inner)?;
            Self::load_recent(&mut inner)?;
        }
        Ok(store)
    }

    /// Hydrates stored exchanges in bounded batches, rewriting only those whose
    /// decode is out of date. Without batching, a long history would be fully
    /// deserialized into one `Vec` before any row was updated.
    fn backfill_decoded_exchanges(inner: &mut Inner) -> Result<()> {
        let Some(db) = inner.db.as_mut() else {
            return Ok(());
        };
        let mut last_id: i64 = 0;
        while Self::backfill_batch(db, &mut last_id)? {
            // Keep going until a batch comes back short.
        }
        Ok(())
    }

    /// Hydrates the next `BACKFILL_BATCH` rows past `last_id`. Returns whether a
    /// further batch may exist.
    fn backfill_batch(db: &mut Connection, last_id: &mut i64) -> Result<bool> {
        let rows = {
            let mut stmt = db
                .prepare(
                    "SELECT CAST(id AS INTEGER) AS row_id, payload_json FROM exchanges
                     WHERE CAST(id AS INTEGER) > ? ORDER BY row_id ASC LIMIT ?",
                )
                .context("preparing the backfill read")?;
            let mapped = stmt
                .query_map(params![*last_id, row_limit(BACKFILL_BATCH)], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .context("querying rows to backfill")?;
            let mut collected = Vec::new();
            for row in mapped {
                collected.push(row.context("reading a backfill row")?);
            }
            collected
        };
        if rows.is_empty() {
            return Ok(false);
        }
        let full_batch = rows.len() == BACKFILL_BATCH;
        let mut rewrites = Vec::new();
        for (row_id, payload) in rows {
            *last_id = row_id;
            let mut exchange: Exchange =
                serde_json::from_slice(&payload).context("decoding a backfill row")?;
            if hydrate_stored_exchange(&mut exchange).is_changed() {
                rewrites.push(exchange);
            }
        }
        if !rewrites.is_empty() {
            let tx = db
                .transaction()
                .context("opening the backfill transaction")?;
            for exchange in &rewrites {
                let payload =
                    serde_json::to_vec(exchange).context("encoding a backfilled exchange")?;
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
                .context("writing a backfilled exchange")?;
            }
            tx.commit().context("committing the backfill")?;
        }
        Ok(full_batch)
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
            .context("preparing the recent-rows read")?;
        let rows = stmt
            .query_map([row_limit(inner.max)], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })
            .context("querying recent rows")?;
        let mut loaded = Vec::new();
        for row in rows {
            let (payload, conversation_id) = row.context("reading a recent row")?;
            let mut exchange: Exchange =
                serde_json::from_slice(&payload).context("decoding a recent row")?;
            exchange.summary.conversation_id = conversation_id;
            loaded.push(exchange);
        }
        for exchange in loaded {
            let id = exchange.summary.id.clone();
            inner.exchanges.insert(id.clone(), exchange);
            inner.order.push(id);
        }
        Ok(())
    }

    pub(crate) fn create(&self, mut exchange: Exchange) -> CaptureId {
        let (id, event) = {
            let mut inner = self.inner.lock();
            let id = CaptureId {
                generation: inner.generation,
                number: inner.next_number.to_string(),
            };
            inner.next_number += 1;
            exchange.summary.id.clone_from(&id.number);
            cache_exchange(&mut inner, exchange);
            persist_locked(&mut inner, &id.number);
            let event = StoreEvent {
                kind: StoreEventKind::Created,
                id: Some(id.number.clone()),
            };
            (id, event)
        };
        self.publish(event);
        id
    }

    pub(crate) fn update(&self, id: &CaptureId, apply: impl FnOnce(&mut Exchange)) {
        self.apply_update(id, apply, Persistence::Write);
    }

    pub(crate) fn update_transient(&self, id: &CaptureId, apply: impl FnOnce(&mut Exchange)) {
        self.apply_update(id, apply, Persistence::Skip);
    }

    /// Applies `apply` to the cached exchange and notifies subscribers.
    ///
    /// # Reentrancy
    ///
    /// `apply` runs while the store's lock is held: it must only touch the
    /// exchange it is given. Calling back into this store from it (or doing
    /// decoding that should have happened before) would either block on the
    /// non-reentrant lock or hold it for the length of that work. The
    /// [`UpdateScope`] below turns such a call into a degraded answer instead
    /// of a hang; keep expensive work outside this call.
    fn apply_update(
        &self,
        id: &CaptureId,
        apply: impl FnOnce(&mut Exchange),
        persistence: Persistence,
    ) {
        let event = {
            let mut inner = self.inner.lock();
            if id.generation != inner.generation {
                return;
            }
            let number = id.number.as_str();
            if !inner.exchanges.contains_key(number) {
                match load_persisted_locked(&mut inner, number) {
                    Ok(Some(loaded)) => cache_exchange(&mut inner, loaded),
                    Ok(None) => return,
                    Err(err) => {
                        inner.last_error = Some(err.to_string());
                        return;
                    }
                }
            }
            let Some(exchange) = inner.exchanges.get_mut(number) else {
                return;
            };
            let previous_request_id = exchange.summary.request_id.clone();
            let previous_conversation_id = exchange.summary.conversation_id.clone();
            {
                let _guard = UpdateScope::enter();
                apply(exchange);
            }
            if exchange.summary.request_id != previous_request_id
                || exchange.summary.conversation_id != previous_conversation_id
            {
                let request_id = exchange.summary.request_id.clone();
                let conversation_id = exchange.summary.conversation_id.clone();
                associate_conversation_locked(&mut inner, number, &request_id, conversation_id);
            }
            if persistence == Persistence::Write {
                persist_locked(&mut inner, number);
            }
            StoreEvent {
                kind: StoreEventKind::Updated,
                id: Some(number.to_owned()),
            }
        };
        self.publish(event);
    }

    /// Lists exchange summaries, filtered to `conversation_id` when non-empty.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] when the database cannot be read, and
    /// [`Error::Json`] when a stored payload cannot be decoded.
    pub fn summaries(&self, conversation_id: &str) -> Result<Vec<ExchangeSummary>> {
        if refuse_reentrancy(self, "summaries") {
            return Ok(Vec::new());
        }
        let mut inner = self.inner.lock();
        let conversation = conversation_id.trim();
        if inner.db.is_none() {
            // The memory branch must apply the same filter as the SQL one, or the
            // two paths disagree for the same call.
            return Ok(inner
                .order
                .iter()
                .filter_map(|id| inner.exchanges.get(id))
                .filter(|item| {
                    conversation.is_empty() || item.summary.conversation_id == conversation
                })
                .map(|item| item.summary.clone())
                .collect());
        }
        let Some(db) = inner.db.as_mut() else {
            return Ok(Vec::new());
        };
        // Only the summary columns are read: `payload_json` holds whole bodies,
        // and loading it just to summarise would scale memory with history size.
        // `summary_json` is exactly the shape `ExchangeSummary` serializes to, so
        // this path never needs to deserialize the full payload.
        let sql = if conversation.is_empty() {
            "SELECT id, summary_json FROM exchanges ORDER BY started_at_ms DESC, id DESC"
        } else {
            "SELECT id, summary_json FROM exchanges WHERE conversation_id = ?
             ORDER BY started_at_ms DESC, id DESC"
        };
        let mut stmt = db.prepare(sql).context("preparing the summary read")?;
        let read_row =
            |row: &rusqlite::Row<'_>| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?));
        let mapped = if conversation.is_empty() {
            stmt.query_map([], read_row)
        } else {
            stmt.query_map([conversation], read_row)
        }
        .context("querying exchange summaries")?;
        let mut rows = Vec::new();
        for row in mapped {
            rows.push(row.context("reading a summary row")?);
        }
        drop(stmt);
        // Reuse the cached (authoritative, in-memory) summary when one exists;
        // otherwise decode only the small persisted summary.
        let mut result = Vec::with_capacity(rows.len());
        for (id, summary_json) in rows {
            if let Some(current) = inner.exchanges.get(&id) {
                result.push(current.summary.clone());
                continue;
            }
            let summary: ExchangeSummary =
                serde_json::from_slice(&summary_json).context("decoding a summary row")?;
            result.push(summary);
        }
        Ok(result)
    }

    /// Loads one exchange, from the cache or the database.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] when the database cannot be read, and
    /// [`Error::Json`] when the stored payload cannot be decoded.
    pub fn get(&self, id: &str) -> Result<Option<Exchange>> {
        if refuse_reentrancy(self, "get") {
            return Ok(None);
        }
        let mut inner = self.inner.lock();
        if let Some(exchange) = inner.exchanges.get(id) {
            return Ok(Some(exchange.clone()));
        }
        if inner.db.is_none() {
            return Ok(None);
        }
        load_persisted_locked(&mut inner, id)
    }

    /// Deletes every exchange from the cache and the database.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] when the rows cannot be deleted.
    pub fn clear(&self) -> Result<()> {
        if refuse_reentrancy(self, "clear") {
            return Ok(());
        }
        let event = {
            let mut inner = self.inner.lock();
            if let Some(db) = inner.db.as_mut()
                && let Err(err) = db
                    .execute("DELETE FROM exchanges", [])
                    .context("clearing rows")
            {
                inner.last_error = Some(err.to_string());
                return Err(err);
            }
            inner.order.clear();
            inner.exchanges.clear();
            inner.last_error = None;
            *self.refusals.0.lock() = None;
            inner.generation += 1;
            inner.next_number = 1;
            StoreEvent {
                kind: StoreEventKind::Cleared,
                id: None,
            }
        };
        self.publish(event);
        Ok(())
    }

    /// Groups the stored exchanges by conversation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] when the aggregate query fails.
    pub fn conversations(&self) -> Result<Vec<ConversationSummary>> {
        if refuse_reentrancy(self, "conversations") {
            return Ok(Vec::new());
        }
        let mut inner = self.inner.lock();
        if let Some(db) = inner.db.as_mut() {
            let rows = {
                let mut stmt = db
                    .prepare(
                        "SELECT conversation_id, COUNT(*), MAX(started_at_ms),
                                COALESCE(SUM(request_bytes), 0), COALESCE(SUM(response_bytes), 0)
                         FROM exchanges GROUP BY conversation_id ORDER BY MAX(started_at_ms) DESC",
                    )
                    .context("preparing the conversation query")?;
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
                    .context("querying conversations")?;
                let mut collected = Vec::new();
                for row in mapped {
                    collected.push(row.context("reading a conversation row")?);
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

    /// The store's database path and last persistence failure.
    #[must_use]
    pub fn status(&self) -> StoreStatus {
        if refuse_reentrancy(self, "status") {
            return StoreStatus::default();
        }
        let inner = self.inner.lock();
        let refused = self.refusals.0.lock().clone();
        StoreStatus {
            database_path: inner.database_path.clone(),
            // A refused reentrant call outranks a persistence error: it means the
            // capture path violated the store's contract, so it is not hidden by a
            // later successful write.
            last_error: refused.or_else(|| inner.last_error.clone()),
        }
    }

    /// Subscribes to the store's change events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<StoreEvent> {
        self.events.subscribe()
    }

    /// Records a capture-side problem for the debug UI's `/api/status`, so a
    /// dropped capture job is visible instead of silently missing.
    pub fn note_error(&self, message: &str) {
        if refuse_reentrancy(self, "note_error") {
            return;
        }
        self.inner.lock().last_error = Some(message.to_owned());
    }

    fn publish(&self, event: StoreEvent) {
        // No subscriber is a normal state (the debug UI is closed).
        let _ = self.events.send(event);
    }
}

/// Where the store keeps its database and the last write failure, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreStatus {
    /// Absolute path of the SQLite file; empty for the in-memory store.
    pub database_path: String,
    /// Last persistence error, cleared by the next successful write.
    pub last_error: Option<String>,
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

/// Persists an exchange row: the summary columns the list API needs plus the
/// full payload for the detail view. Both are written together so a summary
/// never disagrees with its payload.
fn persist_locked(inner: &mut Inner, id: &str) {
    let (Some(db), Some(exchange)) = (inner.db.as_mut(), inner.exchanges.get(id)) else {
        return;
    };
    let written = (|| -> Result<()> {
        let payload = serde_json::to_vec(exchange).context("encoding an exchange")?;
        let summary = serde_json::to_vec(&exchange.summary).context("encoding a summary")?;
        db.execute(
            "INSERT INTO exchanges (
                id, started_at_ms, conversation_id, request_id, state, request_bytes,
                response_bytes, payload_json, summary_json, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(id) DO UPDATE SET
                started_at_ms = excluded.started_at_ms,
                conversation_id = excluded.conversation_id,
                request_id = excluded.request_id,
                state = excluded.state,
                request_bytes = excluded.request_bytes,
                response_bytes = excluded.response_bytes,
                payload_json = excluded.payload_json,
                summary_json = excluded.summary_json,
                updated_at_ms = excluded.updated_at_ms",
            params![
                exchange.summary.id,
                exchange.summary.started_at.timestamp_millis(),
                exchange.summary.conversation_id,
                exchange.summary.request_id,
                exchange.summary.state.label(),
                exchange.summary.request_bytes,
                exchange.summary.response_bytes,
                payload,
                summary,
                Utc::now().timestamp_millis()
            ],
        )
        .context("writing an exchange row")
        .map(|_changed| ())
    })();
    match written {
        Ok(()) => inner.last_error = None,
        Err(err) => inner.last_error = Some(err.to_string()),
    }
}

/// Loads one exchange from SQLite into the in-memory window.
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
        .context("loading an exchange row")?;
    let Some((payload, conversation_id)) = row else {
        return Ok(None);
    };
    let mut persisted: Exchange =
        serde_json::from_slice(&payload).context("decoding a stored exchange")?;
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
    if conversation_id.is_empty()
        && let Some(found) = inner
            .exchanges
            .values()
            .find(|candidate| {
                candidate.summary.request_id == request_id
                    && !candidate.summary.conversation_id.is_empty()
            })
            .map(|candidate| candidate.summary.conversation_id.clone())
    {
        conversation_id = found;
    }
    if conversation_id.is_empty()
        && let Some(db) = inner.db.as_mut()
        && let Ok(found) = db.query_row(
            "SELECT conversation_id FROM exchanges
             WHERE request_id = ? AND conversation_id != ''
             ORDER BY started_at_ms DESC LIMIT 1",
            [request_id],
            |row| row.get::<_, String>(0),
        )
    {
        conversation_id = found;
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
        && let Err(err) = db
            .execute(
                "UPDATE exchanges SET conversation_id = ?, updated_at_ms = ?
                 WHERE request_id = ? AND conversation_id = ''",
                params![conversation_id, Utc::now().timestamp_millis(), request_id],
            )
            .context("associating stored rows with a conversation")
    {
        inner.last_error = Some(err.to_string());
    }
}

/// Sorts the request's headers by name, hiding credential values.
#[must_use]
pub fn sorted_headers(headers: &http::HeaderMap) -> Vec<Header> {
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in headers {
        let value = value
            .to_str()
            .map_or_else(|_| "<non-utf8>".to_owned(), str::to_owned);
        grouped
            .entry(name.as_str().to_owned())
            .or_default()
            .push(value);
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
