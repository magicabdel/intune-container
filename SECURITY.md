# Security Policy

## Reporting a vulnerability

Please report security issues **privately** rather than opening a public issue.
Use GitHub's "Report a vulnerability" (Security → Advisories) on this
repository, or email the maintainers listed in `Cargo.toml` / the repository
profile. We aim to acknowledge reports within a few days.

When reporting, include the affected version, your distro/compositor, and steps
to reproduce.

## Trust & threat model

`intune-container` runs Microsoft Intune inside a `systemd-nspawn` container and
brokers Entra ID authentication to your host browser. Understanding what it does
and does **not** isolate is important before deploying it.

### Host privileges it installs

- **Passwordless sudo for one helper.** `init` installs
  `/etc/sudoers.d/intune-container` granting the invoking user `NOPASSWD` on a
  single root-owned helper, `/usr/local/libexec/intune-container/nsenter-exec`.
  The helper resolves the container's leader PID **itself** (from the fixed
  machine name) and `nsenter`s into the container, then drops to the
  unprivileged container user via `su` before running the caller-supplied
  script. The net capability granted is therefore equivalent to *"run commands
  as the container user inside the container"* — comparable to `machinectl
  shell` — not host root. The caller cannot choose an arbitrary PID/namespace.
- **`destroy` removes this rule and helper** so teardown leaves no dangling
  passwordless-sudo grant. `just uninstall` only removes the binary; run
  `intune-container destroy` for full host cleanup.
- Other privileged actions (`systemd-nspawn`, `machinectl poweroff`, extracting
  the rootfs into `/var/lib/machines`) run via `sudo` and will prompt normally.

### What is isolated

- **Display (default headless).** By default the container has **no** access to
  your real display or GPU. The Microsoft identity broker runs against a
  private in-container `Xvfb`. The host screen is only bound in for the
  interactive `enroll` and `edge` flows, for the duration of that session.
- **Filesystem.** The container uses its own rootfs; only `~/Intune`, the
  device-state dirs, and (for SSO) a session-bus socket directory are bind
  mounted.

### What is NOT isolated (important)

- **Network namespace is shared with the host.** The container is **not** run
  with `--private-network`; it uses the host's network stack and a copy of the
  host resolver. Consequences:
  - The container can reach host-local services on `localhost` and **host
    abstract UNIX sockets** — including a Wayland compositor's *abstract* X11
    socket (e.g. niri's XWayland) — regardless of whether any display directory
    is bind-mounted. "Headless" removes the *filesystem* display binding, not
    network reachability of host-local sockets.
  - Treat the container as having the same network reach as your user.
  Private-network isolation (veth/NAT) is planned but not yet implemented.
- **Auth tokens** live in the container's gnome-keyring (unlocked with an empty
  passphrase, mirroring a desktop login keyring) and are reachable by anything
  that can talk to the container's session bus while SSO (`daemon`) is enabled.
- **IPC namespace for display apps.** The interactive GUI flows (`enroll` portal,
  `edge`) are launched in the **host IPC namespace** (the nsenter helper omits
  `-i`) so X shared memory (MIT-SHM) works against the host's XWayland server;
  otherwise the apps crash with an X11 `BadAccess`. While running, those apps can
  therefore see host SysV IPC (shared memory / semaphores) as your user. The
  default headless background path keeps the container's private IPC namespace.

### Browser SSO

`daemon` installs a native-messaging host (this binary) for the
[`linux-entra-sso`](https://github.com/siemens/linux-entra-sso) browser
extension. Install that extension only from its official signed releases. The
native host connects to the container's broker over the exposed session-bus
socket and returns PRT SSO cookies/tokens to the extension for
`login.microsoftonline.com` flows.

## Supported versions

This project is pre-1.0; only the latest release receives security fixes.
