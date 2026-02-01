# ---------- build stage ----------
FROM rust:1.85-bookworm AS builder

WORKDIR /app

# Cache dependencies
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

# Copy full source
COPY . .

# Build actual binary
RUN cargo build --release

# ---------- runtime stage ----------
FROM debian:bookworm-slim

WORKDIR /app

# Runtime dependencies
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN useradd -m -u 10001 appuser
USER appuser

# Copy binary only
COPY --from=builder /app/target/release/blog-backend /app/blog-backend

# Runtime configuration
ENV PORT=3000
ENV DATA_PATH=/data/movies.json
ENV RUST_LOG=info

EXPOSE 3000

CMD ["/app/blog-backend"]
