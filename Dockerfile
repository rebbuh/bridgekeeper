FROM rust:1.52-alpine as builder
RUN mkdir /build
RUN apk add --no-cache musl-dev python3 python3-dev openssl openssl-dev
ADD Cargo.toml /build/
WORKDIR /build
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
COPY src /build/src
COPY manifests /build/manifests
RUN touch src/main.rs && cargo build --release
RUN strip /build/target/release/bridgekeeper

FROM alpine:3.13
RUN apk add --no-cache python3 openssl libgcc
COPY --from=builder /build/target/release/bridgekeeper /usr/local/bin/