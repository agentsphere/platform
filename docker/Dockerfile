# Stage 1: UI
FROM node:22-slim AS ui-builder
WORKDIR /ui
COPY ui/package.json ui/package-lock.json* ./
RUN npm ci --ignore-scripts
COPY ui/ .
RUN npm run build

# Stage 2: Dependency recipe
FROM rust:1.88-slim-bookworm AS planner
RUN cargo install cargo-chef --locked
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Build deps + binary
FROM rust:1.88-slim-bookworm AS builder
RUN apt-get update && apt-get install -y mold && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
COPY --from=ui-builder /ui/dist ./ui/dist
ENV SQLX_OFFLINE=true
RUN cargo build --release

# Stage 4: Runtime
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/platform /platform
EXPOSE 8080
ENTRYPOINT ["/platform"]
