# Quickstart

## 1. Install

```sh
just install      # builds and installs to ~/.local/bin (no sudo)
```

Make sure `~/.local/bin` is on your `PATH`. The default container image is
publicly hosted and already includes everything needed (including `Xvfb` for
headless SSO), so there's nothing else to build — see
[Build your own image](#build-your-own-image-optional) if you want to customize it.

## 2. Enroll your device

```sh
intune-container enroll
```

This provisions the container (first run only), boots it with your display
forwarded, and opens the Intune Portal. Sign in, enroll, then **close the
window** — `enroll` waits for that and then reports success.

!!! tip
    The portal window can take up to ~30s to appear the first time while the
    container finishes booting and its services come up.

## 3. (Optional) Seamless browser SSO

```sh
intune-container daemon
```

This runs the container **headless** and installs a native-messaging host for
the [`linux-entra-sso`](https://github.com/siemens/linux-entra-sso) extension.
Then install the extension:

- **Firefox / Thunderbird** — the signed `.xpi` from the
  [releases page](https://github.com/siemens/linux-entra-sso/releases/).
- **Chrome / Chromium / Brave** — the Chrome Web Store (search `linux-entra-sso`).

Open `teams.microsoft.com` and it signs in automatically using the container's
enrollment.

## 4. Open Microsoft Edge

```sh
intune-container edge
```

Launches Microsoft Edge inside the container with your display forwarded — handy
for sites that require the managed/compliant device. Add `-v` to run it in the
foreground with logs.

## Daily use

```sh
intune-container edge      # open Microsoft Edge (forwards your display)
intune-container status    # container + display + SSO state
intune-container doctor    # health checks across the stack
intune-container stop      # shut the container down
```

## Backup / restore enrollment

```sh
intune-container backup            # archive device registration + tokens
intune-container backup-inspect    # list archive contents
intune-container restore           # restore after a rebuild (container stopped)
```

## Uninstall

```sh
intune-container destroy --purge   # remove container + all data + host integration
just uninstall                     # remove the binary
```

## Build your own image (optional)

The default image is publicly hosted and already includes `Xvfb`, so you only
need this if you want to customize the image or build it yourself. The committed
`Dockerfile` derives from the base image and adds `xvfb`:

```sh
just build-image   # -> localhost/intune-container:local (base + xvfb)
intune-container init --force --image localhost/intune-container:local
```

To make a custom image the default, push it to a registry and set `DEFAULT_IMAGE`
in `src/init.rs` (or pass `--image` per run).
