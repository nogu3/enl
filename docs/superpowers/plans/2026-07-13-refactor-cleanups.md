# リファクタリング 5 件 (schema enum / codec 長さ検証 / map_entry_json / ListenFilter / ErrKind::Usage) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** コードベース調査で挙がった改善 5 件をすべて実装する — (1) ユーザー入力エラーの `ErrKind::Usage` 化、(2) `SchemaTarget` を schema.rs へ移して網羅 match 化、(3) codec `build` の OPC/PDC 上限検証、(4) describe のマップエントリ JSON を properties.rs へ集約、(5) listen フィルタの `ListenFilter` 構造体化。

**Architecture:** 既存 6 モジュール構成 (main / commands / codec / properties / net / error / schema) は変えない。変更はすべて既存モジュール内の型・関数の移動と強化で、新規ファイルは作らない。stdout JSON スキーマは不変。stderr の `kind` に `usage` が加わり `internal` が消える (exit code は 1 のまま)。

**Tech Stack:** Rust (edition 2021)、clap 4 (derive)、serde_json、tracing。依存追加なし。

## Global Constraints

- 依存 crate を追加しない (codec は依存ゼロの手書き、CLAUDE.md 方針)。
- stdout は純粋な構造化 JSON のみ。ログ・診断は stderr (`tracing`)。
- exit code 契約: 0 成功 / 2 clap 引数エラー (他用途で使わない) / 3 timeout / 4 SNA / 5 network|bind / 1 その他。今回追加する `usage` は **exit 1**。
- 各タスク完了時に `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` が全部通ること。
- コミットメッセージは日本語 Conventional Commits (`refactor:` / `fix:` / `docs:`)、末尾に `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。
- 未知の機器・EPC で壊れない原則: 受信側 (parse) の挙動は一切変えない。今回の検証強化は送信側 (build) のみ。

---

### Task 1: ErrKind::Usage の追加と Internal の廃止

ユーザー入力起因のエラー (hex 不正、EOJ 桁数、CIDR 書式など) が stderr JSON で `"kind": "internal"` と出るのは誤解を招く。`Usage` (exit 1) を追加し、全 `Internal` 使用箇所 (すべてユーザー入力検証) を置き換える。置き換え後 `Internal` はどこからも構築されなくなるので variant ごと削除する (dead_code で clippy が落ちるため残せない)。

**Files:**
- Modify: `src/error.rs`
- Modify: `src/main.rs` (Internal 使用 7 箇所 + テスト追加)
- Modify: `src/commands.rs` (Internal 使用 4 箇所 + テスト追加)
- Modify: `CLAUDE.md` (kind の例に usage を追加)
- Modify: `README.md` (exit code 表の 1 の説明)

**Interfaces:**
- Consumes: 既存 `ErrKind` / `AppError` (src/error.rs)
- Produces: `ErrKind::Usage` (as_str() == "usage", exit_code() == 1)。Task 3 の `build_frame` ヘルパーがこれを使う。

- [ ] **Step 1: 失敗するテストを書く**

`src/error.rs` の末尾にテストモジュールを追加:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_kind_maps_to_exit_1() {
        assert_eq!(ErrKind::Usage.as_str(), "usage");
        assert_eq!(ErrKind::Usage.exit_code(), 1);
    }
}
```

`src/main.rs` のテストモジュールに追加:

```rust
    #[test]
    fn user_input_errors_are_usage_kind() {
        let aircon = Eoj::from_hex("013001").unwrap();
        assert_eq!(resolve_epc(aircon, "bogus").unwrap_err().kind, ErrKind::Usage);
        assert_eq!(parse_eoj("zz").unwrap_err().kind, ErrKind::Usage);
        assert_eq!(parse_esv("6201").unwrap_err().kind, ErrKind::Usage);
        assert_eq!(
            resolve_edt(Eoj::from_hex("026301").unwrap(), 0xE0, "bogus")
                .unwrap_err()
                .kind,
            ErrKind::Usage
        );
    }
```

