//! CLI-only bootstrapper for the packaged desktop interface.
//!
//! The CLI binary is a small, statically-linked executable with no GUI compiled
//! in. When the user asks for the interface (`intune-container gui`, or no
//! subcommand on a graphical session), this module downloads the AppImage that
//! matches the CLI's own version from the GitHub release — in pure Rust, no
//! shelling out — installs it under `~/.local/share/intune-container`, and
//! launches it. So one portable binary can still bring up the full desktop app
//! on demand.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// `owner/repo` the release assets are published under.
const REPO: &str = "magicabdel/intune-container";

/// Whether a graphical session is available, so launching a GUI makes sense.
pub fn has_display() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

/// Ensure the desktop app is installed, then launch it.
pub fn install_and_launch() -> Result<()> {
    let path = appimage_path()?;
    if !path.exists() {
        eprintln!("Desktop interface not installed — downloading it (one-time)...");
        download_appimage(&path).context("failed to download the desktop interface")?;
        write_desktop_entry(&path).ok();
        eprintln!("\u{2713} Installed to {}", path.display());
    }
    launch(&path)
}

/// Where the downloaded AppImage lives.
fn appimage_path() -> Result<PathBuf> {
    let dir = data_dir().context("cannot determine ~/.local/share (HOME unset?)")?;
    Ok(dir
        .join("intune-container")
        .join("intune-container.AppImage"))
}

fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// Download the AppImage for this CLI's exact version from its GitHub release.
fn download_appimage(dest: &Path) -> Result<()> {
    let url = resolve_appimage_url().context("could not find an AppImage in the release")?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create install dir")?;
    }

    let bytes = block_on(async {
        let client = http_client()?;
        let resp = client
            .get(&url)
            .send()
            .await
            .context("download request failed")?
            .error_for_status()
            .context("download returned an error status")?;
        resp.bytes().await.context("reading download body")
    })?;

    // Write to a temp file, mark it executable, then rename into place so an
    // interrupted download never leaves a half-written "installed" AppImage.
    let tmp = dest.with_extension("partial");
    std::fs::write(&tmp, &bytes).context("write AppImage")?;
    set_executable(&tmp)?;
    std::fs::rename(&tmp, dest).context("install AppImage")?;
    Ok(())
}

/// Query the GitHub release for this version and pick the AppImage asset.
fn resolve_appimage_url() -> Result<String> {
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let api = format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}");

    let body: serde_json::Value = block_on(async {
        let client = http_client()?;
        let resp = client
            .get(&api)
            .send()
            .await
            .context("release query failed")?
            .error_for_status()
            .with_context(|| format!("no published release for {tag}"))?;
        resp.json::<serde_json::Value>()
            .await
            .context("parsing release JSON")
    })?;

    let urls = body
        .get("assets")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("browser_download_url").and_then(|u| u.as_str()))
        .filter(|u| u.ends_with(".AppImage"));

    // Prefer the 64-bit build when several are present; otherwise take the first.
    let mut first: Option<String> = None;
    for u in urls {
        let lower = u.to_lowercase();
        if lower.contains("amd64") || lower.contains("x86_64") || lower.contains("x86-64") {
            return Ok(u.to_owned());
        }
        first.get_or_insert_with(|| u.to_owned());
    }
    first.with_context(|| format!("release {tag} has no AppImage asset"))
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // GitHub's API rejects requests without a User-Agent.
        .user_agent(concat!("intune-container/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .build()
        .context("build HTTP client")
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(fut)
}

fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .context("stat AppImage")?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).context("chmod AppImage")
}

/// Write a desktop launcher so the app appears in menus (best-effort).
fn write_desktop_entry(appimage: &Path) -> Result<()> {
    let dir = data_dir().context("no data dir")?.join("applications");
    std::fs::create_dir_all(&dir)?;
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Intune Container\n\
         Comment=Manage Microsoft Intune in a rootless container\n\
         Exec={} %U\n\
         Icon=intune-container\n\
         Terminal=false\n\
         Categories=Utility;\n",
        appimage.display()
    );
    std::fs::write(dir.join("intune-container.desktop"), entry)?;
    Ok(())
}

/// Launch the AppImage detached, capturing its output to the GUI log. Uses
/// `--appimage-extract-and-run` when FUSE is missing (the most common failure),
/// and reports clearly if it can't start at all (e.g. WebKitGTK not installed).
fn launch(appimage: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(appimage);
    if !fuse_available() {
        cmd.arg("--appimage-extract-and-run");
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    match open_log() {
        Some(file) => {
            cmd.stderr(Stdio::from(file));
        }
        None => {
            cmd.stderr(Stdio::null());
        }
    }
    // SAFETY: `setsid` takes no arguments; detaching into a new session removes
    // the controlling terminal so the app survives this CLI process exiting.
    unsafe {
        cmd.pre_exec(|| {
            let _ = nix::unistd::setsid();
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .context("failed to launch the desktop interface")?;

    // Give it a moment to fail fast (missing libraries exit immediately); if it's
    // still alive after that, consider the launch successful and let it run.
    std::thread::sleep(Duration::from_millis(1200));
    match child
        .try_wait()
        .context("waiting on the interface process")?
    {
        Some(status) if !status.success() => bail!(
            "the desktop interface exited immediately ({status}).\n\
             It needs WebKitGTK/GTK; on Debian/Ubuntu install:\n  \
             sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0 libayatana-appindicator3-1\n\
             See the log at {}",
            log_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        _ => {
            eprintln!("\u{2713} Interface launched — you can close this terminal.");
            Ok(())
        }
    }
}

/// Whether the AppImage runtime's FUSE mount will work (a `fusermount` binary on
/// `PATH` and `/dev/fuse` present).
fn fuse_available() -> bool {
    if !Path::new("/dev/fuse").exists() {
        return false;
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join("fusermount").exists() || dir.join("fusermount3").exists())
}

fn log_path() -> Option<PathBuf> {
    Some(data_dir()?.join("intune-container").join("gui.log"))
}

fn open_log() -> Option<std::fs::File> {
    let path = log_path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}
