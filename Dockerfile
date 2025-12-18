# Build stage
FROM rust:1.83-alpine AS builder

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
FROM alpine:3.20

RUN apk add --no-cache ca-certificates

COPY --from=builder /build/target/release/node-local-cache /usr/local/bin/

ENTRYPOINT ["node-local-cache"]
