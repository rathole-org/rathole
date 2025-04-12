FROM quay.io/sysdig/sysdig-ubi9:1 AS builder
RUN yum update && yum install -y rust cargo openssl-devel
WORKDIR /home/rust/src
COPY . .
ARG FEATURES
RUN cargo build --locked --release --features ${FEATURES:-default}
RUN mkdir -p build-out/
RUN cp target/release/rathole build-out/



FROM quay.io/sysdig/sysdig-mini-ubi9:1
RUN microdnf update -y
RUN microdnf install -y openssl-libs
WORKDIR /app
COPY --from=builder /home/rust/src/build-out/rathole .
USER 1000:1000
ENTRYPOINT ["./rathole"]
