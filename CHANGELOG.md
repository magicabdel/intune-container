# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [0.1.0] - 2026-06-19

### Added

- Run Microsoft Intune in a `systemd-nspawn` container, driven by a single Rust
  CLI. Works on any Wayland compositor (niri, Hyprland, Sway, GNOME, KDE) and X11.
- Commands: `init`, `enroll`, `daemon`, `edge`, `stop`, `status`, `doctor`,
  `backup` / `restore` / `backup-inspect`, plus hidden `shell`, `destroy`,
  `native-host`, and `sso-test`.
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
