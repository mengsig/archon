use archon_core::acceptance::RequirementResult;
use archon_core::error::ArchonError;
use archon_core::intent::ApprovedIntent;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ImageProof {
    pub path: PathBuf,
    pub sha256: String,
}

pub(super) fn read(path: &Path) -> Result<Vec<u8>, ArchonError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| ArchonError::Invalid(format!("image {}: {e}", path.display())))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_IMAGE_BYTES {
        return Err(ArchonError::Invalid(format!(
            "image {} must be a nonempty file of at most 20 MiB",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| ArchonError::io(format!("read image {}", path.display()), e))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(ArchonError::Invalid("image grew beyond 20 MiB".into()));
    }
    if archon_core::attachment::sniff_media_type(&bytes).is_none() {
        return Err(ArchonError::Invalid(format!(
            "{} is not a PNG, JPEG, GIF or WebP",
            path.display()
        )));
    }
    assert!(!bytes.is_empty());
    assert!(bytes.len() as u64 <= MAX_IMAGE_BYTES);
    Ok(bytes)
}

pub(super) fn hash(path: &Path) -> Result<String, ArchonError> {
    Ok(Sha256::digest(read(path)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

pub(super) fn prove(
    record: &ApprovedIntent,
    results: &[RequirementResult],
    workspace: &Path,
) -> Result<Vec<ImageProof>, ArchonError> {
    let mut proofs = Vec::new();
    for result in results {
        for pair in &result.visual_pairs {
            let reference = record
                .images
                .iter()
                .find(|image| image.id == pair.reference)
                .ok_or_else(|| {
                    ArchonError::Invalid(format!("unknown reference {}", pair.reference))
                })?;
            let actual = hash(&reference.path)?;
            if actual != reference.sha256 {
                return Err(ArchonError::Invalid(format!(
                    "approved reference {} changed",
                    reference.id
                )));
            }
            let built = super::super::pages::inside_workspace(workspace, &pair.built)
                .map_err(ArchonError::Invalid)
                .and_then(|built| Ok((hash(&built)?, built)));
            let (sha256, built) = built.map_err(|e| {
                archon_pipeline::acceptance::results_refused(&format!(
                    "{} built image {}: {e}",
                    result.id, pair.built
                ))
            })?;
            proofs.push(ImageProof {
                sha256,
                path: built,
            });
            proofs.push(ImageProof {
                path: reference.path.clone(),
                sha256: actual,
            });
        }
    }
    assert_eq!(
        proofs.len(),
        results
            .iter()
            .map(|r| r.visual_pairs.len() * 2)
            .sum::<usize>()
    );
    assert!(proofs.iter().all(|p| p.sha256.len() == 64));
    Ok(proofs)
}

pub(super) fn verify(proofs: &[ImageProof]) -> Result<(), ArchonError> {
    for proof in proofs {
        if hash(&proof.path)? != proof.sha256 {
            return Err(ArchonError::Invalid(format!(
                "acceptance evidence {} changed after build",
                proof.path.display()
            )));
        }
    }
    Ok(())
}
