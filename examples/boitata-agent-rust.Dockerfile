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
FROM rust:1.96.0 AS build
WORKDIR /src
# Copy only what the build needs (not target/, examples/, docs/) for a small,
# cache-friendly context.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p boitata-agent

# --- Stage 2: runtime image with the toolchain, git, and the agent ---
FROM rust:1.96.0
RUN apt-get update \
    && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/boitata-agent /usr/local/bin/boitata-agent

# Run as a non-root user. The agent executes shell/file/git (and `cargo`)
# commands on a cloned repo; a non-root identity shrinks the blast radius of any
# container escape and avoids accidentally writing root-owned files into the
# workspace that a later `ExecNode` step couldn't then modify.
RUN useradd --create-home --shell /bin/bash agent \
    && mkdir -p /workspace \
    && chown -R agent:agent /workspace
WORKDIR /workspace
USER agent

# Default provider config (a z.ai OpenAI-compatible endpoint). No key is baked in.
# Every field is overridable at run time by the matching env var the blueprint
# forwards — BOITATA_PROVIDER / BOITATA_MODEL / BOITATA_BASE_URL / BOITATA_API_KEY
# — so the image stays provider-agnostic and needs no rebuild to switch endpoint.
RUN printf 'provider = "openai"\nmodel = "glm-4.6"\nbase_url = "https://api.z.ai/api/paas/v4/chat/completions"\n' > /etc/boitata.toml
ENV BOITATA_CONFIG=/etc/boitata.toml
