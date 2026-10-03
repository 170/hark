# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends libasound2-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends libasound2 espeak-ng \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 hark \
    && useradd --uid 10001 --gid hark --home-dir /home/hark --create-home hark \
    && install -d -o hark -g hark -m 0750 /run/hark /var/cache/hark \
    && install -d -o hark -g hark -m 0700 /home/hark/.run
COPY --from=build /build/target/release/hark /usr/local/bin/hark
COPY LICENSE /usr/share/doc/hark/LICENSE
ENV HOME=/home/hark XDG_RUNTIME_DIR=/home/hark/.run XDG_CACHE_HOME=/var/cache
USER hark
# Check the binary's runtime libraries without opening a microphone.
RUN hark --help && espeak-ng --voices=en
EXPOSE 8765
ENTRYPOINT ["hark"]
CMD ["serve", "--socket", "/run/hark/hark.sock", "--websocket", "0.0.0.0:8765"]
