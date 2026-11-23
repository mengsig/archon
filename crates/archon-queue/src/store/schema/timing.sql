CREATE TABLE queue_reason_intervals (
    seq        INTEGER PRIMARY KEY,
    item       TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    reason     TEXT NOT NULL CHECK (reason IN ('dependency', 'cap', 'budget', 'pause', 'quota', 'ready', 'other')),
    started_at TEXT NOT NULL,
    ended_at   TEXT CHECK (ended_at >= started_at)
);
CREATE UNIQUE INDEX queue_reason_open ON queue_reason_intervals (item) WHERE ended_at IS NULL;
CREATE INDEX queue_reason_by_item ON queue_reason_intervals (item);
CREATE INDEX queue_reason_by_end ON queue_reason_intervals (ended_at);

INSERT INTO queue_reason_intervals (item, reason, started_at)
SELECT id, 'other', max(updated_at, strftime('%Y-%m-%dT%H:%M:%f000000Z', 'now'))
FROM items WHERE state = 'queued';

ALTER TABLE run_pull_requests ADD COLUMN created_at TEXT;
ALTER TABLE run_pull_requests ADD COLUMN checks_started_at TEXT;
ALTER TABLE run_pull_requests ADD COLUMN checks_finished_at TEXT;
ALTER TABLE run_pull_requests ADD COLUMN merged_at TEXT;
