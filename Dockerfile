FROM rust:1.85 AS builder

WORKDIR /app
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim

WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/blog-backend /app/blog-backend

ENV PORT=3000
ENV DATA_PATH=/app/data/movies.json
ENV RSS_URL=https://letterboxd.com/istangel/rss/

EXPOSE 3000
CMD ["/app/blog-backend"]
