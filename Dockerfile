# The official multi-architecture index pins resolve native ARM64/x64 images.
# OS package security updates are fetched at build time; package versions and
# the resulting image digest are recorded by acceptance, not called bit-identical.
FROM docker.io/library/rust:1.94.0-slim-trixie@sha256:f7bf1c266d9e48c8d724733fd97ba60464c44b743eb4f46f935577d3242d81d0 AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    g++ pkg-config python3 curl ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /source
ENV CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY examples/check_model_runtime.rs ./examples/check_model_runtime.rs
COPY scripts/assemble_container.py scripts/setup_onnx_runtime.py scripts/native_tls_runtime.py scripts/onnx_runtime_manifest.json scripts/container_entrypoint.py ./scripts/
COPY static ./static
COPY models/latest.onnx models/provenance.json ./models/
COPY ops/container-research.json ops/container-model-fixture.json ./ops/
COPY LICENSE.md THIRD_PARTY_NOTICES.md ./
RUN cargo build --locked --release --bin macbot --example check_model_runtime \
    && python3 -B scripts/assemble_container.py --output /assembled

FROM docker.io/library/debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 ca-certificates libstdc++6 libgcc-s1 && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 macbot && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /var/lib/macbot macbot \
    && install -d -m 0700 -o 10001 -g 10001 /var/lib/macbot
COPY --from=build /assembled /opt/macbot
# The native-loader trust policy requires the runtime directory/artifacts to
# belong to the effective non-root user. The root filesystem is read-only at run.
RUN chown -R 10001:10001 /opt/macbot/runtime && chmod 0700 /opt/macbot/runtime \
    && chmod 0444 /opt/macbot/runtime/* && chmod 0444 /opt/macbot/empty.env
WORKDIR /opt/macbot
USER 10001:10001
VOLUME ["/var/lib/macbot"]
EXPOSE 8000
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=5s --start-period=20s --retries=3 CMD ["python3", "-B", "/opt/macbot/container_entrypoint.py", "check"]
ENTRYPOINT ["python3", "-B", "/opt/macbot/container_entrypoint.py"]
CMD ["serve"]
