FROM rust:1.56.0 as build-env
WORKDIR /app
COPY . /app
RUN cargo build --release --package minerp-server

FROM gcr.io/distroless/cc
COPY --from=build-env /app/target/release/minerp-server /
CMD ["./minerp-server"]