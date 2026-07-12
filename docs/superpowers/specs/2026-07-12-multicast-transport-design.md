# multicast transport 対応 設計

日付: 2026-07-12
ステータス: 承認済み（実装は別セッション）

## 背景 / 動機

Panasonic リンクプラス無線アダプタ (WTY2001) は ECHONET Lite AIF 認証済み機器だが、
**unicast で 3610 に届くフレームを一切処理せず、マルチキャスト (224.0.23.0) 宛の
フレームにのみ応答する**ことを実機で確認した（multicast 宛 Get には GetRes を返し、
直後の同一内容 unicast Get は無視。応答自体は unicast・エフェメラル送信元ポートで返る）。

現在の enl は全コマンドが unicast 送信のみのため、この種の機器を **discover で発見できず、
get/set でも操作できない**。実測では同一 LAN 上で sweep discovery が 2 ノードしか
見つけられないのに対し、multicast discovery には 8 ノードが応答した。

## スコープ

- `get` / `set` / `describe` / `raw` に `--multicast` フラグを追加（送信先の切替のみ）。
- `discover` を sweep + multicast の常時併用にする。
- 応答マッチングを TID 一致まで強化する。

### 非スコープ

- 出力 JSON スキーマの変更（一切変えない）。
- exit code の変更。
- `listen` の変更（既に multicast join 済み）。
- 自動フォールバック（unicast 失敗時に multicast 再試行）は採用しない。
  失敗時の所要時間が読めなくなり one-shot の透明性が下がるため、明示フラグとする。

## CLI 仕様

### `--multicast` フラグ（get / set / describe / raw）

```
enl get 192.0.2.22 029101 80 --multicast
enl set 192.0.2.22 029101 80 30 --multicast
enl describe 192.0.2.22 029101 --multicast
enl raw 192.0.2.22 ... --multicast
```

- `ip` 引数は従来どおり必須。役割が「送信先」から「**応答を期待する送信元**」に変わる。
- 送信先だけ `224.0.23.0:3610` になる。応答は `ip` から来たフレームのみ採用。
- フラグ省略時は完全に従来動作（unicast）。

### `discover`: sweep + multicast 常時併用

- 同一ソケットから sweep（CIDR 内全ホストへ unicast Get `0EF001/D6`）に加え、
  同じフレームを multicast へ 1 発送信する。追加フラグなし。
- 収集ウィンドウは共通。既存の IP 重複排除（最初の正常応答を採用）で統合されるため、
  両方に応答する機器が二重に出ることはない。
- **CIDR 解決不能時の緩和**: 現在は `--cidr` / `-i` のどちらも無いとエラーだが、
  変更後は multicast のみで実行する（sweep スキップを stderr に warn）。
  `enl discover` が引数なしで動くようになる。

## 内部設計

### net.rs

- `send_and_recv_one` を「送信先 (dst)」と「応答を期待する IP (expect)」を分離した
  シグネチャに一般化する。unicast では dst.ip == expect、multicast では
  dst = 224.0.23.0:3610 / expect = 指定 IP。
- **応答採用条件の強化**: 従来の「expect IP 一致」に加えて
  `EHD == 0x1081` かつ `TID == 送信 TID` を要求する。
  multicast は他コントローラのトラフィックと混線しうるため必須。unicast にも適用する
  （ECHONET Lite 仕様上、応答 TID は要求 TID と一致する）。
- 判定は codec 非依存のバイト比較でよい:
  `data.len() >= 4 && data[0..2] == [0x10, 0x81] && data[2..4] == tid.to_be_bytes()`。
  これを純関数 `is_reply_candidate(data: &[u8], tid: u16) -> bool` として切り出し、
  ユニットテスト対象にする。不一致フレームは debug ログを出して受信を継続する
  （deadline 管理は現行ロジックを踏襲）。
- **multicast の egress インタフェースは v1 では制御しない**（ルーティングテーブル任せ）。
  std の `UdpSocket` に `IP_MULTICAST_IF` を設定する API が無く、制御するには
  `socket2`/`libc` の依存追加が必要になる。依存ゼロ方針を優先し、multi-homed 環境で
  意図しないインタフェースに流れるケースは既知の制約としてドキュメントに明記する
  （実需が出たら `-i` 連動の egress 制御を追加する）。
- multicast グループへの **join はしない**。応答は unicast で返るため不要。
  join しないので自分の送信フレームがループバックで戻る問題も発生しない。

### commands.rs

- `get` / `set` / `describe` / `raw` の各関数に `multicast: bool` を追加し、
  送信先 SocketAddr の組み立てだけ分岐する。応答処理は共通。
- `discover` は sweep 送信ループの後に multicast へ 1 発 `send_to` を追加。
  受信・集約は既存ロジック（ESV 応答判定・IP 集約）を使い、採用条件に
  `is_reply_candidate`（EHD + TID 一致）を追加する。sweep と multicast は同一 TID の
  同一フレームを使うためチェックは共通で済む。
- `resolve_cidr` 失敗を即エラーにせず「sweep なし」として扱う分岐を追加。

### main.rs / schema.rs

- clap 定義に `--multicast` を追加（get / set / describe / raw）。
- `discover` のヘルプ文を「sweep + multicast 併用」に更新。
- schema.rs は出力スキーマ不変のため変更なし。

## 互換性・エラー・ログ

- stdout JSON スキーマ: **全コマンドで不変**。discover に応答経路フィールドは足さない
  （YAGNI + スキーマ安定原則）。
- exit code: 不変（timeout=3 / SNA=4 / network・bind=5）。
- stderr: 送信時の tracing に `transport="multicast"` / `"unicast"` を含める。
- ドキュメント更新: CLAUDE.md と net.rs 冒頭コメントの「multicast 不採用」記述を
  「sweep と併用（multicast にしか応答しない実機が存在するため）」に改める。

## テスト戦略

- **ユニットテスト**
  - `is_reply_candidate`: EHD 不一致 / TID 不一致 / 短すぎるデータ / 一致、の各ケース。
  - discover の CIDR 省略分岐（sweep スキップで multicast のみになること）。
  - 既存の codec ラウンドトリップは変更なし（codec 非改変のため）。
- **ループバック統合テスト**
  - 127.0.0.1 上に fake device スレッドを立てる既存パターンがあれば踏襲。
  - multicast のループバック送受信（`IP_MULTICAST_LOOP`）が CI 環境で安定しない場合、
    multicast 送信パスの統合テストは無理をせず手動検証に倒してよい。
- **実機検証（必須）**
  - WSL2 では検証不可: mirrored networking がエフェメラル送信元ポートの UDP 応答を
    取りこぼすため、multicast-only 機器の応答が届かない。
  - LAN 直結の Linux ホスト（aarch64）で行う: `aarch64-unknown-linux-musl` を
    クロスビルド → scp → 実機のリンクプラスに対して
    `discover`（発見できること）/ `get --multicast`（GetRes）/ `set --multicast`
    （SetRes + 実照明の点灯消灯）を確認する。
  - 実機の IP・MAC は spec・コード・テストに書かない（例示は RFC 5737 の 192.0.2.0/24）。

## 受け入れ条件

1. `enl discover`（引数なし）が multicast のみで動き、multicast-only 機器を発見できる。
2. `enl get <ip> <eoj> <epc> --multicast` がリンクプラスから GetRes を取得できる。
3. `enl set <ip> <eoj> 80 30 --multicast` で実照明が点灯し、SetRes が返る。
4. `--multicast` なしの全コマンドの挙動・出力・exit code が現状と一致する。
5. `cargo test` / `cargo clippy -- -D warnings` が通る。
