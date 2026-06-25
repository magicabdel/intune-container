<p align="center">
  <img src="assets/hero.jpg" alt="intune-container" width="720">
</p>

# intune-container

Run Microsoft Intune in an isolated, **rootless** Linux container, with seamless
Entra ID single sign-on in your **host** browser — and the container kept
**headless by default** so it has no window into your screen.

Works on any Wayland compositor (niri, Hyprland, Sway, GNOME, KDE) and on X11.

<div class="grid cards" markdown>

- :material-rocket-launch: **[Quickstart](quickstart.md)** — install, enroll, enable SSO.
- :material-feature-search: **[Capabilities](capabilities.md)** — what each command does.
- :material-sitemap: **[Architecture](architecture.md)** — how it fits together (diagrams).
- :material-map: **[Roadmap](roadmap.md)** — network isolation and what's next.

</div>

## Why

Intune's Linux agent and the Microsoft identity broker are desktop apps that
make broad changes to a host. Running them in a dedicated, rootless container
keeps your host clean — no host root and no `sudo` — while a small
native-messaging bridge lets your everyday browser use the container's enrollment
for SSO to Teams, Outlook, and other M365 apps.

!!! warning "Requirements"
    A Linux host with **unprivileged user namespaces enabled**, a `/etc/subuid`
    + `/etc/subgid` range for your user, `newuidmap`/`newgidmap` (the
    `uidmap`/`shadow` package), and **cgroup v2**. No `sudo`, `systemd-nspawn`,
    `machinectl`, `nsenter`, Docker, or Podman. Building from source also needs
    Rust, `just`, and Node.js + npm. See [Architecture](architecture.md) for the
    isolation model.
