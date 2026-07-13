# 3610 bind リトライ Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `0.0.0.0:3610` の bind が `EADDRINUSE` のときだけ短い間隔でリトライし、one-shot 同士の瞬間的なポート衝突（本 CLI を定期実行する常駐プロセスと手動実行の重なり等）を吸収する。

**Architecture:** `src/net.rs` の `open_socket()` を、テスト可能な内部関数 `bind_with_retry(addr, window, interval)` に委譲する形に変える。リトライは `io::ErrorKind::AddrInUse` 限定・30ms 間隔・合計 2000ms 窓。窓を使い切ったら従来どおり `ErrKind::Bind`（exit 5）で失敗し、detail で「リトライしても解放されず」と恒常専有を示す。他のエラー種別は即失敗。

**Tech Stack:** Rust std のみ（依存追加なし）。`std::net::UdpSocket` + `std::thread::sleep`。

## Global Constraints

- 依存 crate を追加しない（プロジェクト方針: 依存ゼロ codec / tokio なし）。
- stdout の JSON スキーマ・exit code 体系は不変（bind 失敗は exit 5 のまま）。
- 診断は `tracing` で stderr へ。stdout に混ぜない。
- テストで実ポート 3610 を bind しない（CI / 並列テストで衝突するため）。ループバック + エフェメラルポートを使う。
- 実機 IP をコード・テスト・ドキュメントに書かない。サンプルは `192.0.2.0/24`。
- `cargo clippy -- -D warnings` が通ること。

### 背景（なぜこの修正か）

実機検証ホストでは本 CLI を約 5 秒間隔で定期実行する常駐プロセスが動いており、その one-shot（bind 時間は数十 ms × 数機器）と手動実行が重なると、後から起動した側が `Address already in use (os error 98)` → exit 5 で落ちる（2026-07-13 に並列起動で確定再現）。`SO_REUSEPORT` での共存は応答データグラムが他プロセスに吸われるため採用不可（CLAUDE.md 記載）。よって「短時間の専有は待つ、恒常専有は従来どおり即座に近い形で exit 5」というリトライを bind に入れる。

---

### Task 1: `bind_with_retry` の実装（TDD）

**Files:**
- Modify: `src/net.rs:30-41`（`open_socket()`）と同ファイル末尾の `mod tests`
- Test: `src/net.rs` 内の `mod tests`（このプロジェクトはユニットテストを同一ファイルに置く規約）

**Interfaces:**
- Consumes: 既存の `AppError::new(ErrKind::Bind, ...)`（`src/error.rs`）、既存 `ECHONET_PORT: u16 = 3610`
- Produces: `pub fn open_socket() -> Result<UdpSocket, AppError>`（シグネチャ不変。呼び出し側 `src/commands.rs` は無変更）。内部に `fn bind_with_retry(addr: SocketAddrV4, window: Duration, interval: Duration) -> Result<UdpSocket, AppError>`

- [ ] **Step 1: 失敗するテストを書く**

`src/net.rs` の `mod tests` に追記（`use super::*;` 済みなので追加 import は不要。`SocketAddr` は既に `use` されている）:

```rust
    #[test]
    fn bind_retry_succeeds_after_release() {
        let holder = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = match holder.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        // 100ms 後に holder がポートを解放する = 他の one-shot の専有が終わる状況
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });
        let sock = bind_with_retry(
            addr,
            Duration::from_millis(2000),
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(sock.local_addr().unwrap(), SocketAddr::V4(addr));
        t.join().unwrap();
    }

    #[test]
    fn bind_retry_times_out_when_never_released() {
        let _holder = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = match _holder.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        let start = Instant::now();
        let err = bind_with_retry(
            addr,
            Duration::from_millis(120),
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert_eq!(err.kind, crate::error::ErrKind::Bind);
        assert!(err.detail.contains("解放されず"), "detail={}", err.detail);
        // 窓いっぱいまでは粘る
        assert!(start.elapsed() >= Duration::from_millis(120));
    }

    #[test]
    fn bind_fails_immediately_on_non_addrinuse() {
        // 192.0.2.1 (RFC 5737) はローカルに存在しないアドレス
        // → EADDRNOTAVAIL であり AddrInUse ではないので即失敗すること
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 39610);
        let start = Instant::now();
        let err = bind_with_retry(
            addr,
            Duration::from_millis(2000),
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert_eq!(err.kind, crate::error::ErrKind::Bind);
        assert!(!err.detail.contains("解放されず"), "detail={}", err.detail);
        assert!(start.elapsed() < Duration::from_millis(500));
    }
```

