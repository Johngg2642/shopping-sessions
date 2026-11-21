# syntax=docker/dockerfile:1
FROM rust:1.100-bookworm AS build
ARG TARGETARCH
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN --mount=type=cache,id=shopping-registry-${TARGETARCH},target=/usr/local/cargo/registry \
    --mount=type=cache,id=shopping-target-${TARGETARCH},target=/build/target \
    if [ "$TARGETARCH" = amd64 ]; then \
        export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+aes"; \
        cargo build --release --locked --target x86_64-unknown-linux-gnu && \
        cp target/x86_64-unknown-linux-gnu/release/shopping-sessions /shopping-sessions; \
    else \
        cargo build --release --locked && cp target/release/shopping-sessions /shopping-sessions; \
    fi

FROM debian:bookworm-slim
COPY --from=build /shopping-sessions /usr/local/bin/shopping-sessions
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch HOME=/scratch TMPDIR=/scratch
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/shopping-sessions"]
