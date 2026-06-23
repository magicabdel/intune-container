# intune-container justfile
# Run `just --list` to see all available recipes

# Default recipe: list available recipes
default:
    @just --list

# Build the binary (GUI + CLI) in release mode. Builds the TypeScript/Emotion
# frontend first, since Tauri embeds it into the binary at compile time.
build: frontend
    cargo build --release

# Build the TypeScript + Emotion frontend (requires Node.js + npm)
frontend:
    cd frontend && npm install && npm run build

# Build the derived container image (base + xvfb) locally for testing.
# Prefers `docker`, falls back to `podman`. Push the result to your registry,
# then hardcode that URL as DEFAULT_IMAGE in src/backend.rs.
build-image:
    #!/usr/bin/env sh
    set -e
    engine=$(command -v docker >/dev/null 2>&1 && echo docker || echo podman)
    # On hosts without a systemd user session (e.g. niri), rootless podman can't
    # create build cgroups. chroot isolation avoids cgroups entirely. Real
    # Docker has no --isolation flag, so only add it for podman(-docker).
    iso=""
    if "$engine" --version 2>/dev/null | grep -qi podman; then
        iso="--isolation=chroot"
    fi
    "$engine" build $iso -t localhost/intune-container:local -f Dockerfile .
    echo "✓ Built localhost/intune-container:local (via $engine $iso)"
    echo "  Test:  intune-container init --force --image localhost/intune-container:local"
    echo "  Push:  $engine tag localhost/intune-container:local <registry>/intune-container:latest && $engine push <registry>/intune-container:latest"

# Run tests (library unit tests; no frontend build required)
test:
    cargo test --lib

# Run clippy lints
lint:
    cargo clippy -- -W clippy::all

# Format code
fmt:
    cargo fmt

# Install the binary to ~/.local/bin. It opens the GUI when run with no
# subcommand, and acts as the CLI for any subcommand (enroll, edge, …).
install: build
    install -Dm755 target/release/intune-container ~/.local/bin/intune-container
    @echo "✓ Installed to ~/.local/bin/intune-container"
    @echo "  Open the interface:  intune-container        (no subcommand = GUI)"
    @echo "  Or use the CLI:       intune-container enroll"

# Uninstall the binary (run `intune-container destroy` first for full cleanup)
uninstall:
    rm -f ~/.local/bin/intune-container
    @echo "✓ Removed ~/.local/bin/intune-container"
    @echo "  Note: this removes only the binary. To also remove the container"
    @echo "  and its enrollment store, run 'intune-container destroy --purge'"
    @echo "  BEFORE uninstalling (or destroy from the GUI)."

# Clean build artifacts
clean:
    cargo clean

# Serve the docs locally with live reload (needs: pip install mkdocs-material)
docs:
    mkdocs serve

# Build the docs site into ./site (needs: pip install mkdocs-material)
docs-build:
    mkdocs build --strict

# Regenerate the whole CHANGELOG from Conventional Commits (needs: git-cliff)
changelog:
    @command -v git-cliff >/dev/null 2>&1 || { echo "git-cliff not found. Install with:  cargo install git-cliff   (or: pacman -S git-cliff)"; exit 1; }
    git cliff --output CHANGELOG.md

# Prepend the entry for an upcoming tag, e.g. `just changelog-release v0.2.0`.
# NOTE: not for the first release — v0.1.0's changelog is hand-written.
changelog-release version:
    @command -v git-cliff >/dev/null 2>&1 || { echo "git-cliff not found. Install with:  cargo install git-cliff   (or: pacman -S git-cliff)"; exit 1; }
    git cliff --unreleased --tag {{ version }} --prepend CHANGELOG.md

# Cut a release: stamp the changelog, commit, tag, and push the tag.
# Bump the version in Cargo.toml first, then run e.g. `just release v0.2.0`.
# For the FIRST release (v0.1.0), skip this — see the README/CONTRIBUTING notes.
release version:
    just changelog-release {{ version }}
    git add CHANGELOG.md
    git commit -m "chore(release): {{ version }}"
    git tag -a {{ version }} -m "{{ version }}"
    @echo "Tagged {{ version }}. Push with:  git push && git push origin {{ version }}"
