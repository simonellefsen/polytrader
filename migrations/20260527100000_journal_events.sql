-- General journal events for audit artifacts that are not paper orders/fills or Hermes reflections.
-- Used first for real-CLOB dry-run intent validation. This table is append-only by convention.

CREATE TABLE IF NOT EXISTS journal.events (
    id UUID PRIMARY KEY,
    event_type TEXT NOT NULL,
    source TEXT NOT NULL,
    severity TEXT NOT NULL DEFAULT 'info',
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_journal_events_type_created
    ON journal.events(event_type, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_journal_events_source_created
    ON journal.events(source, created_at DESC);

COMMENT ON TABLE journal.events IS 'Append-only audit events for decisions, diagnostics, dry-runs, and future non-paper trading safety gates.';
COMMENT ON COLUMN journal.events.payload IS 'Structured JSON payload; must not contain secrets or raw private keys.';