- [ ] **Step 2: テストが失敗する（コンパイルエラーになる）ことを確認**

Run: `cargo test bind_`
Expected: `bind_with_retry` 未定義のコンパイルエラーで FAIL。

- [ ] **Step 3: 最小実装**

`src/net.rs` の `open_socket()`（現 30-41 行）を以下で置き換える:

```rust
/// bind リトライの間隔と最大待ち時間。本 CLI を定期実行する別プロセス
/// (cron / 常駐アプリの one-shot 呼び出し) との瞬間的な 3610 衝突を吸収する。
/// 恒常専有 (HA 等) は窓を使い切って従来どおり exit 5 になる。
/// 窓 2000ms の根拠: 相手側 one-shot が応答タイムアウト (既定 2000ms) いっぱい
/// 専有し続ける最悪ケースを 1 回分は跨げる長さ。
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(30);
const BIND_RETRY_WINDOW: Duration = Duration::from_millis(2000);

/// 3610 を専有する UDP ソケットを開く。AddrInUse は BIND_RETRY_WINDOW まで
/// リトライし、それ以外のバインド失敗は即 bind エラー (exit 5)。
pub fn open_socket() -> Result<UdpSocket, AppError> {
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, ECHONET_PORT);
    bind_with_retry(bind_addr, BIND_RETRY_WINDOW, BIND_RETRY_INTERVAL)
}

/// addr への bind を AddrInUse に限り interval 間隔で window までリトライする。
/// one-shot 同士の瞬間衝突 (数十 ms) を吸収するのが目的で、AddrInUse 以外の
/// エラー (権限・アドレス不在等) はリトライしても直らないため即失敗させる。
fn bind_with_retry(
    addr: SocketAddrV4,
    window: Duration,
    interval: Duration,
) -> Result<UdpSocket, AppError> {
    let deadline = Instant::now() + window;
    let mut waited = false;
    loop {
        match UdpSocket::bind(addr) {
            Ok(s) => {
                if waited {
                    tracing::info!(%addr, "解放を確認、バインド成功");
                }
                return Ok(s);
            }
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                if Instant::now() >= deadline {
                    return Err(AppError::new(
                        ErrKind::Bind,
                        format!(
                            "{addr} へのバインド失敗: {e}。{}ms 再試行しても解放されず。HA 等の常駐コントローラが専有していないか確認",
                            window.as_millis()
                        ),
                    ));
                }
                if !waited {
                    // 対話利用時に無言で待たないよう、待ち始めに 1 回だけ知らせる
                    tracing::info!(
                        %addr,
                        "使用中 (他の one-shot と衝突の可能性)。最大 {}ms 解放を待つ",
                        window.as_millis()
                    );
                    waited = true;
                }
                std::thread::sleep(interval);
            }
            Err(e) => {
                return Err(AppError::new(
                    ErrKind::Bind,
                    format!("{addr} へのバインド失敗: {e}"),
                ));
            }
        }
    }
}
```

注意: 既存メッセージ「HA 等が 3610 を専有していないか確認」は AddrInUse 枯渇時のメッセージに引き継がれる。非 AddrInUse（権限エラー等）に HA のヒントを付けるのは誤誘導なので外す。

- [ ] **Step 4: 全テスト + clippy が通ることを確認**

Run: `cargo test && cargo clippy -- -D warnings`
Expected: 既存テスト含め全 PASS、clippy 警告ゼロ。

- [ ] **Step 5: Commit**

