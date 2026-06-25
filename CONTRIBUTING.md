# Contributing

Thanks for your interest in improving `intune-container`!

## Development setup

You'll need a Rust toolchain and [`just`](https://github.com/casey/just). Building
the GUI (`just build`/`just install`) also needs Node.js + npm and the
WebKitGTK/GTK development libraries (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`,
`libayatana-appindicator3-dev`, `librsvg2-dev`, `libsoup-3.0-dev` on
Debian/Ubuntu).

```sh
just build      # cargo build --release
just test       # cargo test
just lint       # cargo clippy -- -W clippy::all
just fmt        # cargo fmt
just install    # build + install to ~/.local/bin
```

## Bar for changes

Before opening a PR, please make sure:

- `cargo fmt` has been run (no diff).
- `cargo clippy --all-targets -- -W clippy::all` is **warning-free**.
- `cargo test` passes.
- `cargo build --release` succeeds.

New behavior should come with a unit test where practical. The shell scripts we
generate (session setup, virtual-display) are built by pure functions
specifically so they can be asserted on in tests — prefer adding to those rather
than hand-rolling untested string building.

Changes to the **rootless runtime** (`src/runtime.rs` — namespaces, `setns`,
mounts, the cgroup scope) can't be exercised by unit tests, which never boot a
container. Run `just smoke` (boots a real container in both profiles and
`setns`-execs into each) before merging anything that touches it.

## Guidelines

- Keep changes minimal and focused; match the surrounding style.
- Anything that touches the **rootless runtime** (user namespaces, `setns`, id
  mapping, the cgroup scope), the network/display isolation model, or token
  handling is security-sensitive — call it out explicitly in the PR description.
- Don't add packages to the base image; the derived image (`Dockerfile`) is the
  place for extra packages like `xvfb`.
- Prefer existing dependencies; justify new ones.
- Prefer existing dependencies; justify new ones.

## Reporting security issues

Please report vulnerabilities **privately** — use GitHub's "Report a
vulnerability" (Security → Advisories) rather than opening a public issue.
