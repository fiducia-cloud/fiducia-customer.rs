# Standalone image for local / non-cluster use. The AWS + Hetzner clusters build
# this backend from source in-pod (see the k8s-cluster dd-fiducia-rs deployment),
# so this Dockerfile is a convenience, not the deploy path.
FROM rust:1.90-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app
RUN useradd -u 1000 -m app
COPY --from=build /app/target/release/fiducia-backend /usr/local/bin/fiducia-backend
COPY static ./static
USER app
ENV PORT=8080
EXPOSE 8080
CMD ["fiducia-backend"]
