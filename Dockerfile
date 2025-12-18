# Build stage
ARG RUST_IMAGE=rust:1.85-alpine
FROM ${RUST_IMAGE} AS builder

RUN apk add --no-cache musl-dev protobuf-dev protoc

WORKDIR /build

# Cache dependencies
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null || true
RUN rm -rf src

# Build actual binary
COPY . .
RUN cargo build --release

# Runtime stage
ARG ALPINE_IMAGE=alpine:3.20
FROM ${ALPINE_IMAGE}

RUN apk add --no-cache ca-certificates

COPY --from=builder /build/target/release/node-local-cache /usr/local/bin/

ENTRYPOINT ["node-local-cache"]
