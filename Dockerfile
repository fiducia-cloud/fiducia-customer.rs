# syntax=docker/dockerfile:1
# Multi-stage build for fiducia-backend.
FROM rust:1.97.0-slim-bookworm@sha256:cfbb0e0ef7a73e736386bfa346f1cb0503c6d162969dc9426fb37834f3f64c25 AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates
WORKDIR /build
# Immutable cross-repository input. Bump this SHA together with the CI checkout.
ARG INTERFACES_SHA=487e470c45ab5851e8f6f3b1dc048fe067fbf408
RUN git init fiducia-interfaces \
    && git -C fiducia-interfaces remote add origin \
       https://github.com/fiducia-cloud/fiducia-interfaces.git \
    && git -C fiducia-interfaces fetch --depth 1 origin "$INTERFACES_SHA" \
    && git -C fiducia-interfaces checkout --detach FETCH_HEAD \
    && test "$(git -C fiducia-interfaces rev-parse HEAD)" = "$INTERFACES_SHA"
COPY . fiducia-customer.rs
WORKDIR /build/fiducia-customer.rs
RUN cargo build --locked --release && strip target/release/fiducia-backend

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:ce0d66bc0f64aae46e6a03add867b07f42cc7b8799c949c2e898057b7f75a151
COPY --from=build --chown=65532:65532 /build/fiducia-customer.rs/target/release/fiducia-backend /usr/local/bin/fiducia-backend
ENV STATIC_DIR=/app/static
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fiducia-backend"]
