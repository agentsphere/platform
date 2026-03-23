# cli.just — Agent Runner CLI recipes

worktree := `bash hack/detect-worktree.sh`
agent_runner_dir := "/tmp/platform-e2e/" + worktree + "/agent-runner"

[group('cli')]
build:
    cargo build --release --manifest-path cli/agent-runner/Cargo.toml

# Cross-compile agent-runner for linux/amd64 and linux/arm64 (uses Docker)
[group('cli')]
cross dir=agent_runner_dir:
    mkdir -p {{ dir }}
    docker run --rm \
      -v "$(pwd)/cli/agent-runner:/src" \
      -v "{{ dir }}:/out" \
      -v "platform-cross-cargo-registry:/usr/local/cargo/registry" \
      -v "platform-cross-cargo-git:/usr/local/cargo/git" \
      -v "platform-cross-target:/src/target" \
      rust:1.88-slim-bookworm sh -c '\
        apt-get update && apt-get install -y --no-install-recommends \
          gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
          gcc-x86-64-linux-gnu libc6-dev-amd64-cross && \
        rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu && \
        cd /src && \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
          cargo build --release --target aarch64-unknown-linux-gnu && \
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
          cargo build --release --target x86_64-unknown-linux-gnu && \
        cp target/aarch64-unknown-linux-gnu/release/agent-runner /out/arm64 && \
        cp target/x86_64-unknown-linux-gnu/release/agent-runner /out/amd64'

[group('cli')]
install:
    cargo install --path cli/agent-runner

# `just cli test` or `just cli test some_test_name`
[group('cli')]
test *args="":
    cargo test --manifest-path cli/agent-runner/Cargo.toml --bin agent-runner {{args}}

[group('cli')]
lint:
    cargo clippy --manifest-path cli/agent-runner/Cargo.toml --all-features -- -D warnings

[group('cli')]
fmt:
    cargo fmt --manifest-path cli/agent-runner/Cargo.toml

# CLI pub/sub integration test (requires cluster)
[group('cli')]
test-pubsub:
    bash hack/cli-test-pubsub.sh

# Real Claude CLI tests (requires CLAUDE_OAUTH_TOKEN)
[group('cli')]
test-llm:
    cargo test --manifest-path cli/agent-runner/Cargo.toml --bin agent-runner llm_ -- --ignored

[group('cli')]
cov:
    cargo llvm-cov --manifest-path cli/agent-runner/Cargo.toml --bin agent-runner
