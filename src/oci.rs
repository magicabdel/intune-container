//! Pure-Rust OCI image acquisition for the experimental rootless backend.
//!
//! Pulls an image straight from the registry over HTTPS (no docker/podman) and
//! extracts its filesystem layers into a user-owned rootfs directory, honoring
//! overlay whiteouts. This is the Phase 1 building block of the rootless
//! migration; it does not yet boot or wire into the live commands.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use oci_client::{manifest, secrets::RegistryAuth, Client, Reference};

/// Pull `image` (e.g. `ghcr.io/magicabdel/intune-container:latest`) and extract
/// its layers into `dest`, which is created if missing. Blocking wrapper around
/// the async pull so callers don't need their own runtime.
pub fn pull_rootfs(image: &str, dest: &Path) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(pull_rootfs_async(image, dest))
}

async fn pull_rootfs_async(image: &str, dest: &Path) -> Result<()> {
    let reference: Reference = image.parse().context("invalid image reference")?;
    let client = Client::default();
    let auth = RegistryAuth::Anonymous;

    let accepted = vec![
        manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
        manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
        "application/vnd.oci.image.layer.v1.tar+zstd",
        "application/vnd.oci.image.layer.v1.tar",
    ];

    let image_data = client
        .pull(&reference, &auth, accepted)
        .await
        .with_context(|| format!("failed to pull {image}"))?;

    fs::create_dir_all(dest)
        .with_context(|| format!("failed to create rootfs dir {}", dest.display()))?;

    // Layers are applied in order; later layers (incl. whiteouts) override earlier.
    for layer in &image_data.layers {
        extract_layer(&layer.media_type, &layer.data, dest)
            .context("failed to extract image layer")?;
    }

    Ok(())
}

/// Extract one image layer into `dest`, choosing the decompressor by media type
/// (gzip via `flate2`, zstd via the pure-Rust `ruzstd`, or uncompressed tar).
fn extract_layer(media_type: &str, blob: &[u8], dest: &Path) -> Result<()> {
    if media_type.ends_with("+gzip")
        || media_type.ends_with(".gzip")
        || media_type.ends_with(".tar.gzip")
    {
        unpack_tar(flate2::read::GzDecoder::new(blob), dest)
    } else if media_type.ends_with("+zstd") {
        let decoder =
            zstd::stream::read::Decoder::new(blob).context("failed to init zstd decoder")?;
        unpack_tar(decoder, dest)
    } else {
        // Assume an uncompressed tar layer.
        unpack_tar(blob, dest)
    }
}

/// Apply a tar stream to `dest`, honoring overlayfs whiteout semantics
/// (`.wh.<name>` deletes; `.wh..wh..opq` clears a directory).
fn unpack_tar<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    // Don't apply xattrs: `security.capability` needs privilege and fails for an
    // unprivileged extract. File capabilities are re-applied at runtime inside
    // the user namespace where they're valid.
    archive.set_unpack_xattrs(false);
    // Don't chown to the image's uids/gids — extract as the current user; the
    // rootless uid-map makes those appear as container-root at runtime.
    archive.set_preserve_ownerships(false);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();

        if let Some(target) = name.strip_prefix(".wh.") {
            let parent = path.parent().unwrap_or_else(|| Path::new(""));
            if target == ".wh..opq" {
                // Opaque directory: drop everything currently under `parent`.
                let dir = dest.join(parent);
                if dir.is_dir() {
                    for child in fs::read_dir(&dir)?.flatten() {
                        remove_any(&child.path());
                    }
                }
            } else {
                remove_any(&dest.join(parent).join(target));
            }
            continue;
        }

        // Later layers override earlier ones: replace an existing non-directory
        // target so a read-only file from a prior layer can't block the write.
        let full = dest.join(&path);
        if full.starts_with(dest) && !entry.header().entry_type().is_dir() {
            remove_any(&full);
        }

        entry
            .unpack_in(dest)
            .with_context(|| format!("failed to unpack {}", path.display()))?;
    }

    Ok(())
}

fn remove_any(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::pull_rootfs;
    use std::fs;

    /// Real end-to-end pull+extract against the published image. Network- and
    /// disk-heavy, so it's ignored by default:
    ///   cargo test --lib --features rootless pull_and_extract -- --ignored --nocapture
    #[test]
    #[ignore = "downloads the full image; run manually"]
    fn pull_and_extract() {
        let dir = std::env::temp_dir().join("intune-oci-test");
        let _ = fs::remove_dir_all(&dir);
        pull_rootfs("ghcr.io/magicabdel/intune-container:latest", &dir).unwrap();
        assert!(
            dir.join("sbin/init").exists() || dir.join("usr/bin/intune-portal").exists(),
            "extracted rootfs is missing expected files"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
