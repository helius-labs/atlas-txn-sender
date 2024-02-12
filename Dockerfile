# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.76.0
ARG APP_NAME=atlas-txn-sender
FROM rust:${RUST_VERSION}-slim-bullseye AS build
WORKDIR /app
RUN apt-get update && apt-get -y install libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang cmake make libprotobuf-dev protobuf-compiler
RUN --mount=type=bind,source=src,target=src \
    --mount=type=bind,source=Cargo.toml,target=Cargo.toml \
    --mount=type=bind,source=Cargo.lock,target=Cargo.lock \
    --mount=type=cache,target=/app/target/ \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    <<EOF
set -e
cargo build --locked --release
cp ./target/release/atlas-txn-sender /bin/atlas-txn-sender
EOF

FROM debian:bullseye-slim AS final

ARG UID=10001
RUN adduser \
    --disabled-password \
    --gecos "" \
    --home "/nonexistent" \
    --shell "/sbin/nologin" \
    --no-create-home \
    --uid "${UID}" \
    rustuser
USER rustuser

# Copy the executable from the "build" stage.
COPY --from=build /bin/atlas-txn-sender /bin/

# # Expose the port that the application listens on.
EXPOSE 4040

# What the container should run when it is started.
CMD ["/bin/atlas-txn-sender"]