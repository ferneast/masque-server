# Stage 1: Build static binary
FROM rust:1-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release --target $(uname -m)-unknown-linux-musl \
    && cp target/$(uname -m)-unknown-linux-musl/release/masque-tunnel /masque-tunnel

# Stage 2: Minimal runtime image
FROM scratch

COPY --from=builder /masque-tunnel /masque-tunnel

EXPOSE 443/udp

ENTRYPOINT ["/masque-tunnel"]
