# syntax=docker/dockerfile:1
FROM rust:1.97-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends clang libclang-dev cmake pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /work/preconf
COPY . .
ENV CARGO_BUILD_JOBS=4
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git --mount=type=cache,target=/work/preconf/target cargo build --release --locked && cp /work/preconf/target/release/preconf-demo /usr/local/bin/preconf-demo

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 ca-certificates libssl3 libstdc++6 && rm -rf /var/lib/apt/lists/* && useradd --uid 10001 --create-home demo
WORKDIR /app/preconf
COPY --from=build /usr/local/bin/preconf-demo /app/preconf/target/release/preconf-demo
COPY scripts/ scripts/
COPY web/ web/
RUN mkdir -p /data && chown demo:demo /data
USER demo
EXPOSE 8080
STOPSIGNAL SIGTERM
CMD ["python3", "scripts/live.py", "--run-dir", "/data/live", "--host", "0.0.0.0"]
