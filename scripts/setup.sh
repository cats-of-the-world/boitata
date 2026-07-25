#!/usr/bin/env bash
# Deterministic dependency setup for a new machine.
#
# Installs everything Boitata needs to build and run:
#   - the pinned Rust toolchain (from rust-toolchain.toml)
#   - ripgrep at a pinned version (backs the `search` tool)
#   - verifies git is present (backs the git_* tools)
#
# Crate versions are pinned separately by the committed Cargo.lock.
# Safe to re-run — every step is idempotent.
set -euo pipefail

# Pinned version of ripgrep to install if it's missing or mismatched.
RIPGREP_VERSION="14.1.1"

# Run from the repo root so rustup finds rust-toolchain.toml.
cd "$(dirname "$0")/.."

echo "==> Ensuring rustup and the pinned Rust toolchain"
if ! command -v rustup >/dev/null 2>&1; then
    echo "Installing rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    # shellcheck disable=SC1091
    source "${CARGO_HOME:-$HOME/.cargo}/env"
fi
# `rustup show` materializes the channel + components pinned in
# rust-toolchain.toml, installing them if missing.
rustup show >/dev/null

echo "==> Ensuring ripgrep ${RIPGREP_VERSION} (for the search tool)"
if command -v rg >/dev/null 2>&1 && rg --version | head -1 | grep -q "ripgrep ${RIPGREP_VERSION}"; then
    echo "ripgrep ${RIPGREP_VERSION} already installed"
else
    # Build from source with the toolchain we just pinned — deterministic and
    # cross-platform (no per-OS package name or checksum juggling).
    cargo install ripgrep --version "${RIPGREP_VERSION}" --locked
fi

echo "==> Checking git (for the git tools)"
if ! command -v git >/dev/null 2>&1; then
    echo "ERROR: git is required but was not found on PATH." >&2
    exit 1
fi

echo "==> Done. Build with: cargo build --release"
