FROM rust:1.99-slim-trixie

ARG ZELLIJ_VERSION=0.45.1
ARG ZELLIJ_SHA256=40bcc2e03f5d5ae8e054e39f676081fe12ab70871506996ba595834c3718eefc

RUN apt-get update \
    && apt-get install -y --no-install-recommends tmux ca-certificates procps util-linux tzdata \
    && rm -rf /var/lib/apt/lists/*

ADD --checksum=sha256:${ZELLIJ_SHA256} \
    https://github.com/zellij-org/zellij/releases/download/v${ZELLIJ_VERSION}/zellij-x86_64-unknown-linux-musl.tar.gz \
    /tmp/zellij.tar.gz

RUN tar -xzf /tmp/zellij.tar.gz -C /usr/local/bin zellij \
    && rm /tmp/zellij.tar.gz \
    && mkdir -m 1777 /cargo-home

ENV CARGO_HOME=/cargo-home \
    CARGO_CACHE_AUTO_CLEAN_FREQUENCY=never
