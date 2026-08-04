# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

## [0.2.2] - 2026-08-04

### Added

- **`update` command + automatic image updates.** The rootfs now records the
  digest of the image it was extracted from, and every start asks the registry
  (one HEAD request) whether the tag moved — if so, the image is re-pulled and
  the rootfs rebuilt before boot. Enrollment survives: device registration,
  tokens, and the keyring live outside the rootfs. `update --check` compares
  local vs. registry digests without downloading; `update --force` re-pulls
  unconditionally; `auto_update = false` in the config pins the rootfs. `status`
  now shows the image reference and digest. A registry hiccup never blocks a
  boot — the existing rootfs is kept.
  ([@magicabdel](https://github.com/magicabdel))
- **`autostart` command** (`enable`/`disable`/`status`) — run the container on
  login and at boot via a `systemd --user` unit: `start` on login, `stop` on
  logout, with lingering enabled so it also comes up at boot before any
  interactive session. Idempotent, reversible, and the unit pins the absolute
  path of the current binary so it keeps working regardless of `PATH` at boot.
  ([@theophile-wallez](https://github.com/theophile-wallez),
  [#2](https://github.com/magicabdel/intune-container/pull/2))

### Fixed

- **Sign-in silently lost minutes after boot** — every token call answered with
  no account while enrollment, the bus, and the keyring all read healthy. Three
  faults produced the one symptom: (1) `Xvfb` and the unlocking
  `gnome-keyring-daemon` ran as children of the session-setup exec and died with
  its cgroup — they now run as transient user units with `Restart=always`, and
  the keyring daemon takes the bus name with `--replace` instead of racing
  `pkill` against D-Bus activation; (2) stale broker POSIX semaphores in
  `/dev/shm` (shared with the host) left at 0 by a broker killed mid-call made
  `getAccounts` block 15 s and answer empty — boot now clears locks no live
  process maps; (3) `doctor` gained the two missing checks — *Virtual display*
  and *Broker locks* — and each broker check now names a distinct cause.
  ([@theophile-wallez](https://github.com/theophile-wallez),
  [#3](https://github.com/magicabdel/intune-container/pull/3))

## [0.2.1] - 2026-07-06

### Fixed

- **AppImage: blank window / `EGL_BAD_PARAMETER` on modern and hybrid GPUs**
  (e.g. Intel + NVIDIA laptops). The bundler was shipping host-managed graphics
  and Wayland libraries (`libepoxy`, `libwayland-*`) inside the AppImage, which
  shadowed the user's own copies and broke EGL initialization. A patched
  `linuxdeploy` GTK plugin now drops those libraries so the app uses the host's
  GPU stack, as a well-formed AppImage should. (see tauri-apps/tauri#9394)
- **GUI single instance.** Launching `intune-container` repeatedly no longer
  stacks up duplicate, tray-resident interface processes. A file lock guarantees
  a single interface; the D-Bus single-instance dedup was unreliable on some
  Wayland compositors.
- **Tray icon lingering after Quit.** The tray icon is now explicitly withdrawn
  on Quit instead of occasionally staying in the panel after the app exits.

### Added

- **`just bundle-appimage`** — build the desktop AppImage in an Ubuntu container,
  the same way CI does. Reproducible, needs no FUSE on the host, and sidesteps
  bleeding-edge host toolchains that trip up the AppImage tooling.

## [0.2.0] - 2026-06-25

### Changed

- **Rootless runtime.** The container now boots inside an unprivileged user
  namespace via a detached supervisor (`fork` + `unshare`, multi-id mapping with
  `newuidmap`/`newgidmap`, a delegated cgroup scope over the user's systemd
  manager, `pivot_root`). Commands enter it with `setns`. **No host root, no
  `sudo`, `systemd-nspawn`, `machinectl`, `nsenter`, Docker, or Podman** — the
  image is pulled with a built-in OCI client, and `destroy` leaves nothing
  privileged behind because nothing privileged is installed.
- Renamed the `daemon` command to **`start`**: starting the container now also
  makes browser SSO ready (installs the native-messaging host), so "start" means
  the same thing in the CLI, the window, and the tray.
- The browser SSO native-host bridge runs **inside** the container (via `setns`)
  against the container's own session bus; the host session bus is no longer
  exposed.
- **Distribution.** Releases now ship Linux desktop bundles — **AppImage**,
  `.deb`, and `.rpm`, built by Tauri — and `install.sh` installs the AppImage to
  `~/.local/bin`. The static musl CLI binary and the crates.io publish are gone:
  the GUI links WebKitGTK/GTK, so it can't be a static binary or build on
  crates.io. Building from source needs Node.js + npm and the WebKitGTK/GTK dev
  libraries.

### Added

- **Reworked graphical interface** — a tabbed, tray-resident Tauri app:
    - **Console** — containment state, primary actions (start/stop, open portal,
      open Edge), the signed-in **identity**, and **live health checks**.
    - **Shell** — a real interactive terminal inside the container.
    - **Backup** — back up / restore enrollment.
    - **Logs** — follow and search the app log.
    - **Destroy** — shown only when there's something to remove.
- **System tray** — a status-tinted icon (grey = stopped, teal = running, amber
  = display attached), single-click quick panel, double-click for the full
  window, and a menu with Open portal / Open Edge / a dynamic Start–Stop item.
- **Single instance** — the supervisor holds a process-lifetime lock (one
  container, ever), and the GUI focuses an existing window instead of opening a
  duplicate.
- **Hardened headless profile** — a private IPC namespace and cgroup memory/task
  limits on the delegated scope.
- **Preflight checks** — clear, actionable errors when unprivileged user
  namespaces are disabled, no `/etc/subuid` range exists, or cgroup v2 is
  missing.
- **`just smoke`** — boots a real container in both profiles and `setns`-execs
  into each; the gate for runtime/namespace changes.

### Fixed

- Entering a display-mode container no longer fails with `EPERM`: it is joined to
  a separate IPC namespace only when it actually has one. This fixes the session
  setup (keyring unlock + compliance agent) that had silently broken and left the
  device reporting non-compliant.

### Removed

- The old design's host privileges — the passwordless `sudoers` rule and the
  setuid `nsenter` helper — are gone with the move to rootless.

## [0.1.0] - 2026-06-19

### Added

- Run Microsoft Intune in a `systemd-nspawn` container, driven by a single Rust
  CLI. Works on any Wayland compositor (niri, Hyprland, Sway, GNOME, KDE) and X11.
- Commands: `init`, `enroll`, `daemon`, `edge`, `stop`, `status`, `doctor`,
  `backup` / `restore` / `backup-inspect`, plus hidden `shell`, `destroy`,
  `native-host`, and `sso-test`.
- Multiple install paths: a static musl binary attached to each GitHub Release
  (`curl | sh` via `install.sh`), `cargo install intune-container`, or from
  source with `just install`.
- **Headless by default** — the container has no access to your screen; the real
  display and GPU are forwarded only for the interactive `enroll` and `edge`
  flows. Background SSO runs against a private in-container `Xvfb`.
- **Seamless host browser SSO** (`daemon`) via a built-in native-messaging host
  for the [`linux-entra-sso`](https://github.com/siemens/linux-entra-sso)
  extension (Firefox, Thunderbird, Chrome, Chromium, Brave) — no Python, no proxy.
- **Enrollment persistence**: device registration and tokens are stored on the
  host and bind-mounted in, so they survive container rebuilds; `backup`/`restore`
  bundle them.
- Robustness/safety: a cross-process lifecycle lock, a hardened passwordless
  nsenter helper (resolves the container leader PID itself), `stop` that errors
  rather than risk operating on a mounted rootfs, `destroy` that removes all
  privileged host artifacts, and `host_user`/`machine_name` validation.
- GitHub Actions: reusable `CI` (fmt/clippy/test/build) and a `Release` pipeline
  that, on a semver tag, runs CI then publishes the container image to the GitHub
  Container Registry (ghcr.io) and the crate to crates.io.
- MkDocs documentation site (quickstart, capabilities, architecture, roadmap),
  published to GitHub Pages on push to `master`.
