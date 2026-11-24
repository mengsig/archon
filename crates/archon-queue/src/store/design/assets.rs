use archon_core::design::{DesignAsset, MAX_ITEM_ASSET_BYTES};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use super::{DesignAssetId, Store, corrupt, dec, enc, keys, parse_ts, read_sessions, settings};
use crate::store::codec::{SqlContext, from_json, to_i64, to_json, to_u64, ts, write_tx};

impl Store {
    pub fn design_asset_put(&mut self, asset: DesignAsset) -> Result<DesignAsset, ArchonError> {
        self.design_asset_publish(asset, None, |_| Ok(()))?
            .ok_or_else(|| {
                ArchonError::Invalid("this item's pictures are at the 200 MiB limit".into())
            })
    }

    /// Reuse an exact capture or add its record. Publish its file before committing
    /// while holding the writer lock shared with other publishers and the sweep.
    pub fn design_asset_publish(
        &mut self,
        asset: DesignAsset,
        sketch: Option<&str>,
        publish: impl FnOnce(&DesignAsset) -> Result<(), ArchonError>,
    ) -> Result<Option<DesignAsset>, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let target = asset
            .target
            .as_ref()
            .map(|t| to_json(t, "asset target"))
            .transpose()?;
        let stored =
            if let Some(id) = matching_capture(&tx, &asset, target.as_deref(), sketch)? {
                let updated = tx.execute(
                "UPDATE design_assets SET last_used_at = MAX(last_used_at, ?1) WHERE id = ?2",
                params![ts(&asset.created_at), id],
            ).ctx("renew pending picture")?;
                assert_eq!(
                    updated, 1,
                    "the matching capture exists in this transaction"
                );
                asset_by_id(&tx, &asset.item, &DesignAssetId::parse(&id)?)?
                    .expect("the matching capture belongs to the item")
            } else {
                if !fits_asset_cap(&tx, &asset, sketch)? {
                    return Ok(None);
                }
                insert_asset(&tx, &asset, target.as_deref(), sketch)?;
                asset_by_id(&tx, &asset.item, &asset.id)?.expect("the inserted capture exists")
            };
        debug_assert_eq!(stored.item, asset.item);
        debug_assert_eq!(stored.sha256, asset.sha256);
        publish(&stored)?;
        tx.commit().ctx("commit design asset")?;
        Ok(Some(stored))
    }

    pub fn design_asset(
        &self,
        item: &QueueItemId,
        id: &DesignAssetId,
    ) -> Result<Option<DesignAsset>, ArchonError> {
        asset_by_id(&self.conn, item, id)
    }

    /// The bytes occupied by the item's shared picture files and stored sketches.
    pub fn design_asset_bytes(&self, item: &QueueItemId) -> Result<u64, ArchonError> {
        asset_bytes(&self.conn, item)
    }

    /// Old captures that no note uses.
    pub fn design_unused_assets(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<Vec<DesignAsset>, ArchonError> {
        read_assets(
            &self.conn,
            &format!("WHERE {UNUSED_ASSET} ORDER BY last_used_at, id"),
            &[&ts(&older_than)],
        )
    }

    /// Recheck the candidate after taking the writer lock. Remove its shared
    /// file only when this was the last capture pointing to it.
    pub fn design_asset_sweep(
        &mut self,
        item: &QueueItemId,
        id: &DesignAssetId,
        older_than: DateTime<Utc>,
        remove_file: impl FnOnce(&DesignAsset) -> Result<(), ArchonError>,
    ) -> Result<bool, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let Some(asset) = asset_by_id(&tx, item, id)? else {
            return Ok(false);
        };
        let deleted = tx
            .execute(
                &format!(
                    "DELETE FROM design_assets WHERE {UNUSED_ASSET} AND item = ?2 AND id = ?3"
                ),
                params![ts(&older_than), item.as_str(), id.as_str()],
            )
            .ctx("sweep unused design asset")?;
        if deleted == 0 {
            return Ok(false);
        }
        assert_eq!(deleted, 1, "asset IDs are unique");
        assert_eq!(&asset.item, item);
        if picture_bytes(&tx, &asset)?.is_none() {
            remove_file(&asset)?;
        }
        tx.commit().ctx("commit asset sweep")?;
        Ok(true)
    }

    /// Remove everything the item holds; the pictures come back so their files can go.
    pub fn design_item_delete(
        &mut self,
        item: &QueueItemId,
    ) -> Result<Vec<DesignAsset>, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let assets = read_assets(
            &tx,
            "WHERE item = ?1 ORDER BY created_at",
            &[&item.as_str()],
        )?;
        for session in read_sessions(&tx, "WHERE item = ?1", &[&item.as_str()])? {
            settings::kv_delete(&tx, &keys::design_owed(&session.id))?;
        }
        for table in ["design_notes", "design_sessions", "design_assets"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE item = ?1"),
                [item.as_str()],
            )
            .ctx("delete design rows")?;
        }
        settings::kv_delete(&tx, &keys::design_seq(item))?;
        tx.execute(
            "DELETE FROM kv WHERE key >= ?1 AND key < ?2",
            params![
                keys::design_input(item, ""),
                format!("design_input:{item};")
            ],
        )
        .ctx("remove stored design note requests")?;
        tx.commit().ctx("commit design item delete")?;
        Ok(assets)
    }
}

const UNUSED_ASSET: &str = "last_used_at < ?1 AND NOT EXISTS (
    SELECT 1 FROM design_notes n WHERE n.item = design_assets.item
       AND design_assets.id IN (n.before_asset, n.after_asset, n.final_asset))";

