# Build stage
FROM rust:1.82-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Vendor-less dependency warm-up: create a stub so layers cache deps.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release || true
COPY . .
RUN touch src/main.rs && cargo build --release --locked

# Runtime stage: no shell tooling required, run as non-root.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 sandbox
COPY --from=build /app/target/release/wasm-plugin-sandbox /usr/local/bin/wasm-plugin-sandbox
USER sandbox
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/wasm-plugin-sandbox"]
