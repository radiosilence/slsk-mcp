# slsk-mcp as a container. CI builds the static musl binary and this image
# only copies it in, so `docker build .` by hand needs dist/ populated first.

FROM scratch

ARG TARGETARCH

# rustls-platform-verifier needs a system trust store on Linux and does not
# fall back to compiled-in roots; without this, every HTTPS request —
# MusicBrainz, the Cover Art Archive, the identity provider — fails.
COPY --from=gcr.io/distroless/static:latest \
     /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

COPY dist/slsk-mcp-linux-${TARGETARCH}-musl /slsk-mcp

EXPOSE 8080 8081

USER 1000:1000

LABEL org.opencontainers.image.title="slsk-mcp"
LABEL org.opencontainers.image.licenses="MIT"
LABEL org.opencontainers.image.source="https://github.com/radiosilence/slsk-mcp"

ENTRYPOINT ["/slsk-mcp"]
