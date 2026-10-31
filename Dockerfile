# slsk-mcp as a container. Packages a binary built outside it: CI builds the
# static musl binary per architecture (the `linux-build` job) and builds this
# with dist/ as the context, holding <arch>/slsk-mcp, the same binaries a
# release attaches. By hand: `docker build -f Dockerfile dist`.

FROM scratch

ARG TARGETARCH

# rustls-platform-verifier needs a system trust store on Linux and does not
# fall back to compiled-in roots; without this, every HTTPS request —
# MusicBrainz, the Cover Art Archive, the identity provider — fails.
COPY --from=gcr.io/distroless/static:latest@sha256:15f8637509792e05b97d83451b0b6ccfa4abd548f997e406ac1e72c87fe1fa78 \
     /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

COPY ${TARGETARCH}/slsk-mcp /slsk-mcp

EXPOSE 8080 8081

USER 1000:1000

LABEL org.opencontainers.image.title="slsk-mcp"
LABEL org.opencontainers.image.licenses="MIT"
LABEL org.opencontainers.image.source="https://github.com/radiosilence/slsk-mcp"

ENTRYPOINT ["/slsk-mcp"]
