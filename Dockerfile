# syntax=docker/dockerfile:1

FROM rust:1.97-slim-trixie AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The static data and a fresh world's starting data are compiled into the
# server (crates/eve-server/src/sde.rs, startup.rs).
COPY db ./db
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p eve-server --bins \
 && cp target/release/eve-server target/release/fakeclient /usr/local/bin/

FROM debian:trixie-slim
COPY --from=build /usr/local/bin/eve-server /usr/local/bin/fakeclient /usr/local/bin/
COPY crates/eve-marshal/data/strings.txt /app/strings.txt
# The server writes its log and undecodable frames into ./logs on the host
# (docker-compose.yml), so it runs as the host user that owns that folder:
# UID/GID 1000, the first user on Linux and WSL. Build with
# --build-arg UID=$(id -u) --build-arg GID=$(id -g) for another.
ARG UID=1000
ARG GID=1000
RUN groupadd --gid "$GID" trinity \
 && useradd --uid "$UID" --gid "$GID" --no-create-home --shell /usr/sbin/nologin trinity \
 && mkdir -p /app/dumps /app/data && chown trinity:trinity /app/dumps /app/data
USER trinity
WORKDIR /app
ENV RMR_BIND=0.0.0.0:26000 \
    RMR_WEB=0.0.0.0:8080 \
    RMR_STRINGS=/app/strings.txt \
    RMR_DUMP_DIR=/app/dumps
EXPOSE 26000 8080
CMD ["eve-server"]
