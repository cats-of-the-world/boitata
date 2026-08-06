# Installation

Boitata is a Rust workspace. Building it needs the Rust toolchain plus two
external tools: `ripgrep` (backs the `search` tool) and `git` (backs the `git_*`
tools).

## Prerequisites

| Tool | Why | Minimum |
|------|-----|---------|
| Rust toolchain | Build the project | pinned by `rust-toolchain.toml` |
| `ripgrep` (`rg`) | The `search` tool | 14.x |
| `git` | The `git_*` tools | any recent release |

The toolchain channel is pinned in
[`rust-toolchain.toml`](https://github.com/cats-of-the-world/boitata/blob/master/rust-toolchain.toml).
`rustup` reads it automatically, so local builds, CI, and new machines all use
the same compiler. Crate versions are locked by the committed `Cargo.lock`.

## Automated setup

To set up a new machine deterministically:

```bash
./scripts/setup.sh
```

This installs the exact pinned Rust toolchain, installs the pinned `ripgrep`
release, and checks for `git`.

## Build from source

```bash
git clone https://github.com/cats-of-the-world/boitata.git
cd boitata
cargo build --release
```

The binary lands at `./target/release/boitata`.

There is no published binary or crate yet, so build from source for now.

## Verify the install

```bash
./target/release/boitata --help
```

Next, configure a provider in [Quick Start](./quick-start.md).