fn matching_capture(
    conn: &Connection,
    asset: &DesignAsset,
    target: Option<&str>,
    sketch: Option<&str>,
) -> Result<Option<String>, ArchonError> {
    conn.query_row(
        "SELECT id FROM design_assets
         WHERE item = ?1 AND sha256 = ?2 AND mime = ?3 AND bytes = ?4 AND width = ?5 AND height = ?6
           AND path = ?7 AND source = ?8 AND target_json IS ?9 AND note IS ?10 AND commit_sha IS ?11
           AND sketch IS ?12
         ORDER BY created_at, id LIMIT 1",
        params![
            asset.item.as_str(),
            asset.sha256,
            asset.mime,
            to_i64(asset.bytes, "asset bytes")?,
            asset.width,
            asset.height,
            asset.path,
            enc(&asset.source),
            target,
            asset.note,
            asset.commit,
            sketch,
        ],
        |row| row.get(0),
    )
    .optional()
    .ctx("find exact design capture")
}

fn insert_asset(
    conn: &Connection,
    asset: &DesignAsset,
    target: Option<&str>,
    sketch: Option<&str>,
) -> Result<(), ArchonError> {
    let inserted = conn
        .execute(
            "INSERT INTO design_assets (id, item, sha256, mime, bytes, width, height, path,
             source, target_json, note, commit_sha, created_at, last_used_at, sketch)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, ?14)",
            params![
                asset.id.as_str(),
                asset.item.as_str(),
                asset.sha256,
                asset.mime,
                to_i64(asset.bytes, "asset bytes")?,
                asset.width,
                asset.height,
                asset.path,
                enc(&asset.source),
                target,
                asset.note,
                asset.commit,
                ts(&asset.created_at),
                sketch,
            ],
        )
        .ctx("insert design asset")?;
    assert_eq!(inserted, 1, "each new capture inserts one row");
    Ok(())
}

fn asset_bytes(conn: &Connection, item: &QueueItemId) -> Result<u64, ArchonError> {
    // The daemon uses .png for PNG and .jpg for other media types.
    let total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(file_bytes + sketch_bytes), 0) FROM (
             SELECT MAX(bytes) AS file_bytes, COALESCE(SUM(octet_length(sketch)), 0) AS sketch_bytes
             FROM design_assets WHERE item = ?1 GROUP BY sha256, mime = 'image/png'
         )",
            [item.as_str()],
            |row| row.get(0),
        )
        .ctx("sum design asset bytes")?;
    to_u64(total, "asset bytes")
}

fn picture_bytes(conn: &Connection, asset: &DesignAsset) -> Result<Option<u64>, ArchonError> {
    let bytes: Option<i64> = conn
        .query_row(
            "SELECT MAX(bytes) FROM design_assets WHERE item = ?1 AND sha256 = ?2
             AND (mime = 'image/png') = (?3 = 'image/png')",
            params![asset.item.as_str(), asset.sha256, asset.mime],
            |row| row.get(0),
        )
        .ctx("check shared design picture")?;
    bytes.map(|n| to_u64(n, "shared asset bytes")).transpose()
}

fn fits_asset_cap(
    conn: &Connection,
    asset: &DesignAsset,
    sketch: Option<&str>,
) -> Result<bool, ArchonError> {
    let shared = picture_bytes(conn, asset)?.unwrap_or(0);
    let extra = asset
        .bytes
        .saturating_sub(shared)
        .saturating_add(sketch.map_or(0, str::len) as u64);
    Ok(extra == 0 || extra <= MAX_ITEM_ASSET_BYTES.saturating_sub(asset_bytes(conn, &asset.item)?))
}

const ASSET_COLUMNS: &str = "id, item, sha256, mime, bytes, width, height, path, source,
    target_json, note, commit_sha, created_at";

fn read_assets(
    conn: &Connection,
    clause: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<DesignAsset>, ArchonError> {
    let sql = format!("SELECT {ASSET_COLUMNS} FROM design_assets {clause}");
    let mut statement = conn.prepare_cached(&sql).ctx("prepare design assets")?;
    let rows = statement
        .query_map(args, |row| {
            let item: String = row.get(1)?;
            let bytes: i64 = row.get(4)?;
            let width: i64 = row.get(5)?;
            let height: i64 = row.get(6)?;
            let source: String = row.get(8)?;
            let target: Option<String> = row.get(9)?;
            let created: String = row.get(12)?;
            Ok((|| -> Result<DesignAsset, ArchonError> {
                fn ctx<T>(r: rusqlite::Result<T>) -> Result<T, ArchonError> {
                    r.ctx("read design asset")
                }
                Ok(DesignAsset {
                    id: DesignAssetId::try_from(ctx(row.get::<_, String>(0))?)?,
                    item: QueueItemId::parse(&item)?,
                    sha256: ctx(row.get(2))?,
                    mime: ctx(row.get(3))?,
                    bytes: to_u64(bytes, "asset bytes")?,
                    width: u32::try_from(width).map_err(corrupt("asset width"))?,
                    height: u32::try_from(height).map_err(corrupt("asset height"))?,
                    path: ctx(row.get(7))?,
                    source: dec(&source, "asset source")?,
                    target: target.map(|t| from_json(&t, "asset target")).transpose()?,
                    note: ctx(row.get(10))?,
                    commit: ctx(row.get(11))?,
                    created_at: parse_ts(&created)?,
                })
            })())
        })
        .ctx("read design assets")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.ctx("read design assets")??);
    }
    Ok(out)
}

fn asset_by_id(
    conn: &Connection,
    item: &QueueItemId,
    id: &DesignAssetId,
) -> Result<Option<DesignAsset>, ArchonError> {
    Ok(read_assets(
        conn,
        "WHERE item = ?1 AND id = ?2",
        &[&item.as_str(), &id.as_str()],
    )?
    .pop())
}
