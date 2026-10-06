-- Schema for the plugin host. Plugins are content-addressed by their SHA-256
-- digest; tasks carry a strict state machine enforced by a CHECK constraint so
-- a failed task can never persist half of a success.

CREATE TABLE IF NOT EXISTS plugins (
    id          UUID PRIMARY KEY,
    tenant_id   TEXT        NOT NULL,
    name        TEXT        NOT NULL,
    wasm        BYTEA       NOT NULL,           -- original module bytes
    sha256      TEXT        NOT NULL,           -- hex digest of `wasm`
    size_bytes  BIGINT      NOT NULL,
    contract    JSONB       NOT NULL,           -- JSON Schema for task input
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS plugins_tenant_idx ON plugins (tenant_id);
CREATE INDEX IF NOT EXISTS plugins_digest_idx ON plugins (sha256);

CREATE TABLE IF NOT EXISTS tasks (
    id            UUID PRIMARY KEY,
    plugin_id     UUID        NOT NULL REFERENCES plugins (id),
    tenant_id     TEXT        NOT NULL,
    status        TEXT        NOT NULL
                  CHECK (status IN ('pending', 'running', 'succeeded', 'failed')),
    input         JSONB       NOT NULL,
    output        JSONB,                          -- set only on success
    error_kind    TEXT,                           -- set only on failure
    error_message TEXT,
    fuel_consumed BIGINT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at    TIMESTAMPTZ,
    finished_at   TIMESTAMPTZ,

    -- A task is either fully successful (output, no error) or fully failed
    -- (error, no output). There is no representable "partial success".
    CONSTRAINT tasks_outcome_integrity CHECK (
        (status = 'succeeded' AND output IS NOT NULL AND error_kind IS NULL AND finished_at IS NOT NULL)
        OR (status = 'failed' AND output IS NULL AND error_kind IS NOT NULL AND finished_at IS NOT NULL)
        OR (status IN ('pending', 'running') AND output IS NULL AND error_kind IS NULL AND finished_at IS NULL)
    )
);
CREATE INDEX IF NOT EXISTS tasks_tenant_idx ON tasks (tenant_id);
CREATE INDEX IF NOT EXISTS tasks_plugin_idx ON tasks (plugin_id);
CREATE INDEX IF NOT EXISTS tasks_status_idx ON tasks (status) WHERE status IN ('pending', 'running');
