# Pin the official multi-arch image so a stale or moved `latest` tag cannot
# silently change the filesystem layout used by this build.
FROM rustdesk/rustdesk-server:1.1.15@sha256:10818ec05b179039c6660f4d8e74b303f0db2858bbad2b18e24992ea22d54cd6 AS upstream

# O hbbs OSS 1.1.15 nao responde ao handshake TCP seguro que o cliente
# autenticado >= 1.4.1 exige. Este commit imutavel e o PR upstream #699,
# limitado a implementar essa troca e seus testes de compatibilidade.
FROM rust:1-slim-bookworm AS secure-hbbs-builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates cmake git libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --recurse-submodules https://github.com/rylos/rustdesk-server.git /src \
    && cd /src \
    && git checkout b9d886495f4cabcaba5e4ecea71262dcf2d42776 \
    && git submodule update --init --recursive
WORKDIR /src
RUN cargo test --locked --test key_exchange -- --nocapture \
    && cargo build --locked --release --bin hbbs

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /usr/local/bin
# Mantemos o hbbr oficial 1.1.15; somente o hbbs recebe a correcao.
COPY --from=secure-hbbs-builder /src/target/release/hbbs /usr/local/bin/hbbs
COPY --from=upstream /usr/bin/hbbr /usr/local/bin/hbbr
COPY deploy/rustdesk-server/hbbs-entrypoint.sh /usr/local/bin/hbbs-entrypoint
RUN chmod +x /usr/local/bin/hbbs /usr/local/bin/hbbr /usr/local/bin/hbbs-entrypoint

ENTRYPOINT ["/usr/bin/tini", "--"]
