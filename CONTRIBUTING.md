# Contributing

Thanks for your interest in improving `intune-container`!

## Development setup

You'll need a Rust toolchain and [`just`](https://github.com/casey/just).

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
generate (session setup, nsenter helper, virtual-display) are built by pure
functions specifically so they can be asserted on in tests — prefer adding to
those rather than hand-rolling untested string building.

## Guidelines

- Keep changes minimal and focused; match the surrounding style.
- Anything that touches privilege (the sudoers rule, the nsenter helper),
  the network/display isolation model, or token handling is security-sensitive
  — call it out explicitly in the PR description, and update `SECURITY.md` if
  the trust model changes.
- Don't add packages to the base image; the derived image (`Dockerfile`) is the
  place for extra packages like `xvfb`.
- Prefer existing dependencies; justify new ones.

## Reporting security issues

Please do **not** open public issues for vulnerabilities. See `SECURITY.md`.
