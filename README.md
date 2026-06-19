<!--
HERO IMAGE
The banner lives at docs/assets/hero.jpg.
-->
<p align="center">
  <img src="docs/assets/hero.jpg" alt="intune-container" width="640">
</p>

<h1 align="center">intune-container</h1>

<p align="center">
  <em>Microsoft Intune in an isolated systemd-nspawn container —
  headless by default, with seamless Entra ID SSO in your host browser.</em>
</p>

<p align="center">
  📖 <a href="https://magicabdel.github.io/intune-container/"><b>Documentation</b></a>
  · <a href="https://magicabdel.github.io/intune-container/quickstart/">Quickstart</a>
  · <a href="https://magicabdel.github.io/intune-container/architecture/">Architecture</a>
  · <a href="https://magicabdel.github.io/intune-container/roadmap/">Roadmap</a>
</p>

---

Intune's Linux agent and the Microsoft identity broker are desktop apps that
make broad changes to a host. **intune-container** runs them in a dedicated
`systemd-nspawn` container so your host stays clean — and a tiny native-messaging
bridge lets your everyday browser use the container's enrollment to sign in to
Teams, Outlook, and other M365 apps. Works on any Wayland compositor (niri,
Hyprland, Sway, GNOME, KDE) and X11.

## Quick start

```sh
# Prebuilt binary (Linux x86_64) — installs to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/magicabdel/intune-container/master/install.sh | sh

# ...or from crates.io
cargo install intune-container

# ...or from source
just install
```

Then:

```sh
intune-container enroll # set up + enroll your device (opens the portal)
intune-container daemon # (optional) seamless Teams/M365 SSO in your browser
```

Daily use: `edge` · `status` · `doctor` · `stop`. Full walkthrough in the
**[Quickstart](docs/quickstart.md)**.

The default container image is publicly hosted and ready to go (it already
includes everything for headless SSO) — there's nothing to build.

> **Requirements:** a **systemd** host with `systemd-nspawn` + `machinectl`,
> plus `just`, `cargo`, `nsenter` (util-linux), and `docker` (or `podman`).
> Non-systemd distros (Alpine, Void, Gentoo/OpenRC, …) aren't supported.

## Features

### ✅ Available now

- [x] **Headless by default** — no window into your screen; the real display +
  GPU are forwarded only for the interactive `enroll` and `edge` flows.
- [x] **Seamless host-browser SSO** — Teams/Outlook/M365 sign in automatically
  via the container's enrollment (no Python, no proxy daemon).
- [x] **Compositor-agnostic** — auto-detects Wayland, abstract X11, and
  Xauthority; no hardcoded socket names.
- [x] **One-command enroll** — provision, boot, and open the portal in one step.
- [x] **Enrollment backup/restore** — survive container rebuilds without
  re-enrolling.
- [x] **Microsoft Edge** in the container, with audio/GPU passthrough during GUI
  sessions.
- [x] **`doctor`** health checks across the whole stack.
- [x] **Pure Rust** host binary — no GTK dependency on the host.

### 🔲 Planned

- [ ] **Private network namespace** — NAT egress with LAN/localhost blocked
  (closes the main isolation gap; see the [Roadmap](docs/roadmap.md)).
- [ ] **No-restart display attach** via `machinectl bind`, so launching
  `enroll`/`edge` no longer interrupts a running `daemon` SSO session.
- [ ] **Preflight checks** — verify the host toolchain up front with one clear,
  actionable error.

## Compositor support

| Compositor | Status | Notes |
|------------|:------:|-------|
| Niri | ✅ | Abstract X11 sockets auto-detected |
| Hyprland | ✅ | Standard XWayland |
| Sway | ✅ | Standard XWayland |
| GNOME | ✅ | Mutter Xauthority auto-detected |
| KDE | ✅ | Standard Xauthority |

## Credits & inspiration

This project stands on the shoulders of two excellent projects:

- **[frostyard/intuneme](https://github.com/frostyard/intuneme)** — the original
  `systemd-nspawn`-based Intune manager that inspired this container approach
  (and the base OCI image).
- **[siemens/linux-entra-sso](https://github.com/siemens/linux-entra-sso)** — the
  browser extension and native-messaging protocol that make host SSO work;
  this CLI ships a compatible native-messaging host. Install the extension from
  its [releases](https://github.com/siemens/linux-entra-sso/releases/).

## License

Source is [MIT](LICENSE). It automates **Microsoft proprietary** software
(`intune-portal`, `microsoft-edge`, the identity broker) and integrates with
`linux-entra-sso` (MPL-2.0); those have their own terms and require a valid
Intune / Microsoft 365 subscription. See [`SECURITY.md`](SECURITY.md) for the
trust and isolation model.
