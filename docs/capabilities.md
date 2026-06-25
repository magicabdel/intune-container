# Capabilities

## Command surface

| Command | What it does |
|---------|--------------|
| `init` | Provision the container rootfs from the OCI image. |
| `enroll` | One-shot first-time setup: init (if needed) → boot with display → open the Intune Portal and wait until you close it. |
| `start` | Start the container headless and make browser SSO ready: install the native-messaging host for the `linux-entra-sso` extension. |
| `edge` | Launch Microsoft Edge inside the container (forwards your display). `-v` runs it in the foreground with logs. |
| `stop` | Power the container off. |
| `status` | Show container state, display mode, and SSO state. |
| `doctor` | Health checks across the whole stack (registration, container, network, identity broker, keyring, compliance agent). |
| `backup` / `restore` | Preserve enrollment (device registration + tokens) across rebuilds. |
| `backup-inspect` | List the contents of a backup archive. |

Advanced/hidden (not in `--help`): `shell`, `destroy [--purge]`, `native-host`
(used by the browser), `sso-test` (SSO debugging).

## Graphical interface

Running `intune-container` with no subcommand opens the tray-resident desktop
app (the default). It's organized as tabs:

- **Console** — the containment state, the primary actions (start/stop the
  container, open the portal, open Edge, enable browser SSO), the signed-in
  identity, and **live health checks** (the same data as `doctor`/`sso-test`).
- **Shell** — a real interactive terminal *inside* the container.
- **Backup** — back up and restore enrollment state.
- **Logs** — follow and search the app log.
- **Destroy** — shown only when there's something to remove.

From the system tray: **single-click** opens a compact quick panel,
**double-click** opens the full window, and **right-click** gives a menu (open
portal/Edge, start/stop, quit). The tray icon is tinted to the container's
state — grey (stopped), teal (running headless), amber (display attached).
Closing the window keeps the app in the tray; only **Quit** exits.

## Display model

The container is **headless by default** — no access to your screen or GPU. The
real display (X11/Wayland + GPU) is forwarded **only** while you run the two
interactive GUI flows:

- `enroll` — needs the Intune Portal sign-in window.
- `edge` — the GUI browser.

Everything else (the background SSO path, token serving) runs headless, where
the broker renders to a private in-container `Xvfb`. Switching modes restarts
the same container (see [Architecture](architecture.md)).

## Headless background SSO

`start` installs a native-messaging host (this same binary) for the browser
extension and keeps the container running headless. When the extension calls in,
the host **enters the container** (`setns`) and talks to the Microsoft identity
broker over the container's **own** session bus, returning the PRT SSO cookie
that authenticates `login.microsoftonline.com` flows. No Python, no proxy, and
the host's session bus is never exposed.

## Enrollment persistence

Device registration, tokens, and broker/agent config live in a persistent store
**outside the rootfs**, bound into the container at boot, so they survive
`init --force` / image rebuilds:

- `~/.local/share/intune-container/persist/state/{device-broker,intune}`
- `~/.local/share/intune-container/persist/home/{keyrings,config-broker,config-intune}`

`backup`/`restore` bundle exactly these paths into a portable archive
(`device-state/…`, `home/…`). `destroy --purge` removes the store.

## Safety & robustness

- **One container, always.** The supervisor holds a process-lifetime singleton
  lock, so however many times you launch the app there is at most one running
  container; the GUI focuses the existing window instead of starting a duplicate.
- A cross-process **lifecycle lock** serializes boot/stop so concurrent commands
  (including the browser-spawned native host) can't race.
- The container runs **rootless** in an unprivileged user namespace — no host
  root, no `sudo`, `systemd-nspawn`, `machinectl`, or `nsenter` helper. `destroy`
  leaves nothing privileged behind because nothing privileged was installed.
- `host_user` is validated before being interpolated into any generated script.

## Configuration

Config lives at `~/.local/share/intune-container/config.toml`:

```toml
machine_name = "intune"
rootfs_path = "/home/youruser/.local/share/intune-container/rootfs"
host_uid = 1000
host_user = "youruser"
```

The container image used by `init`/`enroll` defaults to `DEFAULT_IMAGE` in
`src/backend.rs` — a publicly hosted image that already includes `Xvfb` for
headless SSO, so there's nothing to build to get started. Override per-run with
`--image`, or build/customize your own (see
[Build your own image](quickstart.md#build-your-own-image-optional)).

## Runtime

The container runs as a pure-Rust, rootless runtime: the rootfs's `systemd` boots
inside an unprivileged user namespace (via a detached supervisor process), and
other commands enter it with `setns`. The image is pulled with a built-in OCI
client into `~/.local/share/intune-container/rootfs` — no docker/podman needed.

In-container apps run as the container's **root**, which the user-namespace
id-map points at your unprivileged host user, so host-owned resources (the
Wayland socket, the persistence store) are accessible and anything created stays
owned by you. Browser SSO (via `start`) runs the native-messaging bridge inside
the container over its own session bus, so no privileged bus exposure is needed.

Hosts that can't run rootless (user namespaces disabled, no `/etc/subuid` range,
or no cgroup v2) get a clear preflight error from `enroll`/`status`.

## Debugging

```sh
intune-container -v enroll     # verbose: show display detection + readiness
intune-container status        # container + display + SSO state
intune-container doctor        # health checks across the stack
intune-container sso-test      # query the broker directly (SSO debugging)
intune-container shell         # open a shell inside the container
```
