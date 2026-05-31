# ───── builder: ビルド + テスト ─────
FROM rust:1-bookworm AS builder
WORKDIR /app

# 依存だけ先にキャッシュ
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release 2>/dev/null || true
RUN rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

# ───── runtime: 最小実行イメージ ─────
FROM debian:bookworm-slim AS runtime
LABEL org.opencontainers.image.title="enl" \
      org.opencontainers.image.description="ECHONET Lite 専用 CLI (ステートレス / one-shot)"

COPY --from=builder /app/target/release/enl /usr/local/bin/enl

# 3610 を専有する前提。host network で実行すること (compose 参照)。
EXPOSE 3610/udp
ENTRYPOINT ["enl"]
CMD ["--help"]
