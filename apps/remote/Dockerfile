FROM rust:1.97.1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY apps ./apps
RUN cargo build --locked --release --package kelp-remote

FROM debian:bookworm-slim
RUN groupadd --gid 10001 kelp && useradd --uid 10001 --gid kelp --no-create-home kelp \
    && mkdir /data && chown kelp:kelp /data
COPY --from=build /build/target/release/kelp-remote /usr/local/bin/kelp-remote
USER kelp
ENV KELP_LISTEN=0.0.0.0:8080 KELP_DATA_DIR=/data
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["kelp-remote"]
