# set --nowait (SetI fire-and-forget) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `enl set` に `--nowait` を追加し、SetI (0x60, 応答不要) をエフェメラルポートから送信のみ行うことで、listen の 3610 専有と共存できるようにする。

**Architecture:** 既存の `request()` 経路（3610 バインド + 送受信）には触れず、`commands::set_nowait()` という送信専用の並行経路を追加する。ソケットは `net::open_ephemeral_socket()`（`0.0.0.0:0`、リトライ不要）。応答を一切待たないため exit 3 / 4 は発生せず、送信成功で exit 0。

**Tech Stack:** Rust (std のみ、依存ゼロ codec)、clap derive、serde_json。

**Spec:** `docs/superpowers/specs/2026-07-16-set-nowait-design.md`

## Global Constraints

- 依存 crate を追加しない（`std::net::UdpSocket` のみ。socket2 / tokio 禁止）。
- stdout は純粋な JSON のみ。診断は `tracing` で stderr。
- 既存の set（SetC + 応答待ち）の動作・出力スキーマは一切変えない。
- コミット/テスト/ドキュメントに自宅の実機 IP・MAC を書かない（例示は RFC 5737 の `192.0.2.0/24`）。
- 各タスク完了時に `cargo test` と `cargo clippy -- -D warnings` が通ること。
- コメント・ログ・ヘルプ文は既存コードに合わせ日本語。

---

### Task 1: net::open_ephemeral_socket()

**Files:**
- Modify: `src/net.rs` (`open_socket()` の直後、44 行目付近に追加)
- Test: `src/net.rs` 内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 既存の `AppError::new(ErrKind::Bind, ...)`、`ECHONET_PORT` 定数
- Produces: `pub fn open_ephemeral_socket() -> Result<UdpSocket, AppError>`（Task 2 が使う）

- [ ] **Step 1: 失敗するテストを書く**

`src/net.rs` の tests モジュール末尾に追加:

```rust
    #[test]
    fn ephemeral_socket_binds_off_3610() {
        let s = open_ephemeral_socket().unwrap();
        let port = s.local_addr().unwrap().port();
        assert_ne!(port, 0);
        assert_ne!(port, ECHONET_PORT);
    }
```

- [ ] **Step 2: テストが失敗する（コンパイルエラー）ことを確認**

Run: `cargo test ephemeral_socket_binds_off_3610`
Expected: FAIL — `cannot find function open_ephemeral_socket`

- [ ] **Step 3: 実装**

`src/net.rs` の `open_socket()` 直後に追加:

```rust
/// 送信専用のエフェメラルポートソケットを開く (set --nowait 用)。
/// 3610 を専有しないため listen と共存できる。実機検証 (2026-07-16) で
/// 機器の応答は要求の送信元ポートでなく常に 3610 固定宛てに返ることを
/// 確認済みのため、このソケットで応答は受信できない (受信しない前提で使う)。
/// エフェメラルは AddrInUse が起き得ないためリトライしない。
pub fn open_ephemeral_socket() -> Result<UdpSocket, AppError> {
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
    UdpSocket::bind(addr)
        .map_err(|e| AppError::new(ErrKind::Bind, format!("{addr} へのバインド失敗: {e}")))
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test ephemeral_socket_binds_off_3610`
Expected: PASS

- [ ] **Step 5: 全テスト + clippy**

