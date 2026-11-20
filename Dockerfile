# Watcher image. Build from the repository root:
#   docker build -t warn-watcher .
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p warn-watcher

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 watcher \
 && mkdir /data && chown watcher /data
COPY --from=build /src/target/release/warn-watcher /usr/local/bin/warn-watcher
USER watcher
VOLUME /data
ENV WARN_CONFIG=/data/config.toml
ENTRYPOINT ["warn-watcher"]
CMD ["run"]
