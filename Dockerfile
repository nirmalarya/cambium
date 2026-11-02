# Cambium server image.
#  - Builder and runtime bases are pinned by digest (update deliberately, via PR).
#  - Runtime is distroless (no shell, no package manager), runs as UID 65532 (nonroot),
#    and works with a read-only root filesystem: all state lives under /data.
#  - The CycloneDX SBOM for the Rust dependency tree ships inside the image at
#    /usr/share/cambium/sbom/ (regenerate with `cargo cyclonedx -f json`, then rewrite local `path+file://` refs to `path+file:///src/`).
# Build target `tools` adds git for the one-time `cmbd import-git` migration job.
ARG RUST_IMAGE=rust:1.99.1-slim-bookworm@sha256:24707e70a4f1e23d9a7b25202ec43126d28a52294b96aad5c670df1fd1f1d400
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
ARG TOOLS_IMAGE=debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY docs/schema.sql docs/schema-pg.sql docs/
COPY docs/migrations docs/migrations
RUN cargo build --release --locked --bin cmbd --bin cmb && mkdir -p /out/data && cp target/release/cmbd target/release/cmb /out/

FROM ${TOOLS_IMAGE} AS tools
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates && rm -rf /var/lib/apt/lists/* \
 && useradd -r -u 65532 nonroot && mkdir -p /data && chown 65532 /data
COPY --from=build /out/cmbd /out/cmb /usr/local/bin/
USER 65532:65532
ENV CAMBIUM_DATA=/data
ENTRYPOINT ["cmbd"]

FROM ${RUNTIME_IMAGE} AS runtime
COPY --from=build /out/cmbd /out/cmb /usr/local/bin/
COPY --from=build --chown=65532:65532 /out/data /data
COPY sbom/ /usr/share/cambium/sbom/
USER 65532:65532
ENV CAMBIUM_DATA=/data CAMBIUM_ADDR=0.0.0.0:7878
VOLUME /data
EXPOSE 7878
ENTRYPOINT ["/usr/local/bin/cmbd"]
LABEL org.opencontainers.image.title="Cambium" org.opencontainers.image.description="Server-side version control (cmbd)" org.opencontainers.image.licenses="MIT"
