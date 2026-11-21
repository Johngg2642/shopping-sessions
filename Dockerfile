# syntax=docker/dockerfile:1
FROM rust:1.100-bookworm AS build
ARG TARGETARCH
ARG AMD64_TARGET_CPU=cascadelake
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN --mount=type=cache,id=shopping-registry-${TARGETARCH},target=/usr/local/cargo/registry \
    --mount=type=cache,id=shopping-target-${TARGETARCH}-${AMD64_TARGET_CPU},target=/build/target \
    if [ "$TARGETARCH" = amd64 ]; then \
        export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=${AMD64_TARGET_CPU} -C target-feature=+aes"; \
        cargo build --release --locked --target x86_64-unknown-linux-gnu && \
        cp target/x86_64-unknown-linux-gnu/release/shopping-sessions /shopping-sessions; \
    else \
        cargo build --release --locked && cp target/release/shopping-sessions /shopping-sessions; \
    fi

FROM build AS build-probe
ARG TARGETARCH
COPY scripts/phase_probe.rs scripts/
RUN --mount=type=cache,id=shopping-registry-${TARGETARCH},target=/usr/local/cargo/registry \
    --mount=type=cache,id=shopping-target-${TARGETARCH}-${AMD64_TARGET_CPU},target=/build/target \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=${AMD64_TARGET_CPU} -C target-feature=+aes" \
    cargo build --release --locked --target x86_64-unknown-linux-gnu --features phase-probe --bin phase-probe && \
    cp target/x86_64-unknown-linux-gnu/release/phase-probe /phase-probe

FROM debian:bookworm-slim AS runtime
COPY --from=build /shopping-sessions /usr/local/bin/shopping-sessions
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch HOME=/scratch TMPDIR=/scratch
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/shopping-sessions"]

FROM runtime AS benchmark
COPY --from=build-probe /phase-probe /usr/local/bin/phase-probe

FROM runtime AS scored
