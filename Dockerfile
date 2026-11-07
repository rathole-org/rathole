FROM rust:1.92.0-bookworm@sha256:e90e846de4124376164ddfbaab4b0774c7bdeef5e738866295e5a90a34a307a2 AS builder
WORKDIR /home/rust/src
COPY . .
ARG FEATURES
ARG SOURCE_DATE_EPOCH
ARG VERGEN_GIT_BRANCH
ARG VERGEN_GIT_COMMIT_TIMESTAMP
ARG VERGEN_GIT_DESCRIBE
ARG VERGEN_GIT_SHA
RUN cargo build --locked --release --features "${FEATURES:-default}"
RUN mkdir -p build-out/ && cp target/release/rathole build-out/

FROM gcr.io/distroless/cc-debian12@sha256:e5d81ddde149641e2a9ba55be4545bc125c67de07508b03ba4c22e6eb0ded5aa
WORKDIR /app
COPY --from=builder /home/rust/src/build-out/rathole .
USER 1000:1000
ENTRYPOINT ["./rathole"]
