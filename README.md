# enl

ECHONET Lite 専用 CLI。AIネイティブなスマートホーム CLI 群の第一弾。

ステートレス / one-shot。stdout は純粋な構造化 JSON、stderr は `tracing` 構造化ログ + 機械可読エラー JSON。
詳細な設計方針は [CLAUDE.md](./CLAUDE.md) を参照。

## 実行 (Docker)

3610 番ポート専有 + マルチキャスト受信のため **host network 必須**（bridge では機器応答を受信できない）。
ローカルに Rust ツールチェインは不要。

```bash
make build                                    # 実行イメージをビルド
docker compose run --rm enl discover          # ノード探索
docker compose run --rm enl get 192.0.2.10 013001 80      # 取得 (家庭用エアコン 0x80 動作状態)
docker compose run --rm enl set 192.0.2.10 013001 80 30   # 設定 (ON)
docker compose run --rm enl describe 192.0.2.10 013001    # プロパティマップ introspection
RUST_LOG=debug docker compose run --rm enl discover        # 診断ログを stderr に
```

> ⚠️ Home Assistant 等の ECHONET 統合が 3610 を握っていると応答を奪われる。検証中は停止すること。
> サンプル IP は RFC 5737 のドキュメント用 `192.0.2.0/24`。実機 IP に置き換えて使う。

## サブコマンドと出力スキーマ

- `discover [--timeout-ms 3000]` — `{"devices":[{"ip","count","instances":[...]}]}`
- `get <ip> <eoj> <epc...> [--timeout-ms 2000]` — `{"ip","eoj","esv","properties":[{"epc","pdc","edt_hex","value?"}]}`
- `set <ip> <eoj> <epc> <edt> [--timeout-ms 2000]` — `{"ip","eoj","esv","result":"accepted","properties":[...]}`
- `describe <ip> <eoj> [--timeout-ms 2000]` — `{"ip","eoj","esv","get_map":[...],"set_map":[...],"inf_map":[...]}`

`eoj`/`epc`/`edt` は hex。バイナリ値は常に `edt_hex` を含み、デコード辞書にあれば `value` を併記する。

## exit code (cron / n8n が分岐できる)

| code | 意味 |
|---|---|
| 0 | 成功 |
| 2 | CLI 引数エラー (clap 既定) |
| 3 | タイムアウト (応答なし) |
| 4 | 機器リジェクト (SNA) |
| 5 | ネットワーク / バインド失敗 |
| 1 | その他想定外 |

## 開発

```bash
make test      # codec ラウンドトリップ等のテスト (Docker 内)
make clippy    # lint (-D warnings)
make fmt       # rustfmt
```

## 構成

- `src/codec.rs` — フレームのデータモデル + parse/build。依存ゼロ手書き。ラウンドトリップテストで非対称バグを防ぐ。
- `src/properties.rs` — 任意のデコードレイヤ。プロパティマップ (15以下/16以上の2形式) パーサ含む。
- `src/net.rs` — UDP ソケット層 (0.0.0.0:3610 専有、マルチキャスト join)。
- `src/commands.rs` — discover / get / set / describe。
- `src/error.rs` — 機械可読エラー + exit code。
- `src/main.rs` — clap CLI。
