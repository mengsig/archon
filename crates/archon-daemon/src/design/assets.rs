//! Pictures: pending crops, uploads, after and final pictures, stored once
//! each under `data_dir/design/assets/<item>/` and recorded in the store.

use archon_core::design::{
    AssetSource, DesignAsset, DesignAssetId, DesignSession, MAX_ENCODED_IMAGE_BYTES,
    MAX_IMAGE_SIDE, MAX_ITEM_ASSET_BYTES, SessionState, Target,
};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{DesignAssetView, Reply};
use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::DesignService;
use crate::context::DaemonContext;

/// Pending assets nobody uses are deleted after this long.
const UNUSED_AFTER: chrono::Duration = chrono::Duration::hours(1);

/// Width and height of a PNG, JPEG or WebP, read from its header.
pub(crate) fn dimensions(mime: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    match mime {
        "image/png" => {
            if bytes.len() < 24 {
                return None;
            }
            let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
            let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
            Some((w, h))
        }
        "image/jpeg" => {
            let mut i = 2;
            while i + 9 < bytes.len() {
                if bytes[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = bytes[i + 1];
                if marker == 0xFF {
                    i += 1;
                    continue;
                }
                if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                    let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                    return Some((w, h));
                }
                let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
                i += 2 + len;
            }
            None
        }
        "image/webp" => {
            if bytes.len() < 30 {
                return None;
            }
            match &bytes[12..16] {
                b"VP8X" => {
                    let w = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
                    let h = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
                    Some((w, h))
                }
                b"VP8L" => {
                    let b = &bytes[21..25];
                    let w = 1 + (((b[1] as u32 & 0x3F) << 8) | b[0] as u32);
                    let h = 1
                        + (((b[3] as u32 & 0xF) << 10)
                            | ((b[2] as u32) << 2)
                            | ((b[1] as u32 & 0xC0) >> 6));
                    Some((w, h))
                }
                b"VP8 " => {
                    let w = (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF) as u32;
                    let h = (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF) as u32;
                    Some((w, h))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The media type the bytes' magic numbers show, if it is one we keep.
pub(crate) fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(crate) struct NewAsset<'a> {
    pub source: AssetSource,
    pub target: Option<Target>,
    pub note: Option<String>,
    pub commit: Option<String>,
    pub mime: &'a str,
    pub bytes: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub sketch: Option<&'a str>,
}

impl DesignService {
    /// Keep a picture. `Ok(None)` when the item is at its asset cap and the
    /// picture is an after picture, which is skipped.
    pub(crate) async fn put_asset(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        new: NewAsset<'_>,
    ) -> Result<Option<DesignAsset>, ArchonError> {
        let size = new.bytes.len() as u64;
        let mut store = ctx.store.lock().await;
        if store.design_asset_bytes(item)? + size > MAX_ITEM_ASSET_BYTES {
            if new.source == AssetSource::After {
                tracing::info!(%item, "design: asset cap reached; the after picture is skipped");
                return Ok(None);
            }
            return Err(ArchonError::Invalid(
                "this item's pictures are at the 200 MiB limit".into(),
            ));
        }
        let sha256 = Sha256::digest(new.bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut asset = DesignAsset {
            id: DesignAssetId::new(),
            item: item.clone(),
            sha256,
            mime: new.mime.to_owned(),
            bytes: size,
            width: new.width,
            height: new.height,
            path: String::new(),
            source: new.source,
            target: new.target,
            note: new.note,
            commit: new.commit,
            created_at: archon_core::now(),
        };
        let path = self.asset_path(&asset);
        asset.path = path.display().to_string();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ArchonError::io(format!("creating {}", parent.display()), e))?;
        }
        if !path.exists() {
            std::fs::write(&path, new.bytes)
                .map_err(|e| ArchonError::io(format!("writing {}", path.display()), e))?;
        }
        if let Some(sketch) = new.sketch {
            let _ = std::fs::write(path.with_extension("json"), sketch);
        }
        Ok(Some(store.design_asset_put(asset)?))
    }

    pub(crate) fn asset_bytes(&self, asset: &DesignAsset) -> Result<Vec<u8>, ArchonError> {
        let path = self.asset_path(asset);
        std::fs::read(&path).map_err(|e| ArchonError::io(format!("reading {}", path.display()), e))
    }

    pub(crate) fn data_url(&self, asset: &DesignAsset) -> Result<String, ArchonError> {
        let bytes = self.asset_bytes(asset)?;
        Ok(format!(
            "data:{};base64,{}",
            asset.mime,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    }

    pub(crate) async fn asset_view(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &DesignAssetId,
    ) -> Result<Reply, ArchonError> {
        let asset = ctx
            .store
            .lock()
            .await
            .design_asset(item, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("picture {id}")))?;
        let caption = caption_of(&asset);
        Ok(Reply::DesignAsset(DesignAssetView {
            id: asset.id.clone(),
            data_url: self.data_url(&asset)?,
            width: asset.width,
            height: asset.height,
            caption,
        }))
    }

    /// `design_upload`: a picture the operator brings, checked by its bytes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn upload(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        media_type: &str,
        data: &str,
        name: Option<String>,
        sketch: Option<String>,
    ) -> Result<Reply, ArchonError> {
        if matches!(
            session.state,
            SessionState::Finishing | SessionState::Confirmed
        ) {
            return Err(super::finishing());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|_| ArchonError::Invalid("the picture is not valid base64".into()))?;
        if bytes.len() > MAX_ENCODED_IMAGE_BYTES {
            return Err(ArchonError::Invalid(
                "the picture is over 560 KiB; make it smaller".into(),
            ));
        }
        let mime = sniff(&bytes).ok_or_else(|| {
            ArchonError::Invalid("only PNG, JPEG and WebP pictures are accepted".into())
        })?;
        if !media_type.is_empty() && media_type != mime {
            return Err(ArchonError::Invalid(
                "the picture's bytes are not the type it says".into(),
            ));
        }
        let (width, height) = dimensions(mime, &bytes)
            .ok_or_else(|| ArchonError::Invalid("the picture's size cannot be read".into()))?;
        if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
            return Err(ArchonError::Invalid(format!(
                "the picture must be at most {MAX_IMAGE_SIDE} px on each side"
            )));
        }
        let source = if sketch.is_some() {
            AssetSource::Sketch
        } else {
            AssetSource::Upload
        };
        let sketch = sketch.map(|s| archon_core::design::clean_text(&s, 200_000));
        let asset = self
            .put_asset(
                ctx,
                &session.item,
                NewAsset {
                    source,
                    target: None,
                    note: name,
                    commit: None,
                    mime,
                    bytes: &bytes,
                    width,
                    height,
                    sketch: sketch.as_deref(),
                },
            )
            .await?
            .ok_or_else(|| ArchonError::Invalid("the picture was not kept".into()))?;
        let data_url = self.data_url(&asset)?;
        Ok(Reply::DesignResolved(
            archon_core::protocol::DesignResolvedView {
                target: upload_target(&asset),
                label: "uploaded picture".into(),
                asset: Some(asset.id),
                data_url: Some(data_url),
            },
        ))
    }

    /// Delete pending assets no note uses, after an hour.
    pub(crate) async fn sweep_assets(&self, ctx: &DaemonContext) -> Result<usize, ArchonError> {
        let mut store = ctx.store.lock().await;
        let old = store.design_unused_assets(archon_core::now() - UNUSED_AFTER)?;
        let count = old.len();
        for asset in old {
            store.design_asset_delete(&asset.item, &asset.id)?;
            // Records of different sources share one file per hash.
            if !store.design_asset_sha_held(&asset.item, &asset.sha256)? {
                let _ = std::fs::remove_file(self.asset_path(&asset));
            }
        }
        Ok(count)
    }
}

fn upload_target(asset: &DesignAsset) -> Target {
    Target {
        path: "/".into(),
        selector: None,
        role_name: None,
        component: None,
        rect: archon_core::design::Rect {
            x: 0.0,
            y: 0.0,
            w: asset.width as f64,
            h: asset.height as f64,
        },
        viewport: archon_core::design::Viewport {
            w: asset.width,
            h: asset.height,
            preset: archon_core::design::ViewportPreset::Desktop,
        },
        styles: Default::default(),
        region: true,
    }
}

/// "captured from the preview at commit 3f9c2a1, 14:32".
pub(crate) fn caption_of(asset: &DesignAsset) -> Option<String> {
    let commit = asset.commit.as_deref()?;
    Some(format!(
        "captured from the preview at commit {}, {}",
        super::short(commit),
        asset.created_at.format("%H:%M UTC")
    ))
}
