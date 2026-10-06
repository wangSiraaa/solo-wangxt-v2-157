-- Embedded migrations applied at startup. Every table is tenant scoped.

CREATE TABLE IF NOT EXISTS tenants (
    id          TEXT PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS plugins (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    name            TEXT NOT NULL,
    wasm_bytes      BYTEA NOT NULL,
    sha256          TEXT NOT NULL,
    contract        JSONB NOT NULL,
    interface       JSONB NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    plugin_id       TEXT NOT NULL REFERENCES plugins(id),
    status          TEXT NOT NULL CHECK (status IN ('pending','running','succeeded','failed')),
    error_stage     TEXT CHECK (error_stage IS NULL OR error_stage IN
                        ('validation','instantiation','invocation')),
    error_code      TEXT,
    error_message   TEXT,
    output          JSONB,
    fuel_consumed   BIGINT NOT NULL DEFAULT 0,
    duration_ms     BIGINT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ,
    -- Database-enforced invariant: no half results.
    CONSTRAINT tasks_success_needs_output CHECK
        (status <> 'succeeded' OR output IS NOT NULL),
    CONSTRAINT tasks_failure_needs_stage CHECK
        (status <> 'failed' OR error_stage IS NOT NULL),
    CONSTRAINT tasks_no_output_unless_done CHECK
        (output IS NULL OR status = 'succeeded')
);

CREATE INDEX IF NOT EXISTS idx_tasks_tenant ON tasks (tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_plugins_tenant ON plugins (tenant_id, created_at DESC);
