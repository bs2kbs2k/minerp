FROM scratch
COPY target/x86_64-unknown-linux-musl/release/minerp-server /minerp-server
ENTRYPOINT /minerp-server
