#!/usr/bin/env bash
# Deterministic dependency setup for a new machine.
#
# Installs everything Boitata needs to build and run:
#   - the pinned Rust toolchain (from rust-toolchain.toml)
#   - ripgrep at a pinned version (backs the `search` tool)
#   - verifies git is present (backs the git_* tools)
#
# Crate versions for the Boitata build are pinned by the committed Cargo.lock.
# (ripgrep's own transitive deps are pinned by its published Cargo.lock, used
# via --locked below.)
# Safe to re-run — every step is idempotent.
set -euo pipefail

# Pinned version of ripgrep to install if it's missing or mismatched.
RIPGREP_VERSION="14.1.1"

# Run from the repo root so rustup finds rust-toolchain.toml.
cd "$(dirname "$0")/.."

echo "==> Ensuring rustup and the pinned Rust toolchain"
if ! command -v rustup >/dev/null 2>&1; then
    echo "Installing rustup..."
    # NOTE: this is the official rustup install method (curl | sh). It trusts the
    # transport and origin; `--proto '=https' --tlsv1.2` guard the network path
    # but not a compromised origin. If that's a concern, install rustup from your
    # OS package manager first and re-run this script.
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    # shellcheck disable=SC1091
    source "${CARGO_HOME:-$HOME/.cargo}/env"
fi
# `rustup show` materializes the channel + components pinned in
# rust-toolchain.toml, installing them if missing.
rustup show >/dev/null
# Ensure cargo is on PATH even when rustup was pre-installed but its bin dir
# isn't exported in this shell (otherwise the cargo install below would fail).
if ! command -v cargo >/dev/null 2>&1; then
    # shellcheck disable=SC1091
    source "${CARGO_HOME:-$HOME/.cargo}/env" 2>/dev/null || true
fi

echo "==> Ensuring ripgrep ${RIPGREP_VERSION} (for the search tool)"
# Compare just the version token (e.g. "14.1.1") for a robust, distribution-
# independent check, so we don't needlessly rebuild on a matching install.
if command -v rg >/dev/null 2>&1 && [ "$(rg --version | head -1 | awk '{print $2}')" = "${RIPGREP_VERSION}" ]; then
    echo "ripgrep ${RIPGREP_VERSION} already installed"
else
    # Build from source with the toolchain we just pinned — deterministic and
    # cross-platform (no per-OS package name or checksum juggling). Note: this
    # installs into the cargo bin dir and takes precedence over any ripgrep you
    # installed via a system package manager.
    if command -v rg >/dev/null 2>&1; then
        echo "note: replacing existing $(rg --version | head -1) with the pinned build"
    fi
    cargo install ripgrep --version "${RIPGREP_VERSION}" --locked --force
fi

echo "==> Checking git (for the git tools)"
if ! command -v git >/dev/null 2>&1; then
    echo "ERROR: git is required but was not found on PATH." >&2
    exit 1
fi

echo "==> Done. Build with: cargo build --release"