`src/commands.rs` のテストモジュールに追加 (先頭の `use super::*;` で ErrKind は見えないので `crate::error::ErrKind` を使う):

```rust
    #[test]
    fn user_input_errors_are_usage_kind() {
        use crate::error::ErrKind;
        assert_eq!(resolve_cidr(None, None).unwrap_err().kind, ErrKind::Usage);
        assert_eq!(
            sweep_hosts(Some("nope/24"), None).unwrap_err().kind,
            ErrKind::Usage
        );
        assert_eq!(
            EojFilter::from_hex("02").unwrap_err().kind,
            ErrKind::Usage
        );
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test usage_kind 2>&1 | tail -20`
Expected: コンパイルエラー「no variant or associated item named `Usage` found for enum `ErrKind`」

- [ ] **Step 3: 実装**

`src/error.rs` の `ErrKind` を変更 — `Internal` を削除し `Usage` を追加:

```rust
/// エラー種別。stderr JSON の "kind" と exit code に対応。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    Timeout,        // exit 3
    DeviceRejected, // exit 4 (SNA)
    Network,        // exit 5
    Bind,           // exit 5
    Parse,          // exit 1
    Usage,          // exit 1 (enl 側で検出した入力不正。clap の exit 2 とは別)
}
```

`as_str` / `exit_code` の対応する arm を変更:

```rust
            ErrKind::Parse => "parse",
            ErrKind::Usage => "usage",
```

```rust
            ErrKind::Parse | ErrKind::Usage => 1,
```

`src/main.rs`: `ErrKind::Internal` を全箇所 (`parse_hex_byte` ×2、`parse_prop_arg`、`parse_eoj`、`resolve_epc`、`resolve_edt`、`Listen` arm の `--count は 1 以上`) `ErrKind::Usage` に置換。

`src/commands.rs`: `ErrKind::Internal` を全箇所 (`resolve_cidr` ×2、`EojFilter::from_hex` ×2) `ErrKind::Usage` に置換。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 全テスト PASS、clippy 指摘ゼロ (`Internal` が残っていると dead_code で落ちる)

- [ ] **Step 5: ドキュメント更新**

`CLAUDE.md` の「### stderr エラー」節:

```
- `kind` の例: `timeout` / `device_rejected` / `network` / `parse` / `bind` / `usage`。
```

`README.md` の exit code 表の最終行:

```
| 1 | invalid input detected by enl (`usage`), parse error, or other unexpected error |
```

- [ ] **Step 6: コミット**

```bash
git add src/error.rs src/main.rs src/commands.rs CLAUDE.md README.md
git commit -m "refactor: ユーザー入力エラーの kind を internal から usage に分離

exit code は 1 のまま。internal はどこからも構築されなくなったため削除。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: SchemaTarget を schema.rs へ移し網羅 match 化

`SchemaTarget::as_str()` (main.rs) と `for_target` の文字列 match (schema.rs) が手書き 2 表になっており、サブコマンド追加時に片方だけ更新すると `Some(_) | None => all()` フォールバックで黙って全スキーマが返る。enum を schema.rs に移して enum で match すればコンパイラが網羅性を強制する。

**Files:**
- Modify: `src/schema.rs` (enum 移設 + for_target 書き換え + テスト書き換え)
- Modify: `src/main.rs` (SchemaTarget 定義削除、schema::SchemaTarget 参照)

**Interfaces:**
- Consumes: なし (自己完結)
- Produces: `pub enum schema::SchemaTarget` (`Clone, Copy, ValueEnum` derive)、`pub fn schema::for_target(target: Option<SchemaTarget>) -> Value`

- [ ] **Step 1: 失敗するテストを書く**

`src/schema.rs` のテストモジュールで、既存の `all_covers_every_target` と `each_schema_is_valid_object_with_dialect` を enum ベースに書き換え、`none_returns_all` を維持する。`TARGETS` 定数 (schema.rs:11-13) は削除:

```rust
#[cfg(test)]
mod tests {
    use super::*; // Step 3 でモジュール先頭に足す `use clap::ValueEnum;` がここに効く

