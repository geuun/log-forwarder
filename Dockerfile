# syntax=docker/dockerfile:1
FROM rust:1.96.0-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# alpine(musl)에서 빌드하면 정적 바이너리가 나온다. 레지스트리·빌드 캐시는 재빌드 시간을 줄이려고 캐시 마운트에 둔다.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/log-forwarder /log-forwarder

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /log-forwarder /log-forwarder
ENTRYPOINT ["/log-forwarder"]
