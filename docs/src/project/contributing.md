# Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Development setup

See [Installation](../getting-started/installation.md) to get a working build.
The development loop is standard for a Rust workspace:

```bash
cargo fmt --all              # format
cargo clippy --all-targets --all-features   # lint
cargo test --all-features    # tests
```

CI runs all three on every pull request (pinned to the same toolchain via
`rust-toolchain.toml`), so a green local run is a good signal.

## Editing this book

The documentation lives in [`docs/`](https://github.com/cats-of-the-world/boitata/tree/master/docs)
and is built with [mdBook](https://rust-lang.github.io/mdBook/):

```bash
cargo install mdbook      # if you don't have it
mdbook serve docs/        # live preview at http://localhost:3000
mdbook build docs/        # render to docs/book/
```

The table of contents is [`docs/src/SUMMARY.md`](https://github.com/cats-of-the-world/boitata/blob/master/docs/src/SUMMARY.md).
Add a chapter by creating a Markdown file and listing it there.

## Acknowledgments

- **Stripe's Minions** — for the blueprint architecture and determinism-first
  philosophy.
- **Block's Goose** — for the modular Rust architecture and MCP integration
  patterns.

## License

MIT License — see the [LICENSE](https://github.com/cats-of-the-world/boitata/blob/master/LICENSE)
file for details.
