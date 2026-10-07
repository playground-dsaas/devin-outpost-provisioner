FROM rust:1.96-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin org-provisioner

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/org-provisioner /usr/local/bin/org-provisioner
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/org-provisioner"]