Run: `cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS / warning なし

- [ ] **Step 6: Commit**

```bash
git add src/net.rs
git commit -m "feat(net): 送信専用エフェメラルソケット open_ephemeral_socket を追加"
```

---

### Task 2: commands::set_nowait() + codec SetI ラウンドトリップ

**Files:**
- Modify: `src/codec.rs` (tests モジュールに SetI ラウンドトリップテスト追加のみ、実装変更なし)
- Modify: `src/commands.rs` (`set()` の直後、285 行目付近に `set_nowait()` を追加)
- Test: 各ファイル内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 1 の `net::open_ephemeral_socket()`。既存の `next_tid()` / `dst_for(ip, multicast)` / `transport_name()` / `build_frame()` / `props_json()` / `CONTROLLER` / `Frame::standard()` / `Esv::SetI` / `Property::new()`
- Produces: `pub fn set_nowait(ip: IpAddr, eoj: Eoj, epc: u8, edt: Vec<u8>, multicast: bool) -> Result<Value, AppError>`（Task 3 が使う）

- [ ] **Step 1: codec に SetI ラウンドトリップテストを書く**

ESV 0x60 の u8 変換は `esv_u8_roundtrip_all_values` で網羅済みだが、SetI フレーム全体の parse→build→parse は未カバー。`src/codec.rs` tests の `roundtrip_get_request` の後に追加:

```rust
    #[test]
    fn roundtrip_seti_request() {
        // SetI (0x60, 応答不要): set --nowait が送るフレーム。
        let buf = build(&Frame::standard(
            0x002A,
            Eoj([0x05, 0xFF, 0x01]),
            Eoj([0x02, 0x91, 0x01]),
            Esv::SetI,
            vec![Property::new(0x80, vec![0x30])],
        ))
        .unwrap();
        assert_eq!(
            buf,
            [0x10, 0x81, 0x00, 0x2A, 0x05, 0xFF, 0x01, 0x02, 0x91, 0x01, 0x60, 0x01, 0x80, 0x01, 0x30]
        );
        roundtrip(&buf);
    }
```

Run: `cargo test roundtrip_seti_request`
Expected: PASS（codec は ESV 非依存なので最初から通る。カバレッジ固定が目的）

- [ ] **Step 2: set_nowait の失敗するテストを書く**

`src/commands.rs` の tests モジュール末尾に追加:

```rust
    #[test]
    fn set_nowait_sends_seti_from_ephemeral_and_returns_sent() {
        use std::net::UdpSocket;
        use std::time::Duration;
        // 127.0.0.1:3610 で機器役として受ける。ローカルで 3610 が使用中
        // (enl listen 等) だとこのテストは bind に失敗する。
        let dev = UdpSocket::bind("127.0.0.1:3610").expect("127.0.0.1:3610 が使用中");
        let eoj = Eoj::from_hex("029101").unwrap();

        let out = set_nowait("127.0.0.1".parse().unwrap(), eoj, 0x80, vec![0x30], false).unwrap();
        assert_eq!(out["result"], "sent");
        assert_eq!(out["esv"], "SetI");
        assert_eq!(out["ip"], "127.0.0.1");
        assert_eq!(out["eoj"], "029101");
        assert_eq!(out["properties"][0]["edt_hex"], "30");

        // set_nowait は既に return 済み = 応答を待っていない。フレーム内容を検証。
        dev.set_read_timeout(Some(Duration::from_millis(2000))).unwrap();
        let mut buf = [0u8; 1500];
        let (n, from) = dev.recv_from(&mut buf).unwrap();
        // EDATA: ESV=0x60 (SetI), OPC=1, EPC=0x80, PDC=1, EDT=0x30
        assert_eq!(&buf[10..n], &[0x60, 0x01, 0x80, 0x01, 0x30]);
        // 送信元がエフェメラルポート (3610 ではない) であること
        assert_ne!(from.port(), 3610);
    }
