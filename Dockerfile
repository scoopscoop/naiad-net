# naiad-repo container image. The build runs inside the image, so the host
# only needs Docker — no Rust toolchain.
#
#   docker compose up -d --build          # via docker-compose.yml, or:
#   docker build -t naiad-repo .
#   docker run -d -p 9090:9090 -v naiad-repo-data:/data naiad-repo
#
# Admin subcommands work without --db because the image sets NAIAD_REPO_DB;
# the binary finds the right database automatically via the env tier:
#   docker exec <container> naiad-repo account list

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# Cache mounts keep the registry and target dir across builds, so an
# incremental source change doesn't recompile the whole dependency tree.
# The binary is copied out because cache mounts are not part of the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release -p naiad-server \
    && cp target/release/naiad-repo /naiad-repo

FROM debian:bookworm-slim
# curl: HEALTHCHECK below. sqlite3: live backups via `docker exec <c> sqlite3 /data/repo.db .backup dest.db`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl sqlite3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /naiad-repo /usr/local/bin/naiad-repo

# repo.db, repo.toml (scaffolded on first run), repo.key, and bridge-state.db
# all live beside each other in /data.
WORKDIR /data
VOLUME /data
EXPOSE 9090

# 0.0.0.0 inside the container: the built-in 127.0.0.1 default would make the
# published port unreachable. Reachability from outside the host is still
# governed by the `ports:` mapping / -p flag.
# ENV beats baked-in flags: every setting here is overridable with -e or a
# compose environment: block without rebuilding the image.
ENV NAIAD_REPO_DB=/data/repo.db \
    NAIAD_REPO_ADDR=0.0.0.0:9090
ENTRYPOINT ["naiad-repo"]
CMD ["serve"]

# Assumes port 9090 (override NAIAD_REPO_ADDR if you change it).
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD curl -fsS http://127.0.0.1:9090/health > /dev/null || exit 1
