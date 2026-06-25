# Quickstart

## Install

Pick one:

=== "Prebuilt binary"

    Static Linux x86_64 binary from the latest GitHub Release, into `~/.local/bin`:

    ```sh
    curl -fsSL https://raw.githubusercontent.com/magicabdel/intune-container/master/install.sh | sh
    # or: wget -qO- https://raw.githubusercontent.com/magicabdel/intune-container/master/install.sh | sh
    ```

    Override the location with `DEST=/usr/local/bin`, or pin a version with
    `VERSION=v0.2.0`.

=== "cargo"

    ```sh
    cargo install intune-container
    ```

=== "From source"

    ```sh
    just install      # builds and installs to ~/.local/bin (no sudo)
    ```

Make sure `~/.local/bin` is on your `PATH`. The same binary is **both** the
graphical interface and the command-line tool: run it with **no subcommand** for
the GUI, or with a subcommand for the CLI. The default container image is
publicly hosted and already includes everything needed (including `Xvfb` for
headless SSO), so there's nothing else to build — and no Docker/Podman is
required, the image is pulled with a built-in OCI client.

---

## Using the interface

Open the app (the default — no subcommand):

```sh
intune-container
```

1. **Enroll** — click **Enroll this device** on the Console. It provisions the
   container (first run only), opens the Intune Portal; sign in and enroll, then
   close the window.
2. **Everyday actions** live on the Console: **Start/Stop** the container
   (starting also makes browser SSO ready), **Open portal**, and **Open Edge**.
   The signed-in identity and **live health checks** are right there too.
3. **Other tabs** — **Shell** (a terminal inside the container), **Backup**
   (back up / restore enrollment), **Logs**, and **Destroy**.
4. **Tray** — single-click for a compact quick panel, double-click for the full
   window, right-click for a menu. The icon is tinted to the container's state
   (grey = stopped, teal = running, amber = display attached). Closing the
   window keeps the app in the tray; only **Quit** exits.

!!! tip
    The portal window can take up to ~30s to appear the first time while the
    container finishes booting and its services come up.

---

## Using the command line

The same binary is the CLI when given a subcommand.

### Enroll your device

```sh
intune-container enroll
```

Provisions the container (first run only), boots it with your display forwarded,
and opens the Intune Portal. Sign in, enroll, then **close the window** —
`enroll` waits for that and reports success.

### Seamless browser SSO (optional)

```sh
intune-container start
```

Starts the container **headless** and installs a native-messaging host for the
[`linux-entra-sso`](https://github.com/siemens/linux-entra-sso) extension. Then
install the extension:

- **Firefox / Thunderbird** — the signed `.xpi` from the
  [releases page](https://github.com/siemens/linux-entra-sso/releases/).
- **Chrome / Chromium / Brave** — the Chrome Web Store (search `linux-entra-sso`).

Open `teams.microsoft.com` and it signs in automatically using the container's
enrollment.

### Open Microsoft Edge

```sh
intune-container edge      # add -v to run in the foreground with logs
```

Launches Edge inside the container with your display forwarded — handy for sites
that require the managed/compliant device.

### Daily use

```sh
intune-container status    # container + display + SSO state
intune-container doctor    # health checks across the stack
intune-container stop      # shut the container down
```

### Back up / restore enrollment

```sh
intune-container backup            # archive device registration + tokens
intune-container backup-inspect    # list archive contents
intune-container restore           # restore after a rebuild (container stopped)
```

### Uninstall

```sh
intune-container destroy --purge   # remove container + all data + host integration
just uninstall                     # remove the binary
```

---

## Build your own image (optional)

The default image is publicly hosted and already includes `Xvfb`, so you only
need this if you want to customize the image or build it yourself. The committed
`Dockerfile` derives from the base image and adds `xvfb`:

```sh
just build-image   # -> localhost/intune-container:local (base + xvfb)
intune-container init --force --image localhost/intune-container:local
```

To make a custom image the default, push it to a registry and set `DEFAULT_IMAGE`
in `src/backend.rs` (or pass `--image` per run).
