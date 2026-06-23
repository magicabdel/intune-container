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

Build and install the single binary from source:

```sh
just install   # builds + installs `intune-container` (GUI + CLI in one binary)
```

Run it with **no subcommand** to open the graphical interface (the default) and
click **Enroll this device** — enroll, Edge, browser SSO, health checks and
backups are all one click away, and closing the window keeps it in your tray:

```sh
intune-container          # opens the GUI
```

The same binary is the command-line tool when given a subcommand:

```sh
intune-container enroll   # set up + enroll your device (opens the portal)
intune-container daemon    # (optional) seamless Teams/M365 SSO in your browser
```

Daily CLI use: `edge` · `status` · `doctor` · `stop`. Full walkthrough in the
**[Quickstart](docs/quickstart.md)**.

The default container image is publicly hosted and ready to go (it already
includes everything for headless SSO) — there's nothing to build.

> **Requirements:** a **systemd** host with `systemd-nspawn` + `machinectl`,
> plus `just`, `cargo`, `nsenter` (util-linux), and `docker` (or `podman`).
> **Building from source also needs Node.js + npm** — the interface is a
> TypeScript / React / Emotion app (in `frontend/`) that Tauri bundles into the
> binary at compile time. At runtime the GUI needs WebKitGTK 4.1 and, for the
> system tray, `libayatana-appindicator` (`libappindicator-gtk3` on some
> distros); graphical `sudo` prompts use `zenity`, `kdialog`, or an
> `ssh-askpass`. Without the appindicator library the GUI still runs as a plain
> window (no tray). Non-systemd distros (Alpine, Void, Gentoo/OpenRC, …) aren't
> supported.

## Architecture at a glance

A single crate produces a single `intune-container` binary that is **both** the
graphical interface (default) and the command-line tool:

| Part | Role |
|------|------|
| `src/lib.rs` (library) | All logic — container lifecycle, enroll, SSO, backups, health checks — exposed as Rust functions in `ops`. |
| `src/main.rs` (binary) | clap dispatch: no subcommand → GUI; any subcommand → CLI. |
| `src/gui.rs` | The Tauri shell: window, tray, and typed commands that call `ops`. |
| `frontend/` | The interface itself — TypeScript + React + Emotion (Vite), bundled into the binary. |

Both the GUI and CLI call the same `ops` functions **in-process** — neither
shells out. Privileged work goes through one `sudo` helper that prompts on the
tty for the CLI and through a graphical askpass for the GUI.

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
- [x] **No-restart display attach** — `enroll`/`edge` bind the host display into
  the already-running container via `machinectl bind` and detach again when the
  app closes, so a background `daemon` SSO session is never interrupted.
- [x] **Graphical interface (default)** — a Tauri desktop app that lives in the
  system tray: a window with live status and one-click actions for every
  operation (enroll, Edge, browser SSO, health checks, backup/restore, destroy).
  Closing the window hides it to the tray; left-click the tray to reopen.
  Privileged steps prompt for `sudo` through a graphical askpass.
- [x] **One binary, two faces** — a single `intune-container` executable is both
  the GUI (run with no subcommand) and the CLI (run with a subcommand). All
  logic lives in a shared library that both call **directly, in-process**;
  nothing shells out.

### 🔲 Planned

- [ ] **Private network namespace** — NAT egress with LAN/localhost blocked
  (closes the main isolation gap; see the [Roadmap](docs/roadmap.md)).
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

## Disclaimer

This is a personal, educational tool for running Microsoft Intune in an isolated
container — for example, to keep corporate device management off your personal
Linux machine. It is **not** intended to bypass, defeat, or misrepresent your
organization's device-management or compliance controls, and it does not modify
or weaken Intune or Entra ID themselves.

You are responsible for using it in line with your employer's acceptable-use and
security policies and your Microsoft licensing terms. If you're unsure whether
this is permitted in your environment, check with your IT/security team first.
Provided as-is, with no warranty.

## License

Source is [MIT](LICENSE). It automates **Microsoft proprietary** software
(`intune-portal`, `microsoft-edge`, the identity broker) and integrates with
`linux-entra-sso` (MPL-2.0); those have their own terms and require a valid
Intune / Microsoft 365 subscription. See [`SECURITY.md`](SECURITY.md) for the
trust and isolation model.
