# Image for the `containerized_task` example blueprint.
#
# That blueprint's `agent_sandbox` steps launch `boitata-agent` (an ACP server)
# *inside* the container, so the image must carry the `boitata-agent` binary on
# PATH, plus git and the Rust toolchain the `cargo test` step uses.
#
# Build it from the repository root, tagged to match the blueprint's `image`:
#
#   docker build -f examples/boitata-agent-rust.Dockerfile -t boitata-agent-rust:latest .
#
# Runtime: the agent needs a provider config (boitata.toml / $BOITATA_CONFIG) and,
# for a hosted provider, credentials in the environment. A keyless placeholder
# config is baked below so the agent starts out of the box; override it (mount your
# own, or set $BOITATA_CONFIG) and supply the provider's API key to do real work.

# --- Stage 1: build the agent binary against the pinned toolchain ---
FROM rust:latest AS build
WORKDIR /src
# Copy only what the build needs (not target/, examples/, docs/) for a small,
# cache-friendly context.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p boitata-agent

# --- Stage 2: runtime image with the toolchain, git, and the agent ---
FROM rust:latest
RUN apt-get update \
    && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/boitata-agent /usr/local/bin/boitata-agent

# A keyless placeholder provider so `boitata-agent` starts without baked secrets.
# Replace this (or point $BOITATA_CONFIG elsewhere) and pass credentials at run
# time for a real LLM.
RUN printf 'provider = "ollama"\nmodel = "llama3"\n' > /etc/boitata.toml
ENV BOITATA_CONFIG=/etc/boitata.toml
