# Build each requested platform in its own Rust image. Do not select a Rust target
# from TARGETARCH: the shared publishing action supplies a non-Docker value there.
# Base images by digest; Dependabot moves the digests (.github/dependabot.yml).
FROM rust:1.98.1-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730 AS build
# python3 writes the third-party notices from the locked dependency set.
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock LICENSE ./
COPY crates/ crates/
COPY data/ data/
COPY migrations/ migrations/
COPY scripts/third-party-notices scripts/
COPY scripts/spdx/ scripts/spdx/
RUN cargo build --locked --release -p pinakesd \
    && strip target/release/pinakesd \
    && cargo fetch --locked --target "$(rustc --print host-tuple)" \
    && install -d notices \
    && scripts/third-party-notices notices/THIRD-PARTY-NOTICES \
    && cp LICENSE data/LICENSE.* data/oembed-LICENSE notices/

FROM debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e AS runtime
LABEL org.opencontainers.image.source="https://github.com/pubky/pinakes"
# Security updates: Dependabot moves the base digest, and the packages installed here come
# from the distribution when the image is rebuilt.
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 pinakes \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin pinakes \
    && install -d -o 10001 -g 10001 /var/lib/pinakes/dumps /var/lib/pinakes/catalogues /var/lib/pinakes/search-index
COPY --from=build /build/target/release/pinakesd /usr/local/bin/pinakesd
# Pinakes' licence, the licences of the data sets built into the binary and the notices of
# every crate it links.
COPY --from=build /build/notices/ /usr/share/doc/pinakes/
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
