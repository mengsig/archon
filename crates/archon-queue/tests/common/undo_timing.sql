DROP TABLE queue_reason_intervals;
ALTER TABLE run_pull_requests DROP COLUMN created_at;
ALTER TABLE run_pull_requests DROP COLUMN checks_started_at;
ALTER TABLE run_pull_requests DROP COLUMN checks_finished_at;
ALTER TABLE run_pull_requests DROP COLUMN merged_at;
DELETE FROM schema_migrations WHERE version = 28;