```

- [ ] **Step 3: テストが失敗する（コンパイルエラー）ことを確認**

Run: `cargo test set_nowait_sends_seti`
Expected: FAIL — `cannot find function set_nowait`

- [ ] **Step 4: 実装**

`src/commands.rs` の `set()` の直後に追加:

```rust
/// IP / EOJ / EPC / EDT を指定して Set (SetI = 応答不要, fire-and-forget)。
///
/// エフェメラルポートから送信のみ行い 3610 をバインドしないため、listen の
/// 3610 専有と共存できる。機器の応答は 3610 固定宛てに返る (実機検証 2026-07-16)
/// ため応答は待たず、機器リジェクト (SetI_SNA) も検知できない。exit 0 は
/// 「送信できた」ことしか意味しない。実行確認は listen の INF か後続 get に委ねる。
pub fn set_nowait(
    ip: IpAddr,
    eoj: Eoj,
    epc: u8,
    edt: Vec<u8>,
    multicast: bool,
) -> Result<Value, AppError> {
    tracing::info!(%ip, eoj = eoj.to_hex(), epc = format!("{epc:02X}"), transport = transport_name(multicast), "set (SetI, nowait) 送信");

    let props = vec![Property::new(epc, edt)];
    let socket = net::open_ephemeral_socket()?;
    let tid = next_tid();
    let frame = Frame::standard(tid, CONTROLLER, eoj, Esv::SetI, props.clone());
    socket
        .send_to(&build_frame(&frame)?, dst_for(ip, multicast))
        .map_err(|e| AppError::new(ErrKind::Network, format!("送信失敗: {e}")))?;

    Ok(json!({
        "ip": ip.to_string(),
        "eoj": eoj.to_hex(),
        "esv": Esv::SetI.name(),
        "result": "sent",
        "properties": props_json(eoj, &props),
    }))
}
```

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test set_nowait_sends_seti && cargo test roundtrip_seti_request`
Expected: PASS

- [ ] **Step 6: 全テスト + clippy**

