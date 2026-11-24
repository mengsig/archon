CREATE TABLE design_assets_v31 (
    id           TEXT PRIMARY KEY,
    item         TEXT NOT NULL,
    sha256       TEXT NOT NULL,
    mime         TEXT NOT NULL,
    bytes        INTEGER NOT NULL,
    width        INTEGER NOT NULL,
    height       INTEGER NOT NULL,
    path         TEXT NOT NULL,
    source       TEXT NOT NULL,
    target_json  TEXT,
    note         TEXT,
    commit_sha   TEXT,
    created_at   TEXT NOT NULL,
    last_used_at TEXT NOT NULL,
    sketch       TEXT
);

INSERT INTO design_assets_v31 (
    id, item, sha256, mime, bytes, width, height, path, source, target_json, note,
    commit_sha, created_at, last_used_at
)
SELECT id, item, sha256, mime, bytes, width, height, path, source, target_json, note,
       commit_sha, created_at, created_at
FROM design_assets;

DROP TABLE design_assets;
ALTER TABLE design_assets_v31 RENAME TO design_assets;
CREATE INDEX design_assets_by_item_hash ON design_assets (item, sha256);
