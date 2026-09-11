# Runtime image for the published container (ghcr.io/arsalikhov/optimimer).
# Built by .github/workflows/release.yml from the static binaries in dist/;
# TARGETARCH is amd64 or arm64.
FROM debian:bookworm-slim
ARG TARGETARCH
RUN apt-get update \
 && apt-get install -y --no-install-recommends etherwake ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY dist/optimimer-backend-${TARGETARCH} /usr/bin/optimimer-backend
COPY backend/agents /usr/share/optimimer/agents
ENV OPTIMIMER_DB=/var/lib/optimimer/optimimer.db \
    VAULT_DIR=/var/lib/optimimer/vault \
    OPTIMIMER_AGENTS_DIR=/usr/share/optimimer/agents
VOLUME /var/lib/optimimer
WORKDIR /var/lib/optimimer
ENTRYPOINT ["/usr/bin/optimimer-backend"]
