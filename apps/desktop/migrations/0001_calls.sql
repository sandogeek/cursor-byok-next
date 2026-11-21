CREATE TABLE IF NOT EXISTS service_settings (
    setting_key TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

INSERT OR IGNORE INTO service_settings(setting_key, value_json, updated_at_ms)
VALUES ('llm_detailed_logging', 'false', 0);

CREATE TABLE IF NOT EXISTS llm_calls (
    call_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    provider_call_index INTEGER NOT NULL,
    model_hash TEXT,
    provider_type TEXT NOT NULL,
    provider_url TEXT NOT NULL,
    request_type TEXT NOT NULL,
    request_url TEXT NOT NULL,
    model_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    reasoning_effort TEXT,
    fast INTEGER NOT NULL DEFAULT 0 CHECK (fast IN (0, 1)),
    status TEXT NOT NULL,
    finish_reason TEXT,
    created_at_ms INTEGER NOT NULL,
    request_started_at_ms INTEGER,
    response_headers_at_ms INTEGER,
    first_event_at_ms INTEGER,
    first_text_at_ms INTEGER,
    first_valid_response_at_ms INTEGER,
    finished_at_ms INTEGER,
    queue_ms INTEGER,
    response_headers_ms INTEGER,
    ttft_ms INTEGER,
    ttfr_ms INTEGER,
    duration_ms INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    total_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    reasoning_tokens INTEGER,
    usage_json TEXT,
    message_count INTEGER NOT NULL,
    tool_count INTEGER NOT NULL,
    request_bytes INTEGER,
    response_bytes INTEGER NOT NULL DEFAULT 0,
    response_chunk_count INTEGER NOT NULL DEFAULT 0,
    http_status INTEGER,
    error_kind TEXT,
    error_message TEXT,
    detailed INTEGER NOT NULL CHECK (detailed IN (0, 1)),
    request_truncated INTEGER NOT NULL DEFAULT 0 CHECK (request_truncated IN (0, 1)),
    response_truncated INTEGER NOT NULL DEFAULT 0 CHECK (response_truncated IN (0, 1))
);

CREATE INDEX IF NOT EXISTS llm_calls_created ON llm_calls(created_at_ms DESC);
CREATE INDEX IF NOT EXISTS llm_calls_run ON llm_calls(run_id, provider_call_index);
CREATE INDEX IF NOT EXISTS llm_calls_model ON llm_calls(model_hash, created_at_ms DESC);

CREATE TABLE IF NOT EXISTS llm_call_requests (
    call_id TEXT PRIMARY KEY,
    headers_json TEXT NOT NULL,
    body_json TEXT NOT NULL,
    byte_count INTEGER NOT NULL,
    FOREIGN KEY(call_id) REFERENCES llm_calls(call_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS llm_call_response_chunks (
    call_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    received_offset_ms INTEGER NOT NULL,
    data BLOB NOT NULL,
    byte_count INTEGER NOT NULL,
    PRIMARY KEY(call_id, seq),
    FOREIGN KEY(call_id) REFERENCES llm_calls(call_id) ON DELETE CASCADE
);
