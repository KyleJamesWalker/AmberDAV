# Multi-stage multi-platform build
FROM --platform=$BUILDPLATFORM rust:alpine AS builder
RUN apk add --no-cache musl-dev gcc make

WORKDIR /app
COPY . .

ARG TARGETARCH
RUN case "${TARGETARCH}" in \
      "amd64") TARGET="x86_64-unknown-linux-musl" ;; \
      "arm64") TARGET="aarch64-unknown-linux-musl" ;; \
      *) echo "Unsupported architecture: ${TARGETARCH}"; exit 1 ;; \
    esac && \
    rustup target add "${TARGET}" && \
    cargo build --release --target "${TARGET}" --locked && \
    mv target/${TARGET}/release/amber-dav /app/amber-dav

# Final runner container
FROM alpine:3.20
RUN apk add --no-cache ca-certificates tzdata

# Create a non-root group and user
RUN addgroup -S amberdav && adduser -S -G amberdav amberdav

# Default directory to serve WebDAV files from
RUN mkdir -p /data && chown -R amberdav:amberdav /data
WORKDIR /data

# Copy the static headless binary
COPY --from=builder /app/amber-dav /usr/local/bin/amber-dav

USER amberdav
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/amber-dav"]
CMD ["--root", "/data"]
