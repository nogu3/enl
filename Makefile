# Docker 上で開発 / 実行する。ローカルに Rust ツールチェインは不要。
#
#   make test      codec ラウンドトリップ等のテスト
#   make clippy    lint (warning を error 扱い)
#   make build     実行イメージをビルド
#   make run ARGS="discover"   enl を host network で実行

DEV_IMAGE := rust:1-bookworm
WORK := /app
# 依存ビルドキャッシュを名前付きボリュームで永続化
DEV_RUN := docker run --rm -v $(CURDIR):$(WORK) -v enl-cargo-registry:/usr/local/cargo/registry -v enl-target:$(WORK)/target -w $(WORK) $(DEV_IMAGE)

.PHONY: test clippy fmt build run shell clean

test:
	$(DEV_RUN) cargo test

clippy:
	$(DEV_RUN) sh -c "rustup component add clippy >/dev/null 2>&1; cargo clippy -- -D warnings"

fmt:
	$(DEV_RUN) sh -c "rustup component add rustfmt >/dev/null 2>&1; cargo fmt"

build:
	docker compose build

# 例: make run ARGS="get 192.0.2.10 013001 80"
run:
	docker compose run --rm enl $(ARGS)

shell:
	$(DEV_RUN) bash

clean:
	docker volume rm -f enl-cargo-registry enl-target
