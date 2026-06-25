# Security model

This document states, precisely and honestly, what isolation `intune-container`
provides — and what it does not. The runtime is rootless (no host root, no
`sudo`): the container is built from an unprivileged user namespace, and the
container's `root` (uid 0) maps to your own host user. Files created inside stay
owned by you.

Read this before relying on the container as a security boundary.

## What we are protecting, and from whom

**Adversary (the threat we defend against):** a *compromised or backdoored
identity broker*, or a *hostile payload arriving through the browser SSO bridge*.
That is, a process that ends up running inside the container and turns hostile.

**Protected assets:** host root, your host user session, your `$HOME`, the host
display (keystrokes/screen), and
 host-local sockets and services.

**The claim (daemon mode only):** a hostile in-container process cannot reach
host root, read your home, observe your display, reach host-local sockets, or
exhaust your session's memory/process budget.

## Two profiles

The boundary depends on which profile the container booted under. The profile is
chosen automatically from whether the host display is attached.

| Profile | When | Boundary |
|---|---|---|
| **hardened** | daemon / headless (`with_display == false`) | real boundary against the adversary above |
| **compat** | interactive GUI (display attached) | **not** a boundary — see below |

The `compat` profile forwards the host Wayland/X11 socket into the container so
the Intune portal and SSO popup render on your desktop. X11 in particular has no
intra-display isolation: any client can keylog and screenshot the whole session.
While the display is attached, the container is a convenience sandbox, not a
security boundary. Detaching the display (returning to headless) restarts under
the hardened profile.

## Out of scope

- **Kernel exploits.** The hardened profile shrinks the kernel attack surface
  (seccomp, dropped capabilities, `no_new_privs`, no nested user namespaces) but
  cannot eliminate it: unprivileged user namespaces are themselves part of that
  surface. An attacker with a kernel local-privilege-escalation exploit is *not*
  contained by a rootless runtime. Containing that adversary requires a virtual
  machine (Firecracker / Cloud Hypervisor) or a userspace kernel (gVisor), which
  is a different architecture.
- **Supply chain.** A genuine boundary contains a compromised broker; it does
  not vouch for the broker image's integrity. Trust the image you run.

## Acknowledged trust boundary: the SSO bridge

`intune-container` exists to feed the host browser extension tokens via a
native-messaging bridge to the in-container identity broker. That channel is an
*authorized hole* in the boundary and cannot be removed — it is the reason the
container exists. The bridge runs inside the container (joining the broker's own
session bus via `setns`), forwards only the specific broker operations the
extension needs, and exposes no host bus. It is part of the attack surface and is
reviewed as such.

## Hardening status

The hardened daemon profile is being built in phases. Implemented so far:

- **Resource limits** — the container's delegated cgroup scope caps process count
  (and, in the hardened profile, memory), so a fork bomb or runaway broker cannot
  exhaust the host user session.
- **Private IPC namespace** (hardened only) — the container does not share the
  host's System V / POSIX shared memory or `/dev/shm`. (The compat profile keeps
  the host IPC namespace, which XWayland's MIT-SHM requires.)

Planned: seccomp allow-list, capability bounding-set drop, `no_new_privs`,
curated `/dev` and read-only `/sys`, private network namespace with userspace
egress, and an AppArmor/SELinux profile. Until those land, the hardened profile is
stronger than compat but does not yet meet the full claim above.

## Reporting a vulnerability

Please report security issues **privately** rather than opening a public issue.
Use GitHub's "Report a vulnerability" (Security → Advisories) on this repository,
or email the maintainer listed in `Cargo.toml`. We aim to acknowledge reports
within a few days. Include the affected version, your distro/compositor, and
steps to reproduce.

This project is pre-1.0; only the latest release receives security fixes.