Run: `cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS / warning なし

- [ ] **Step 7: Commit**

```bash
git add src/codec.rs src/commands.rs
git commit -m "feat(commands): SetI fire-and-forget の set_nowait を追加"
```

---

### Task 3: CLI --nowait フラグ (main.rs)

**Files:**
- Modify: `src/main.rs:71-85` (`Command::Set` バリアント) と `src/main.rs:190-209` (dispatch)

**Interfaces:**
- Consumes: Task 2 の `commands::set_nowait(ip, eoj, epc, edt, multicast)`
- Produces: `enl set <ip> <eoj> <epc> <edt> --nowait [--multicast]` の CLI

- [ ] **Step 1: Set バリアントに nowait フラグを追加**

`src/main.rs` の `Command::Set` を次のとおり変更（`multicast` の後にフィールド追加）:

```rust
    /// 指定機器のプロパティを設定 (SetC)。
    Set {
        ip: IpAddr,
        /// 対象 EOJ (6 hex 桁)。
        eoj: String,
        /// EPC (2 hex 桁)。
        epc: String,
        /// 設定値 EDT (hex, 例 30)。
        edt: String,
        /// 送信先を 224.0.23.0 (multicast) に切り替える。multicast にしか
        /// 応答しない機器向け。応答は ip から受ける。
        #[arg(long)]
        multicast: bool,
        /// SetI (0x60, 応答不要) で送信し応答を待たない。エフェメラルポート
        /// から送るため listen の 3610 専有と共存できる。機器リジェクト
        /// (SetI_SNA) は検知不能で、exit 0 は送信成功のみを意味する。
        /// --timeout-ms は使われない。
        #[arg(long)]
        nowait: bool,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
```

- [ ] **Step 2: dispatch を分岐**

`run()` の `Command::Set` アームを次のとおり変更:

```rust
        Command::Set {
            ip,
            eoj,
            epc,
            edt,
            multicast,
            nowait,
            timeout_ms,
        } => {
            let eoj = parse_eoj(&eoj)?;
            let epc = resolve_epc(eoj, &epc)?;
            let edt = resolve_edt(eoj, epc, &edt)?;
            if nowait {
                commands::set_nowait(ip, eoj, epc, edt, multicast)
            } else {
                commands::set(
                    ip,
                    eoj,
                    epc,
                    edt,
                    Duration::from_millis(timeout_ms),
                    multicast,
                )
            }
        }
```

- [ ] **Step 3: ビルドと動作確認**

Run: `cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS / warning なし

Run: `cargo run -q -- set 192.0.2.99 029101 80 30 --nowait; echo "exit=$?"`
Expected: 即座に `{"eoj":"029101","esv":"SetI","ip":"192.0.2.99","properties":[...],"result":"sent"}` + `exit=0`（宛先が実在しなくても UDP 送信は成功する = fire-and-forget の期待動作。properties には epc/name/pdc/edt_hex/value が入る）

Run: `cargo run -q -- set --help`
Expected: `--nowait` の説明が表示される

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): set に --nowait フラグを追加"
```

---

### Task 4: schema コマンドの set スキーマ更新

**Files:**
- Modify: `src/schema.rs:113-129` (`fn set()`)
- Test: `src/schema.rs` 内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: なし（スキーマ定義の JSON リテラルのみ）
- Produces: `enl schema set` の出力に `result: "sent"` が含まれる安定契約

- [ ] **Step 1: 失敗するテストを書く**

`src/schema.rs` の tests モジュール末尾に追加:

```rust
    #[test]
    fn set_result_covers_accepted_and_sent() {
        // set の result は SetC 応答確認 (accepted) と --nowait 送信のみ (sent) の 2 値。
        let result = &set()["properties"]["result"];
        let vals: Vec<&str> = result["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(vals, vec!["accepted", "sent"]);
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test set_result_covers_accepted_and_sent`
Expected: FAIL — 現状は `"const": "accepted"` で `enum` が無い

- [ ] **Step 3: スキーマを更新**

`src/schema.rs` の `fn set()` を次のとおり変更:

```rust
/// `enl set` の出力スキーマ。
fn set() -> Value {
    json!({
        "$schema": DIALECT,
        "title": "enl set output",
        "type": "object",
        "properties": {
            "ip": { "type": "string" },
            "eoj": { "type": "string", "description": "EOJ (6 hex 桁)" },
            "esv": { "type": "string", "description": "ESV 名 (SetC 応答時 SetRes / --nowait 時 SetI)" },
            "result": {
                "type": "string",
                "enum": ["accepted", "sent"],
                "description": "accepted=機器が受理を確認 (SetC) / sent=送信のみで実行未確認 (--nowait)"
            },
            "properties": { "type": "array", "items": property() }
        },
        "required": ["ip", "eoj", "esv", "result", "properties"],
        "additionalProperties": false
    })
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test set_result_covers_accepted_and_sent && cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS / warning なし

- [ ] **Step 5: Commit**

```bash
git add src/schema.rs
git commit -m "feat(schema): set 出力の result に sent (--nowait) を追加"
```

---

### Task 5: ドキュメント更新 + 1.4.0 bump

**Files:**
- Modify: `CLAUDE.md`（「⚠️ 最重要の落とし穴: ポート 3610 専有」節）
- Modify: `README.md`（Commands の set 行、`--multicast` 段落付近）
- Modify: `Cargo.toml` / `Cargo.lock`（version 1.3.0 → 1.4.0）

**Interfaces:** なし（ドキュメントのみ）

- [ ] **Step 1: CLAUDE.md の 3610 節に実機検証結果と --nowait を追記**

「⚠️ 最重要の落とし穴: ポート 3610 専有」節の箇条書き末尾に追加:

```markdown
- **応答先が 3610 固定であることは実機検証済み (2026-07-16)**: エフェメラル送信元ポートで unicast Get を送ると全機器が無応答になり、tcpdump では応答が常に送信元ポートでなく 3610 宛てに返ることを確認した。「送信系をエフェメラル化して応答も待つ」設計は成立しない（応答は 3610 を握るプロセスに吸われる）。
- listen が 3610 を専有している間に機器を操作したい場合は `set --nowait` を使う。SetI (0x60, 応答不要) をエフェメラルポートから送信のみ行うため 3610 に触れない。機器リジェクト (SetI_SNA) は検知できず、exit 0 は送信成功のみを意味する。実行確認は listen が受ける INF か後続の get に委ねる（操作と観測の分離）。
```

- [ ] **Step 2: README.md を更新**

Commands セクションの set 行（118 行目付近）を差し替え:

```markdown
- `set <ip> <eoj> <epc> <edt> [--nowait] [--multicast] [--timeout-ms 2000]` — `{"ip","eoj","esv","result":"accepted"|"sent","properties":[...]}`
```

`--multicast` の説明段落（122 行目付近）の直後に段落を追加:

```markdown
`--nowait` sends SetI (0x60, no response requested) from an ephemeral port and exits 0 as soon as the datagram is sent, without binding port 3610 — so it coexists with a running `listen`. The trade-off: device rejections (SetI_SNA) are undetectable, because devices reply to port 3610 regardless of the request's source port (verified with real devices). `"result":"sent"` means "sent", not "executed" — confirm via the INF that `listen` receives, or a follow-up `get`.
```

- [ ] **Step 3: バージョン bump**

`Cargo.toml` の `version = "1.3.0"` を `version = "1.4.0"` に変更し、`cargo build` で `Cargo.lock` を更新。

Run: `cargo build && cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS / warning なし

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md Cargo.toml Cargo.lock
git commit -m "docs: set --nowait と 3610 応答先固定の実機検証結果を反映、1.4.0 に bump"
```

---

### Task 6: 実機受け入れ検証 (jarvis)

**Files:** なし（検証のみ。リポジトリに実機 IP を書かないこと）

**Interfaces:**
- Consumes: Task 1-5 の完成バイナリ
- Produces: 受け入れ判定（合格で完了。不合格なら原因調査に戻る）

実機 IP・ホスト名・対象機器はプロジェクトメモリ（自宅LAN ECHONET機器マップ）を参照。以下 `<jarvis>` = LAN 直結検証ホスト、`<light-ip>` = リンクプラス照明アダプタの IP、`029101` = 一般照明インスタンス。**WSL2 内からの UDP 検証は不可**（既知の罠）。

- [ ] **Step 1: aarch64 バイナリをビルドして jarvis へ配置**

```bash
cross build --release --target aarch64-unknown-linux-musl
scp target/aarch64-unknown-linux-musl/release/enl <jarvis>:~/bin/enl-nowait-test
```

（jarvis の /tmp は noexec のため ~/bin に置く。/usr/local/bin/enl はまだ触らない）

- [ ] **Step 2: listen が 3610 を専有中であることを確認**

```bash
ssh <jarvis> 'ss -ulpn | grep 3610'
```

Expected: `enl listen` が 0.0.0.0:3610 を保持（mando が起動する無期限 listen）。いなければ `~/bin/enl-nowait-test listen --count 1 --timeout-ms 0 &` で意図的に専有して再現する。

- [ ] **Step 3: 3610 専有中に set --nowait が exit 0 になることを確認**

現在の照明状態を控えたうえで（ユーザーに確認 or Web UI）、電源をトグル:

```bash
ssh <jarvis> '~/bin/enl-nowait-test set <light-ip> 029101 power on --nowait; echo exit=$?'
```

Expected: 即座に `{"result":"sent","esv":"SetI",...}` + `exit=0`。従来の set が同条件で exit 5 になることも対照確認:

```bash
ssh <jarvis> '~/bin/enl-nowait-test set <light-ip> 029101 power on 2>/dev/null; echo exit=$?'
```

Expected: `exit=5`（bind リトライ枯渇）

- [ ] **Step 4: 機器が実際に実行したことを確認**

リンクプラスは状変で INF をマルチキャスト送出する（実機確認済み）。tcpdump で観測:

```bash
ssh <jarvis> 'sudo timeout 10 tcpdump -i eth0 -n -q "udp and host <light-ip>" &
sleep 2; ~/bin/enl-nowait-test set <light-ip> 029101 power off --nowait; wait'
```

Expected: 送信フレーム（エフェメラル送信元 → `<light-ip>.3610`）と、その後の INF（`<light-ip>` → 224.0.23.0:3610 ないし 3610 宛て）が見える。照明の実際の点灯/消灯もユーザーに確認してもらう。

- [ ] **Step 5: 照明を元の状態に戻す**

Step 3 で控えた状態に `set ... --nowait` で復元する。

- [ ] **Step 6: 合格したらユーザーに報告し、/usr/local/bin/enl への配備可否を確認**

配備はユーザー承認後: `ssh <jarvis> 'sudo cp ~/bin/enl-nowait-test /usr/local/bin/enl'`
