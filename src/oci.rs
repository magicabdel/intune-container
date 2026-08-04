//! Pure-Rust OCI image acquisition for the experimental rootless backend.
//!
//! Pulls an image straight from the registry over HTTPS (no docker/podman) and
//! extracts its filesystem layers into a user-owned rootfs directory, honoring
//! overlay whiteouts. This is the Phase 1 building block of the rootless
//! migration; it does not yet boot or wire into the live commands.

use std::fs;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use oci_client::{manifest, secrets::RegistryAuth, Client, Reference};

/// Name of the stamp file written at the root of an extracted rootfs, recording
/// the manifest digest of the image it came from. Lets callers tell whether the
/// registry has a newer `:latest` without re-downloading the layers.
const DIGEST_STAMP: &str = ".intune-image-digest";

/// Path of the digest stamp for `rootfs`.
pub fn digest_stamp(rootfs: &Path) -> PathBuf {
    rootfs.join(DIGEST_STAMP)
}

/// Digest of the image the rootfs at `rootfs` was extracted from, if known.
/// `None` for a missing/unstamped rootfs (e.g. extracted by an older version).
pub fn local_digest(rootfs: &Path) -> Option<String> {
    let raw = fs::read_to_string(digest_stamp(rootfs)).ok()?;
    let digest = raw.trim().to_string();
    (!digest.is_empty()).then_some(digest)
}

/// Record `digest` as the image the rootfs at `rootfs` came from.
pub fn write_local_digest(rootfs: &Path, digest: &str) -> Result<()> {
    let path = digest_stamp(rootfs);
    fs::write(&path, format!("{digest}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Current manifest digest of `image` in the registry, fetched with a single
/// HEAD request (no layer download). Requires network access.
pub fn remote_digest(image: &str) -> Result<String> {
    block_on(async move {
        let reference: Reference = image.parse().context("invalid image reference")?;
        Client::default()
            .fetch_manifest_digest(&reference, &RegistryAuth::Anonymous)
            .await
            .with_context(|| format!("failed to query the digest of {image}"))
    })
}

/// Pull `image` (e.g. `ghcr.io/magicabdel/intune-container:latest`) and extract
/// its layers into `dest`, which is created if missing. Blocking wrapper around
/// the async pull so callers don't need their own runtime. Returns the manifest
/// digest that was extracted (also stamped into `dest`).
pub fn pull_rootfs(image: &str, dest: &Path) -> Result<String> {
    block_on(pull_rootfs_async(image, dest))
}

/// Run one blocking OCI operation on a throwaway tokio runtime.
fn block_on<F: Future<Output = Result<T>>, T>(fut: F) -> Result<T> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(fut)
}

async fn pull_rootfs_async(image: &str, dest: &Path) -> Result<String> {
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

    // Prefer the digest the registry reported for this manifest; fall back to a
    // fresh HEAD only if `pull` didn't surface one.
    let digest = match image_data.digest {
        Some(digest) => digest,
        None => client
            .fetch_manifest_digest(&reference, &auth)
            .await
            .with_context(|| format!("failed to query the digest of {image}"))?,
    };
    write_local_digest(dest, &digest)?;

    Ok(digest)
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
    use super::{local_digest, pull_rootfs, remote_digest};
    use std::fs;

    const IMAGE: &str = "ghcr.io/magicabdel/intune-container:latest";

    /// Real end-to-end pull+extract against the published image. Network- and
    /// disk-heavy, so it's ignored by default:
    ///   cargo test --lib --features rootless pull_and_extract -- --ignored --nocapture
    #[test]
    #[ignore = "downloads the full image; run manually"]
    fn pull_and_extract() {
        let dir = std::env::temp_dir().join("intune-oci-test");
        let _ = fs::remove_dir_all(&dir);
        let digest = pull_rootfs(IMAGE, &dir).unwrap();
        assert!(
            dir.join("sbin/init").exists() || dir.join("usr/bin/intune-portal").exists(),
            "extracted rootfs is missing expected files"
        );
        assert!(digest.starts_with("sha256:"), "unexpected digest {digest}");
        assert_eq!(
            local_digest(&dir).as_deref(),
            Some(digest.as_str()),
            "the extracted rootfs should be stamped with the pulled digest"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The remote digest lookup is a HEAD request, so it's cheap — but still
    /// needs network, hence ignored by default.
    #[test]
    #[ignore = "requires network access; run manually"]
    fn digest_of_latest() {
        let digest = remote_digest(IMAGE).unwrap();
        assert!(digest.starts_with("sha256:"), "unexpected digest {digest}");
    }

    #[test]
    fn local_digest_is_none_without_a_stamp() {
        let dir = std::env::temp_dir().join("intune-oci-nostamp");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(local_digest(&dir), None, "missing rootfs has no digest");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(local_digest(&dir), None, "unstamped rootfs has no digest");
        super::write_local_digest(&dir, "sha256:abc").unwrap();
        assert_eq!(local_digest(&dir).as_deref(), Some("sha256:abc"));
        let _ = fs::remove_dir_all(&dir);
    }
}