```bash
git add src/net.rs
git commit -m "fix: 3610 bind の瞬間衝突を AddrInUse 限定リトライで吸収

one-shot 同士 (定期実行の別プロセスと手動実行) が重なると後発が
EADDRINUSE で exit 5 になっていた。30ms 間隔・最大 2000ms で
リトライし、恒常専有は従来どおり exit 5 のまま detail で区別する。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: ドキュメント更新 + バージョン 1.2.1

**Files:**
- Modify: `CLAUDE.md`（「⚠️ 最重要の落とし穴: ポート 3610 専有」節の最終箇条書き、現 63 行目）
- Modify: `README.md:44` の警告ブロック、`README.md:124` の `src/net.rs` 説明
- Modify: `Cargo.toml:3`（version）、`Cargo.lock`（ビルドで追従）

**Interfaces:**
- Consumes: Task 1 の実装値（30ms 間隔 / 2000ms 窓 / AddrInUse 限定）。数値をドキュメントに書くのでズレないこと。
- Produces: なし（ドキュメントのみ）。

- [ ] **Step 1: CLAUDE.md の落とし穴節を更新**

現在の行:

```
- バインド失敗は専用 exit code で即座に切り分けられること。
```

を以下に置き換える:

```
- バインド失敗は専用 exit code (5) で切り分けられること。`EADDRINUSE` に限り 30ms 間隔・最大 2000ms でリトライしてから失敗する（本 CLI を定期実行する別プロセスの one-shot と手動実行の瞬間衝突を吸収するため）。恒常専有（HA 等）はリトライ枯渇後に exit 5 となり、stderr の detail で「再試行しても解放されず」と区別できる。
```

- [ ] **Step 2: README.md の警告ブロックと net.rs 説明を更新**

`README.md:44` の直後に 1 行追加（既存の 2 行は変更しない）:

```
> Overlapping `enl` one-shots retry the bind themselves (`EADDRINUSE` only, 30 ms interval, up to 2 s), so brief collisions with cron/periodic callers resolve without the caller retrying.
```

`README.md:124` の `src/net.rs` の説明文:

```
- `src/net.rs` — UDP socket layer (owns `0.0.0.0:3610`). `discover` is a CIDR sweep plus a multicast probe (the standard ECHONET Lite discovery method); `listen` joins the `224.0.23.0` multicast group.
```

を以下に置き換える:

```
- `src/net.rs` — UDP socket layer (owns `0.0.0.0:3610`; retries `EADDRINUSE` binds for up to 2 s to absorb overlapping one-shots). `discover` is a CIDR sweep plus a multicast probe (the standard ECHONET Lite discovery method); `listen` joins the `224.0.23.0` multicast group.
```

- [ ] **Step 3: バージョンを 1.2.1 に上げる**

`Cargo.toml:3` を `version = "1.2.1"` に変更し、`cargo build` で `Cargo.lock` を追従させる。

Run: `cargo build && cargo run --quiet -- --version`
Expected: `enl 1.2.1`

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md Cargo.toml Cargo.lock
git commit -m "docs: bind リトライの挙動を記載し 1.2.1 に bump

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: 実機検証（検証ホスト上、常駐ポーリング稼働中のまま）

コード変更なし。検証ホスト・実機 IP はプライベート情報のためここには書かない（Claude のプロジェクトメモリ `home-lan-echonet-devices.md` 参照。以下 `$HOST` = 検証ホスト、`$DEV` = 実機 IP、`$EOJ` = 対象 EOJ）。

**Interfaces:**
- Consumes: Task 1-2 のバイナリ（`enl 1.2.1`）。
- Produces: 衝突が解消した実測エビデンス。

- [ ] **Step 1: 検証ホストへデプロイ**

```bash
ssh $HOST 'cd ~/enl && git pull && cargo build --release && install -m 755 target/release/enl ~/bin/enl && ~/bin/enl --version'
```

Expected: `enl 1.2.1`（検証ホストの /tmp は noexec。バイナリは ~/bin に置く）

- [ ] **Step 2: 修正前の確定再現手順が解消していることを確認（並列 2 発）**

```bash
ssh $HOST '~/bin/enl get $DEV $EOJ 80 >/dev/null 2>&1 & ~/bin/enl get $DEV $EOJ 80 >/dev/null 2>/tmp/enl_par; echo "second_exit=$?"; wait'
```

Expected: `second_exit=0`（修正前は 5）。/tmp/enl_par に bind エラー JSON が無いこと。

- [ ] **Step 3: 常駐ポーリング稼働中の soak（200 回連続 get）**

```bash
ssh $HOST 'fail=0; for n in $(seq 1 200); do ~/bin/enl get $DEV $EOJ 80 >/dev/null 2>>/tmp/enl_soak.err; c=$?; if [ $c -ne 0 ]; then fail=$((fail+1)); echo "fail iter=$n exit=$c"; fi; done; echo "fails=$fail"'
```

Expected: `fails=0`。修正前は常駐ポーリング（約 5 秒周期 burst、duty 比 ~5%）との衝突で確率的に exit 5 が混ざる条件。

- [ ] **Step 4: 常駐ポーリング側の bind エラーが増えていないことを確認**

```bash
ssh $HOST 'journalctl --since "-10 min" --no-pager | grep -c "バインド失敗" || true'
```

Expected: soak 実行時間帯に対応する新規の bind 失敗ログが 0 件（リトライは相互に効く: 常駐側も同じ enl バイナリを呼ぶため、デプロイだけで両方向の衝突が消える）。

- [ ] **Step 5: 検証結果を報告**

soak の fails 数・並列テストの exit・journal の件数を報告し、いずれかが期待と違えば superpowers:systematic-debugging で原因調査に戻る（成功を仮定して先へ進まない）。
