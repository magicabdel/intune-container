# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

### Changed

- Renamed the `daemon` command to **`start`**: starting the container now also
  makes browser SSO ready (installs the native-messaging host), so "start" means
  the same thing in the CLI, the window, and the tray.

- **Rootless runtime.** The container now boots inside an unprivileged user
  namespace via a detached supervisor (`fork` + `unshare`, multi-id mapping with
  `newuidmap`/`newgidmap`, a delegated cgroup scope over the user's systemd
  manager, `pivot_root`). Commands enter it with `setns`. **No host root, no
  `sudo`, `systemd-nspawn`, `machinectl`, `nsenter`, Docker, or Podman** — the
  image is pulled with a built-in OCI client. `destroy` leaves nothing
  privileged behind because nothing privileged is installed.
- The browser SSO native-host bridge runs **inside** the container (via `setns`)
  against the container's own session bus; the host bus is no longer exposed.

### Added

- **Reworked graphical interface.** Tabbed Tauri app — Console (containment
  state, start/stop, portal, Edge, signed-in identity, live health checks),
  in-app Shell (a real terminal in the container), Backup/restore, Logs, and a
  conditional Destroy tab. Status-tinted tray icon (grey/teal/amber), single-
  click quick panel, double-click full window, and a dynamic Start/Stop menu
  item. A single-instance plugin focuses the existing window instead of opening
  a duplicate.
- **Single-container guarantee**: the supervisor holds a process-lifetime
  singleton lock, so any number of launches share one container.
- **Hardening (headless profile)**: a private IPC namespace and cgroup memory/
  task limits on the delegated scope.
- **Preflight checks**: clear, actionable errors when unprivileged user
  namespaces are disabled, no `/etc/subuid` range exists, or cgroup v2 is
  missing.
- **`just smoke`**: boots a real container in both profiles and `setns`-execs
  into each — the gate for runtime/namespace changes.

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
