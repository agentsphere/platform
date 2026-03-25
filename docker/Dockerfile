# Stage 1: UI
FROM node:22-slim AS ui-builder
WORKDIR /ui
COPY ui/package.json ui/package-lock.json* ./
RUN npm ci --ignore-scripts
COPY ui/ .
RUN npm run build

# Stage 2: Dependency recipe
FROM rust:1.93-slim-bookworm AS planner
RUN cargo install cargo-chef --locked
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 2.5: Cross-compile agent-runner for linux/amd64 and linux/arm64
FROM rust:1.93-slim-bookworm AS agent-runner-builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
    gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
  && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
WORKDIR /agent-runner
COPY cli/agent-runner/ .
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
RUN cargo build --release --target x86_64-unknown-linux-gnu
RUN cargo build --release --target aarch64-unknown-linux-gnu

# Stage 3: Build deps + binary
FROM rust:1.93-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
COPY --from=ui-builder /ui/dist ./ui/dist
ENV SQLX_OFFLINE=true
RUN cargo build --release

# Stage 4: Runtime — needs git for repo init, smart HTTP, MR merge, ops-repo, etc.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r -g 1000 platform \
    && useradd -r -s /sbin/nologin -u 1000 -g 1000 platform
COPY --from=builder /app/target/release/platform /platform
COPY --from=agent-runner-builder \
  /agent-runner/target/x86_64-unknown-linux-gnu/release/agent-runner \
  /data/agent-runner/amd64
COPY --from=agent-runner-builder \
  /agent-runner/target/aarch64-unknown-linux-gnu/release/agent-runner \
  /data/agent-runner/arm64
EXPOSE 8080
USER 1000
ENTRYPOINT ["/platform"]
