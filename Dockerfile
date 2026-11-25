# Build each requested platform in its own Rust image. Do not select a Rust target
# from TARGETARCH: the shared publishing action supplies a non-Docker value there.
FROM rust:1.98.1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY data/ data/
COPY migrations/ migrations/
RUN cargo build --locked --release -p pinakesd \
    && strip target/release/pinakesd

FROM debian:bookworm-slim AS runtime
# Security updates come from the distribution when the image is rebuilt.
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 pinakes \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin pinakes \
    && install -d -o 10001 -g 10001 /var/lib/pinakes/dumps /var/lib/pinakes/catalogues /var/lib/pinakes/search-index
COPY --from=build /build/target/release/pinakesd /usr/local/bin/pinakesd
ENV PINAKES__LISTEN=0.0.0.0:8090 \
    PINAKES__ADMIN_LISTEN=127.0.0.1:9090 \
    PINAKES__OSM__DUMP_DIR=/var/lib/pinakes/dumps \
    PINAKES__CATALOGUES__DIR=/var/lib/pinakes/catalogues \
    PINAKES__SEARCH__INDEX_DIR=/var/lib/pinakes/search-index
WORKDIR /var/lib/pinakes
USER 10001:10001
EXPOSE 8090
# GET /healthz on the admin listener (loopback in the container); the image has no curl.
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD ["/usr/local/bin/pinakesd", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/pinakesd"]
