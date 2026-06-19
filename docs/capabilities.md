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

Device registration and tokens live on the **host** (bind-mounted into the
container), so they survive `init --force` / image rebuilds:

- `/var/lib/intune-container/device-broker` and `/var/lib/intune-container/intune`
- `~/Intune/.local/share/keyrings`, `~/Intune/.config/microsoft-identity-broker`,
  `~/Intune/.config/intune`

`backup`/`restore` bundle exactly these paths.

## Safety & robustness

- A cross-process **lifecycle lock** serializes boot/stop so concurrent commands
  (including the browser-spawned native host) can't race.
- `stop` escalates to `terminate` and errors rather than letting a later
  `rm -rf` operate on a still-mounted rootfs.
- `destroy` removes the rootfs, config, machine registration, the passwordless
  sudoers rule + nsenter helper, and the browser manifests — leaving nothing
  privileged behind.
- `host_user` / `machine_name` are validated before being interpolated into
  shell scripts or the sudoers rule.

## Configuration

Config lives at `~/.local/share/intune-container/config.toml`:

```toml
machine_name = "intune"
rootfs_path = "/var/lib/machines/intune"
host_uid = 1000
host_user = "youruser"
```

The container image used by `init`/`enroll` defaults to `DEFAULT_IMAGE` in
`src/init.rs` — a publicly hosted image that already includes `Xvfb` for headless
SSO, so there's nothing to build to get started. Override per-run with `--image`,
or build/customize your own (see
[Build your own image](quickstart.md#build-your-own-image-optional)).

## Debugging

```sh
intune-container -v enroll     # verbose: show display detection + readiness
intune-container status        # container + display + SSO state
intune-container doctor        # health checks across the stack
intune-container sso-test      # query the broker directly (SSO debugging)
intune-container shell         # open a shell inside the container
```
