# Capabilities

## Command surface

| Command | What it does |
|---------|--------------|
| `init` | Provision the container rootfs from the OCI image. |
| `enroll` | One-shot first-time setup: init (if needed) → boot with display → open the Intune Portal and wait until you close it. |
| `daemon` | Enable seamless browser SSO: run the container headless and install the native-messaging host for the `linux-entra-sso` extension. |
| `edge` | Launch Microsoft Edge inside the container (forwards your display). `-v` runs it in the foreground with logs. |
| `stop` | Power the container off. |
| `status` | Show container state, display mode, and SSO state. |
| `doctor` | Health checks across the whole stack (config, rootfs, broker, DNS, keyring, bus). |
| `backup` / `restore` | Preserve enrollment (device registration + tokens) across rebuilds. |
| `backup-inspect` | List the contents of a backup archive. |

Advanced/hidden (not in `--help`): `shell`, `destroy [--purge]`, `native-host`
(used by the browser), `sso-test` (SSO debugging).

## Display model

The container is **headless by default** — no access to your screen or GPU. The
real display (X11/Wayland + GPU) is forwarded **only** while you run the two
interactive GUI flows:

- `enroll` — needs the Intune Portal sign-in window.
- `edge` — the GUI browser.

Everything else (the `daemon` SSO path, background token serving) runs headless,
where the broker renders to a private in-container `Xvfb`. Switching modes
reboots the same container (see [Architecture](architecture.md)).

## Headless background SSO

`daemon` exposes the container's session bus to the host and installs a
native-messaging host (this same binary). The browser extension talks to it; it
bridges to the container's Microsoft identity broker over D-Bus and returns the
PRT SSO cookie that authenticates `login.microsoftonline.com` flows. No Python,
no proxy daemon, no host session-bus setup.

## Enrollment persistence

Device registration, tokens, and broker/agent config live in a persistent store
**outside the rootfs**, bound into the container at boot, so they survive
`init --force` / image rebuilds:

- `~/.local/share/intune-container/persist/state/{device-broker,intune}`
- `~/.local/share/intune-container/persist/home/{keyrings,config-broker,config-intune}`

`backup`/`restore` bundle exactly these paths into a portable archive
(`device-state/…`, `home/…`). `destroy --purge` removes the store.

## Safety & robustness

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
owned by you. Browser SSO (`daemon`) runs the native-messaging bridge inside the
container over its own session bus, so no privileged bus exposure is needed.

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