    #[test]
    fn all_covers_every_target() {
        let all = all();
        let obj = all.as_object().unwrap();
        let variants = SchemaTarget::value_variants();
        assert_eq!(obj.len(), variants.len());
        for t in variants {
            let name = t.to_possible_value().unwrap().get_name().to_string();
            assert!(obj.contains_key(&name), "{name} 欠落");
        }
    }

    #[test]
    fn each_schema_is_valid_object_with_dialect() {
        for t in SchemaTarget::value_variants() {
            let name = t.to_possible_value().unwrap().get_name().to_string();
            let s = for_target(Some(*t));
            assert_eq!(s["$schema"], DIALECT, "{name} に $schema 無し");
            assert_eq!(s["type"], "object", "{name} の type が object でない");
            assert!(s["properties"].is_object(), "{name} に properties 無し");
            assert!(s["required"].is_array(), "{name} に required 無し");
        }
    }

    #[test]
    fn none_returns_all() {
        assert_eq!(for_target(None), all());
    }

    // get_property_items_lossless は既存のまま残す
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test --lib 2>&1 | tail -20` (バイナリクレートなので `cargo test schema 2>&1 | tail -20`)
Expected: コンパイルエラー「cannot find type `SchemaTarget` in this scope」(schema.rs にまだ無い)

- [ ] **Step 3: 実装**

`src/schema.rs` 冒頭 (`use serde_json::...` の隣) に追加:

```rust
use clap::ValueEnum;
```

`TARGETS` 定数を削除し、代わりに enum と for_target を配置:

```rust
/// `enl schema` の対象サブコマンド。clap が未知値を弾く (exit 2)。
/// ここで enum を持ち match を網羅させることで、サブコマンド追加時の
/// スキーマ実装漏れをコンパイルエラーで検出する。
#[derive(Clone, Copy, ValueEnum)]
pub enum SchemaTarget {
    Discover,
    Get,
    Set,
    Describe,
    Raw,
    Listen,
}

/// 名前指定があればそのスキーマ、無ければ全サブコマンドのスキーマ集約。
pub fn for_target(target: Option<SchemaTarget>) -> Value {
    match target {
        None => all(),
        Some(SchemaTarget::Discover) => discover(),
        Some(SchemaTarget::Get) => get(),
        Some(SchemaTarget::Set) => set(),
        Some(SchemaTarget::Describe) => describe(),
        Some(SchemaTarget::Raw) => raw(),
        Some(SchemaTarget::Listen) => listen(),
    }
}
```

`src/main.rs`:
1. `SchemaTarget` enum 定義と `impl SchemaTarget` (main.rs:152-174) を丸ごと削除。
2. `use clap::{Parser, Subcommand, ValueEnum};` から `ValueEnum` を外す → `use clap::{Parser, Subcommand};`
3. `Command::Schema` の定義を変更:

```rust
    Schema {
        /// 対象サブコマンド。省略時は全サブコマンドのスキーマを 1 オブジェクトで出す。
        #[arg(value_enum)]
        target: Option<schema::SchemaTarget>,
    },
```

4. `run()` の arm を変更:

```rust
        Command::Schema { target } => Ok(schema::for_target(target)),
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 全テスト PASS。さらに CLI 挙動確認:

Run: `cargo run -q -- schema get | head -3` と `cargo run -q -- schema | python3 -c "import json,sys; print(sorted(json.load(sys.stdin).keys()))"`
Expected: 前者は get スキーマの JSON、後者は `['describe', 'discover', 'get', 'listen', 'raw', 'set']`

- [ ] **Step 5: コミット**

```bash
git add src/schema.rs src/main.rs
git commit -m "refactor: SchemaTarget を schema.rs に移し match を網羅化

main.rs の as_str と schema.rs の文字列 match の手書き 2 表を解消。
サブコマンド追加時のスキーマ実装漏れがコンパイルエラーで検出される。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: codec build の OPC/PDC 上限検証

`build_block` は `props.len() as u8` / `edt.len() as u8` で 256 以上を黙って wrap し、壊れたフレームを無言で送信しうる (CLI の hex EDT 入力で到達可能)。`build` を `Result` 化して上限超過を `CodecError::TooLong` で返す。**Task 1 完了後に実施** (`ErrKind::Usage` を使うため)。

**Files:**
- Modify: `src/codec.rs` (build の Result 化 + TooLong variant + テスト)
- Modify: `src/commands.rs` (呼び出し 4 箇所 + `build_frame` ヘルパー + テスト)

**Interfaces:**
- Consumes: `ErrKind::Usage` (Task 1)
- Produces: `pub fn codec::build(frame: &Frame) -> Result<Vec<u8>, CodecError>`、`CodecError::TooLong(&'static str)`、commands.rs 内部ヘルパー `fn build_frame(frame: &Frame) -> Result<Vec<u8>, AppError>`

- [ ] **Step 1: 失敗するテストを書く**

`src/codec.rs` のテストモジュールに追加:

```rust
    #[test]
    fn build_rejects_oversize_edt() {
        // PDC は u8。256 バイト EDT は wrap で壊れたフレームになるため拒否する。
        let frame = Frame::standard(
            0x0001,
            Eoj([0x05, 0xFF, 0x01]),
            Eoj([0x01, 0x30, 0x01]),
            Esv::SetC,
            vec![Property::new(0x80, vec![0u8; 256])],
        );
        assert!(matches!(build(&frame), Err(CodecError::TooLong(_))));
    }

    #[test]
    fn build_accepts_edt_at_255_limit() {
        let frame = Frame::standard(
            0x0001,
            Eoj([0x05, 0xFF, 0x01]),
            Eoj([0x01, 0x30, 0x01]),
            Esv::SetC,
            vec![Property::new(0x80, vec![0u8; 255])],
        );
        let built = build(&frame).unwrap();
        // EHD(2)+TID(2)+SEOJ(3)+DEOJ(3)+ESV(1)+OPC(1)+EPC(1) の次が PDC
        assert_eq!(built[13], 255);
        roundtrip(&built);
    }

    #[test]
    fn build_rejects_oversize_opc() {
        let many: Vec<Property> = (0..256).map(|_| Property::get(0x80)).collect();
        let frame = Frame::standard(
            0x0001,
            Eoj([0x05, 0xFF, 0x01]),
            Eoj([0x01, 0x30, 0x01]),
            Esv::Get,
            many,
        );
        assert!(matches!(build(&frame), Err(CodecError::TooLong(_))));
    }
```

`src/commands.rs` のテストモジュールに追加:

```rust
    #[test]
    fn build_frame_oversize_edt_is_usage() {
        use crate::error::ErrKind;
        let frame = Frame::standard(
            0x0001,
            CONTROLLER,
            NODE_PROFILE,
            Esv::SetC,
            vec![Property::new(0x80, vec![0u8; 256])],
        );
        assert_eq!(build_frame(&frame).unwrap_err().kind, ErrKind::Usage);
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test build_reject 2>&1 | tail -20`
Expected: コンパイルエラー (「no variant named `TooLong`」、`build` の戻りが `Vec<u8>` で `matches!(… Err(…))` が型不一致)

- [ ] **Step 3: 実装**

`src/codec.rs` — `CodecError` に variant を追加:

```rust
    /// PDC (EDT 長) や OPC (プロパティ数) が 1 バイト表現の上限 255 を超過。
    TooLong(&'static str),
```

`Display` に arm を追加:

```rust
            CodecError::TooLong(w) => write!(f, "サイズ上限超過: {w}"),
```

`build` / `build_block` を Result 化:

```rust
/// Frame をバイト列にシリアライズする。
/// PDC / OPC は 1 バイトのため、EDT 255 バイト超・プロパティ 255 個超は
/// wrap で壊れたフレームを黙って作らず TooLong で拒否する。
pub fn build(frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::with_capacity(16);
    out.push(frame.ehd1);
    out.push(frame.ehd2);
    out.extend_from_slice(&frame.tid.to_be_bytes());
    match &frame.edata {
        Edata::Arbitrary(bytes) => out.extend_from_slice(bytes),
        Edata::Standard {
            seoj,
            deoj,
            esv,
            props,
        } => {
            out.extend_from_slice(&seoj.0);
            out.extend_from_slice(&deoj.0);
            out.push(esv.to_u8());
            build_block(&mut out, props)?;
        }
        Edata::SetGet {
            seoj,
            deoj,
            esv,
            set_props,
            get_props,
        } => {
            out.extend_from_slice(&seoj.0);
            out.extend_from_slice(&deoj.0);
            out.push(esv.to_u8());
            build_block(&mut out, set_props)?;
            build_block(&mut out, get_props)?;
        }
    }
    Ok(out)
}

fn build_block(out: &mut Vec<u8>, props: &[Property]) -> Result<(), CodecError> {
    if props.len() > u8::MAX as usize {
        return Err(CodecError::TooLong("OPC (プロパティ数) は 255 以下"));
    }
    out.push(props.len() as u8);
    for p in props {
        if p.edt.len() > u8::MAX as usize {
            return Err(CodecError::TooLong("EDT は 255 バイト以下 (PDC 上限)"));
        }
        out.push(p.epc);
        out.push(p.pdc());
        out.extend_from_slice(&p.edt);
    }
    Ok(())
}
```

codec.rs の既存テストで `build(...)` を使う箇所に `.unwrap()` を付ける (対象: `roundtrip` ヘルパー内の `let rebuilt = build(&f1)`、`roundtrip_get_request`、`roundtrip_get_response_multi_prop`、`roundtrip_setget`、`parse_sna_rejection`、`unknown_esv_does_not_break`)。例:

```rust
        let rebuilt = build(&f1).expect("build");
```

`src/commands.rs` — ヘルパーを追加 (`parse_response` の近く):

```rust
/// Frame を build する。TooLong (EDT/プロパティ数の上限超過) は CLI の
/// hex 入力からのみ到達しうるため usage エラーにする。
fn build_frame(frame: &Frame) -> Result<Vec<u8>, AppError> {
    codec::build(frame)
        .map_err(|e| AppError::new(ErrKind::Usage, format!("フレーム構築失敗: {e}")))
}
```

呼び出し 4 箇所を更新:

1. `request` (旧 commands.rs:65):

```rust
    let dg = net::send_and_recv_one(&socket, dst, ip, tid, &build_frame(&frame)?, timeout)?;
```

2. `discover` (旧 commands.rs:99):

```rust
    let payload = build_frame(&frame)?;
```

3. `raw` (旧 commands.rs:352):

```rust
    let sent = build_frame(&frame)?;
```

4. `reply_infc_res` (旧 commands.rs:525) — best-effort を維持:

```rust
    let payload = match codec::build(&res) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%src, error = %e, "INFC_Res 構築失敗 (continue)");
            return;
        }
    };
    let dst = SocketAddr::new(src, net::ECHONET_PORT);
    if let Err(e) = socket.send_to(&payload, dst) {
        tracing::warn!(%src, error = %e, "INFC_Res 送信失敗 (continue)");
    }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 全テスト PASS

- [ ] **Step 5: コミット**

```bash
git add src/codec.rs src/commands.rs
git commit -m "fix: codec build で PDC/OPC の 255 超過を検出しフレーム破損を防止

256 バイト以上の EDT を CLI hex で渡すと u8 wrap で壊れたフレームを
無言送信していた。build を Result 化し TooLong (kind=usage) で拒否する。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: describe のマップエントリ JSON を properties.rs へ集約

commands.rs の describe 内にある「EPC hex + 辞書にあれば name + enum なら values」の組み立ては、`property_json` (properties.rs) と同系の辞書 JSON 表現。`properties::map_entry_json` として移し、辞書系の JSON 表現を properties.rs に一元化する。出力 JSON は不変。

**Files:**
- Modify: `src/properties.rs` (`map_entry_json` 追加 + テスト)
- Modify: `src/commands.rs` (describe のインライン組み立てを置換)

**Interfaces:**
- Consumes: 既存 `epc_name` / `epc_values` (properties.rs)
- Produces: `pub fn properties::map_entry_json(eoj: Eoj, epc: u8) -> Value`

- [ ] **Step 1: 失敗するテストを書く**

`src/properties.rs` のテストモジュールに追加:

```rust
    #[test]
    fn map_entry_json_shapes() {
        let shutter = Eoj([0x02, 0x63, 1]);
        // enum 型: name + values 併記
        let v = map_entry_json(shutter, 0xE0);
        assert_eq!(v["epc"], "E0");
        assert_eq!(v["name"], "open_close_operation");
        assert_eq!(v["values"]["42"], "close");
        // 数値型: name のみ (values 無し)
        let v = map_entry_json(shutter, 0xE1);
        assert_eq!(v["name"], "open_level");
        assert!(v.get("values").is_none());
        // 未知 EPC: epc のみ
        let v = map_entry_json(shutter, 0x77);
        assert_eq!(v["epc"], "77");
        assert!(v.get("name").is_none());
        assert!(v.get("values").is_none());
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test map_entry_json 2>&1 | tail -10`
Expected: コンパイルエラー「cannot find function `map_entry_json`」

- [ ] **Step 3: 実装**

`src/properties.rs` に追加 (`property_json` の直前):

```rust
/// describe のマップ 1 エントリ: EPC hex + 辞書にあれば name、enum 型なら values。
pub fn map_entry_json(eoj: Eoj, epc: u8) -> Value {
    let mut obj = json!({ "epc": format!("{epc:02X}") });
    if let Some(name) = epc_name(eoj, epc) {
        obj["name"] = json!(name);
    }
    if let Some(values) = epc_values(eoj, epc) {
        obj["values"] = values;
    }
    obj
}
```

`src/commands.rs` の `describe` 内のインライン組み立て (旧 commands.rs:311-326) を置換:

```rust
        match properties::parse_property_map(&p.edt) {
            Some(epcs) => {
                out[key] = json!(epcs
                    .iter()
                    .map(|&e| properties::map_entry_json(eoj, e))
                    .collect::<Vec<_>>());
            }
            // 壊れた / 空マップでも生 hex は残す。
            None => out[key] = json!({ "edt_hex": codec::bytes_to_hex(&p.edt) }),
        }
```

commands.rs 側で `properties::epc_name` / `properties::epc_values` の直接呼び出しが不要になる (他に使用箇所は無い)。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 全テスト PASS

- [ ] **Step 5: コミット**

```bash
git add src/properties.rs src/commands.rs
git commit -m "refactor: describe のマップエントリ JSON 組み立てを properties.rs へ集約

辞書駆動の JSON 表現 (name/values 付加) を property_json と同じ場所に置く。
出力 JSON は不変。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: listen フィルタを ListenFilter 構造体にまとめる

`listen` が 6 引数、`inf_event` が 5 引数で、うち `from` / `eoj_filter` / `epc` の 3 つは「採用フィルタ」という一つの概念。`ListenFilter` 構造体にまとめ、判定ロジックを `accepts` メソッドに寄せる。挙動は不変。

**Files:**
- Modify: `src/commands.rs` (`ListenFilter` 追加、`listen` / `inf_event` のシグネチャ変更、テスト書き換え)
- Modify: `src/main.rs` (`Listen` arm でフィルタを構築)

**Interfaces:**
- Consumes: 既存 `EojFilter` (commands.rs)
- Produces: `pub struct commands::ListenFilter { pub from: Option<IpAddr>, pub eoj: Option<EojFilter>, pub epc: Option<u8> }` (derive Default)、`pub fn commands::listen(iface: Option<Ipv4Addr>, count: usize, timeout: Option<Duration>, filter: ListenFilter) -> Result<Value, AppError>`

- [ ] **Step 1: 失敗するテストを書く**

`src/commands.rs` の既存 `inf_event_*` テスト 3 本を `ListenFilter` ベースに書き換える:

```rust
    #[test]
    fn inf_event_accepts_inf_and_decodes() {
        let src: IpAddr = "192.0.2.20".parse().unwrap();
        let ev = inf_event(src, &inf_frame(Esv::Inf), &ListenFilter::default()).unwrap();
        assert_eq!(ev["ip"], "192.0.2.20");
        assert_eq!(ev["tid"], "00ab");
        assert_eq!(ev["seoj"], "029101");
        assert_eq!(ev["esv"], "Inf");
        assert_eq!(ev["properties"][0]["epc"], "80");
        assert_eq!(ev["properties"][0]["edt_hex"], "31");
        // スーパークラス共通辞書 (power) でデコードされる
        assert_eq!(ev["properties"][0]["value"]["power"], "off");
    }

    #[test]
    fn inf_event_rejects_non_notification() {
        let src: IpAddr = "192.0.2.20".parse().unwrap();
        let f = ListenFilter::default();
        assert!(inf_event(src, &inf_frame(Esv::GetRes), &f).is_none());
        assert!(inf_event(src, &inf_frame(Esv::Get), &f).is_none());
    }

    #[test]
    fn inf_event_filters() {
        let src: IpAddr = "192.0.2.20".parse().unwrap();
        let other: IpAddr = "192.0.2.99".parse().unwrap();
        let f = inf_frame(Esv::Inf);
        let with = |filter: ListenFilter| inf_event(src, &f, &filter);
        // from フィルタ
        assert!(with(ListenFilter { from: Some(src), ..Default::default() }).is_some());
        assert!(with(ListenFilter { from: Some(other), ..Default::default() }).is_none());
        // eoj フィルタ (クラス / 完全一致 / 不一致)
        let class = EojFilter::from_hex("0291").unwrap();
        let miss = EojFilter::from_hex("013001").unwrap();
        assert!(with(ListenFilter { eoj: Some(class), ..Default::default() }).is_some());
        assert!(with(ListenFilter { eoj: Some(miss), ..Default::default() }).is_none());
        // epc フィルタ
        assert!(with(ListenFilter { epc: Some(0x80), ..Default::default() }).is_some());
        assert!(with(ListenFilter { epc: Some(0xB0), ..Default::default() }).is_none());
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test inf_event 2>&1 | tail -10`
Expected: コンパイルエラー「cannot find struct `ListenFilter`」

- [ ] **Step 3: 実装**

`src/commands.rs` — `EojFilter` の後に追加:

```rust
/// listen の採用フィルタ (--from / --eoj / --epc)。全 None なら全通知を採用。
#[derive(Default)]
pub struct ListenFilter {
    pub from: Option<IpAddr>,
    pub eoj: Option<EojFilter>,
    pub epc: Option<u8>,
}

impl ListenFilter {
    /// 通知 (src から SEOJ=seoj, プロパティ=props) がフィルタを通過するか。
    fn accepts(&self, src: IpAddr, seoj: Eoj, props: &[Property]) -> bool {
        if self.from.is_some_and(|ip| ip != src) {
            return false;
        }
        if self.eoj.as_ref().is_some_and(|f| !f.matches(seoj)) {
            return false;
        }
        if self.epc.is_some_and(|e| !props.iter().any(|p| p.epc == e)) {
            return false;
        }
        true
    }
}
```

`listen` のシグネチャと `inf_event` 呼び出しを変更:

```rust
pub fn listen(
    iface: Option<Ipv4Addr>,
    count: usize,
    timeout: Option<Duration>,
    filter: ListenFilter,
) -> Result<Value, AppError> {
```

ループ内:

```rust
        match inf_event(dg.from.ip(), &frame, &filter) {
```

`inf_event` を変更 (フィルタ判定 3 連 if を `accepts` に置換):

```rust
/// 受信フレームが採用すべき INF / INFC 通知なら event JSON にする。
/// 非通知 ESV・フィルタ不一致は None。
fn inf_event(src: IpAddr, frame: &Frame, filter: &ListenFilter) -> Option<Value> {
    let (seoj, deoj, esv, props) = match &frame.edata {
        Edata::Standard {
            seoj,
            deoj,
            esv,
            props,
        } => (*seoj, *deoj, *esv, props),
        _ => return None,
    };
    if !matches!(esv, Esv::Inf | Esv::InfC) {
        return None;
    }
    if !filter.accepts(src, seoj, props) {
        return None;
    }
    Some(json!({
        "ip": src.to_string(),
        "tid": format!("{:04x}", frame.tid),
        "seoj": seoj.to_hex(),
        "deoj": deoj.to_hex(),
        "esv": esv.name(),
        "properties": props_json(seoj, props),
    }))
}
```

`src/main.rs` の `Listen` arm を変更:

```rust
        Command::Listen {
            count,
            timeout_ms,
            from,
            eoj,
            epc,
        } => {
            if count == 0 {
                return Err(AppError::new(ErrKind::Usage, "--count は 1 以上"));
            }
            let filter = commands::ListenFilter {
                from,
                eoj: eoj
                    .as_deref()
                    .map(commands::EojFilter::from_hex)
                    .transpose()?,
                epc: epc.as_deref().map(parse_epc_one).transpose()?,
            };
            // 0 は無期限 (count 件集まるまで待つ)。
            let timeout = (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms));
            commands::listen(iface, count, timeout, filter)
        }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 全テスト PASS

- [ ] **Step 5: コミット**

```bash
git add src/commands.rs src/main.rs
git commit -m "refactor: listen の採用フィルタを ListenFilter 構造体に集約

from / eoj / epc の 3 引数を 1 概念にまとめ、判定を accepts に寄せる。
挙動は不変。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: バージョン bump と最終確認

stderr の `kind` 契約変更 (`internal` → `usage`) と build 検証の追加は外部から観測可能な変更のため、リポジトリ慣行 (1.2.1 で bind リトライを bump) に合わせて minor bump する。

**Files:**
- Modify: `Cargo.toml` (version 1.2.1 → 1.3.0)
- Modify: `Cargo.lock` (cargo build で自動更新)

**Interfaces:**
- Consumes: Task 1-5 完了済みであること
- Produces: なし (リリース準備)

- [ ] **Step 1: バージョン変更**

`Cargo.toml`:

```toml
version = "1.3.0"
```

- [ ] **Step 2: Cargo.lock 更新と全体確認**

Run: `cargo build && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: ビルド成功 (Cargo.lock の enl バージョンが 1.3.0 に更新される)、fmt 差分なし、clippy ゼロ、全テスト PASS

Run: `cargo run -q -- schema get | head -3`
Expected: get スキーマの JSON が stdout に出る (装飾なし)

Run: `./target/debug/enl get 192.0.2.1 zz 80 2>&1 >/dev/null; echo "exit=$?"`
Expected: stderr に `{"error":{"kind":"usage","detail":"EOJ 不正: ..."}}`、`exit=1`

- [ ] **Step 3: コミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.3.0 に bump (usage kind 追加と build 長さ検証)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
