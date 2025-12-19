# Build image ARG
ARG RUST_IMAGE=rust:1.85-alpine

# Build stage
FROM ${RUST_IMAGE} AS builder

RUN apk add --no-cache musl-dev protobuf-dev protoc

WORKDIR /build

# Cache dependencies
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release || true
RUN rm -rf src

# Build actual binary
COPY . .
RUN cargo build --release

# Runtime stage - scratch for minimal attack surface
FROM scratch

# Copy CA certificates for TLS to Kubernetes API
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

COPY --from=builder /build/target/release/node-local-cache /usr/local/bin/

ENTRYPOINT ["/usr/local/bin/node-local-cache"]
