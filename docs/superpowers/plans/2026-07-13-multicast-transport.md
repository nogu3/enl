# multicast transport 対応 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** multicast (224.0.23.0) にしか応答しない ECHONET Lite 実機を discover で発見し、get/set/describe/raw の `--multicast` フラグで操作できるようにする。

**Architecture:** `net.rs` の `send_and_recv_one` を「送信先 (dst)」と「応答を期待する IP (expect)」を分離したシグネチャに一般化し、応答採用条件を「expect IP 一致 + EHD(0x1081) + TID 一致」に強化する。`discover` は sweep に加えて同一フレームを multicast へ 1 発送る常時併用にする。応答処理・出力 JSON は一切変えない。

**Tech Stack:** Rust (std のみ)、clap derive、tracing。**依存クレート追加は禁止**（socket2 / libc / tokio 不可）。

**Spec:** `docs/superpowers/specs/2026-07-12-multicast-transport-design.md`

## Global Constraints

- stdout JSON スキーマは**全コマンドで不変**。`schema.rs` は変更しない。
- exit code 不変: 0 成功 / 2 引数 / 3 timeout / 4 SNA / 5 network・bind / 1 その他。
- 依存クレート追加ゼロ。multicast の egress インタフェースは制御しない（ルーティングテーブル任せ、既知の制約としてドキュメント明記）。
- multicast グループへの join はしない（応答は unicast で返る。listen の join は既存のまま）。
- 自動フォールバック（unicast 失敗時の multicast 再試行）は実装しない。明示フラグのみ。
- 実機の IP・MAC をコード・テスト・ドキュメントに書かない。例示は RFC 5737 の `192.0.2.0/24`。
- 各タスク完了時に `cargo test` と `cargo clippy -- -D warnings` が通ること。
- コミットメッセージは既存リポジトリの規約（Conventional Commits、日本語本文可）に従う。

---

### Task 1: `is_reply_candidate` 純関数 (net.rs)

**Files:**
- Modify: `src/net.rs`（関数追加 + 末尾に `#[cfg(test)] mod tests` 新設）

**Interfaces:**
- Produces: `pub fn is_reply_candidate(data: &[u8], tid: u16) -> bool` — Task 2 / Task 4 が応答採用判定に使う。

- [ ] **Step 1: 失敗するテストを書く**

`src/net.rs` の末尾に追加:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_candidate_accepts_matching_ehd_and_tid() {
        // EHD=0x1081, TID=0x00AB + 適当な EDATA
        let data = [0x10, 0x81, 0x00, 0xAB, 0x0E, 0xF0, 0x01];
        assert!(is_reply_candidate(&data, 0x00AB));
    }

    #[test]
    fn reply_candidate_rejects_tid_mismatch() {
        let data = [0x10, 0x81, 0x00, 0xAB];
        assert!(!is_reply_candidate(&data, 0x00AC));
    }

    #[test]
    fn reply_candidate_rejects_ehd_mismatch() {
        // EHD2=0x82 (任意フォーマット) や別プロトコルは対象外
        assert!(!is_reply_candidate(&[0x10, 0x82, 0x00, 0xAB], 0x00AB));
        assert!(!is_reply_candidate(&[0x11, 0x81, 0x00, 0xAB], 0x00AB));
    }

    #[test]
    fn reply_candidate_rejects_short_data() {
        assert!(!is_reply_candidate(&[0x10, 0x81, 0x00], 0x00AB));
        assert!(!is_reply_candidate(&[], 0x00AB));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test is_reply_candidate 2>&1 | tail -5` → まずコンパイルエラー（`is_reply_candidate` 未定義）になることを確認。実行は `cargo test reply_candidate` で行う。

Expected: FAIL（`cannot find function is_reply_candidate`）

- [ ] **Step 3: 実装**

`src/net.rs` の `MULTICAST_ADDR` 定数の直後に追加:

```rust
/// 受信フレームが今回の要求への応答候補かを判定する。
/// EHD (0x1081) と TID の一致を要求する。multicast は他コントローラの
/// トラフィックと混線しうるため必須。unicast にも適用する
/// (ECHONET Lite 仕様上、応答 TID は要求 TID と一致する)。
pub fn is_reply_candidate(data: &[u8], tid: u16) -> bool {
    data.len() >= 4 && data[0..2] == [0x10, 0x81] && data[2..4] == tid.to_be_bytes()
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test reply_candidate`
Expected: 4 tests PASS

- [ ] **Step 5: clippy + 全テスト + コミット**

```bash
cargo test && cargo clippy -- -D warnings
git add src/net.rs
git commit -m "feat: 応答候補判定 is_reply_candidate を追加 (EHD+TID 一致)"
```

---

### Task 2: `send_and_recv_one` の一般化（dst / expect 分離 + TID 検査）

**Files:**
- Modify: `src/net.rs:111-159`（`send_and_recv_one`）
- Modify: `src/commands.rs`（`get` / `set` / `describe` / `raw` の呼び出し 4 箇所）

**Interfaces:**
- Consumes: Task 1 の `is_reply_candidate(data, tid)`。
- Produces: 新シグネチャ `pub fn send_and_recv_one(socket: &UdpSocket, dst: SocketAddr, expect: IpAddr, tid: u16, payload: &[u8], timeout: Duration) -> Result<Datagram, AppError>` — Task 3 が multicast 宛先で呼ぶ。

- [ ] **Step 1: 失敗するテストを書く**

`src/net.rs` の `mod tests` に追加（エフェメラルポート同士のループバックなので 3610 に依存せず CI で安定して動く）:

```rust
    use std::time::Duration;

    #[test]
    fn send_and_recv_one_skips_mismatch_then_accepts() {
        let dev = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dev_addr = dev.local_addr().unwrap();
        let cli = UdpSocket::bind("127.0.0.1:0").unwrap();

        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            let (_, from) = dev.recv_from(&mut buf).unwrap();
            // TID 不一致 → EHD 不一致 → 正しい応答、の順に返す
            dev.send_to(&[0x10, 0x81, 0xFF, 0xFF, 0x00], from).unwrap();
            dev.send_to(&[0x10, 0x82, 0x00, 0xAB, 0x00], from).unwrap();
            dev.send_to(&[0x10, 0x81, 0x00, 0xAB, 0x01], from).unwrap();
        });

        let dg = send_and_recv_one(
            &cli,
            dev_addr,
            dev_addr.ip(),
            0x00AB,
            &[0x10, 0x81, 0x00, 0xAB],
            Duration::from_millis(2000),
        )
        .unwrap();
        assert_eq!(dg.data, vec![0x10, 0x81, 0x00, 0xAB, 0x01]);
        t.join().unwrap();
    }

    #[test]
    fn send_and_recv_one_times_out_without_match() {
        let dev = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dev_addr = dev.local_addr().unwrap();
        let cli = UdpSocket::bind("127.0.0.1:0").unwrap();

        let err = send_and_recv_one(
            &cli,
            dev_addr,
            dev_addr.ip(),
            0x00AB,
            &[0x10, 0x81, 0x00, 0xAB],
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(err.kind, crate::error::ErrKind::Timeout);
    }
```

注意: `mod tests` 先頭の `use super::*;` で `send_and_recv_one` / `UdpSocket` は見える。`Duration` の import が重複したらコンパイルエラーに従って調整する。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test send_and_recv_one 2>&1 | tail -5`
Expected: コンパイルエラー（現行シグネチャは引数 4 つで、`expect` / `tid` を受けない）

- [ ] **Step 3: `send_and_recv_one` を書き換える**

`src/net.rs` の既存 `send_and_recv_one` (L111-159) を以下で置き換える:

```rust
/// フレームを dst へ送り、expect からの応答 1 発を待つ (get/set 用)。
/// unicast では dst.ip() == expect、multicast では dst = 224.0.23.0:3610。
/// 採用条件は「expect IP 一致 + EHD/TID 一致 (is_reply_candidate)」。
/// タイムアウト内に応答が無ければ timeout エラー (exit 3)。
pub fn send_and_recv_one(
    socket: &UdpSocket,
    dst: SocketAddr,
    expect: IpAddr,
    tid: u16,
    payload: &[u8],
    timeout: Duration,
) -> Result<Datagram, AppError> {
    socket
        .send_to(payload, dst)
        .map_err(|e| AppError::new(ErrKind::Network, format!("送信失敗: {e}")))?;

    socket
        .set_read_timeout(Some(timeout))
        .map_err(|e| AppError::new(ErrKind::Network, format!("set_read_timeout 失敗: {e}")))?;

    let mut buf = [0u8; 1500];
    let deadline = Instant::now() + timeout;
    loop {
        // 期待送信元以外や EHD/TID 不一致 (他機器・他コントローラのフレーム) は読み飛ばす。
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if from.ip() == expect && is_reply_candidate(&buf[..n], tid) {
                    return Ok(Datagram {
                        from,
                        data: buf[..n].to_vec(),
                    });
                }
                tracing::debug!(%from, len = n, "不一致フレームをスキップ (送信元/EHD/TID)");
                // 残り時間で再試行。
                match deadline.checked_duration_since(Instant::now()) {
                    Some(d) if !d.is_zero() => {
                        let _ = socket.set_read_timeout(Some(d));
                    }
                    _ => break,
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                break
            }
            Err(e) => return Err(AppError::new(ErrKind::Network, format!("受信失敗: {e}"))),
        }
    }
    Err(AppError::new(
        ErrKind::Timeout,
        format!("{expect} からの応答なし ({}ms)", timeout.as_millis()),
    ))
}
```

`src/net.rs` 冒頭の use に `IpAddr` を足す:

```rust
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
```

- [ ] **Step 4: commands.rs の呼び出し 4 箇所を更新**

`get` / `set` / `describe` / `raw` の各関数で、TID を変数に取り出して渡す。パターンは 4 箇所とも同じ。例として `get` (src/commands.rs:166-188):

```rust
pub fn get(ip: IpAddr, eoj: Eoj, epcs: &[u8], timeout: Duration) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let props: Vec<Property> = epcs.iter().map(|&e| Property::get(e)).collect();
    let tid = next_tid();
    let frame = Frame::standard(tid, CONTROLLER, eoj, Esv::Get, props);
    let dst = SocketAddr::new(ip, net::ECHONET_PORT);
    tracing::info!(%ip, eoj = eoj.to_hex(), "get 送信");

    let dg = net::send_and_recv_one(&socket, dst, ip, tid, &codec::build(&frame), timeout)?;
    // ... 以降変更なし
```

`set` (L191-225) / `describe` (L228-277) / `raw` (L284-315) も同様に:
1. `Frame::standard(next_tid(), ...)` を `let tid = next_tid(); let frame = Frame::standard(tid, ...)` に分解。
2. `net::send_and_recv_one(&socket, dst, &..., timeout)` を `net::send_and_recv_one(&socket, dst, ip, tid, &..., timeout)` に変更。

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test`
Expected: 全テスト PASS（`send_and_recv_one_skips_mismatch_then_accepts` / `send_and_recv_one_times_out_without_match` を含む）

- [ ] **Step 6: clippy + コミット**

```bash
cargo clippy -- -D warnings
git add src/net.rs src/commands.rs
git commit -m "feat: send_and_recv_one を dst/expect 分離 + TID 検査に一般化"
```

---

### Task 3: `--multicast` フラグ (get / set / describe / raw)

**Files:**
- Modify: `src/commands.rs`（`get` / `set` / `describe` / `raw` に `multicast: bool` 追加、dst 分岐ヘルパ追加）
- Modify: `src/main.rs`（clap の 4 サブコマンドに `--multicast` 追加、引き回し）

**Interfaces:**
- Consumes: Task 2 の `send_and_recv_one(socket, dst, expect, tid, payload, timeout)`。
- Produces: `commands::get(ip, eoj, epcs, timeout, multicast: bool)`（set/describe/raw も同様に末尾に `multicast: bool`）。CLI に `--multicast` フラグ。

- [ ] **Step 1: 失敗するテストを書く**

`src/commands.rs` の `mod tests` に追加:

```rust
    #[test]
    fn dst_for_unicast_and_multicast() {
        let ip: IpAddr = "192.0.2.22".parse().unwrap();
        assert_eq!(dst_for(ip, false).to_string(), "192.0.2.22:3610");
        assert_eq!(dst_for(ip, true).to_string(), "224.0.23.0:3610");
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test dst_for 2>&1 | tail -5`
Expected: コンパイルエラー（`dst_for` 未定義）

- [ ] **Step 3: commands.rs にヘルパと `multicast` 引数を実装**

`src/commands.rs` の `next_tid` の直後に追加:

```rust
/// 送信先を決める。multicast 時は 224.0.23.0:3610 へ送る
/// (multicast にしか応答しない機器向け。応答自体は ip から unicast で返る)。
fn dst_for(ip: IpAddr, multicast: bool) -> SocketAddr {
    if multicast {
        SocketAddr::new(IpAddr::V4(net::MULTICAST_ADDR), net::ECHONET_PORT)
    } else {
        SocketAddr::new(ip, net::ECHONET_PORT)
    }
}

/// stderr ログ用の transport 名。
fn transport_name(multicast: bool) -> &'static str {
    if multicast {
        "multicast"
    } else {
        "unicast"
    }
}
```

`get` / `set` / `describe` / `raw` の 4 関数それぞれに:
1. 引数リスト末尾（`timeout: Duration` の後）に `multicast: bool` を追加。
2. `let dst = SocketAddr::new(ip, net::ECHONET_PORT);` を `let dst = dst_for(ip, multicast);` に置換。
3. 送信時の `tracing::info!` に `transport = transport_name(multicast)` フィールドを追加。例 (`get`):

```rust
    tracing::info!(%ip, eoj = eoj.to_hex(), transport = transport_name(multicast), "get 送信");
```

応答処理（`send_and_recv_one` の `expect` に `ip` を渡す部分以降）は Task 2 のまま一切変えない。

- [ ] **Step 4: main.rs に `--multicast` を追加**

`Command` enum の `Get` / `Set` / `Describe` / `Raw` バリアントそれぞれに追加（`timeout_ms` フィールドの直前に置く）:

```rust
        /// 送信先を 224.0.23.0 (multicast) に切り替える。multicast にしか
        /// 応答しない機器向け。応答は ip から受ける。
        #[arg(long)]
        multicast: bool,
```

`run()` の 4 つの match アームでフィールドを受けて末尾引数として渡す。例 (`Get`):

```rust
        Command::Get {
            ip,
            eoj,
            epc,
            multicast,
            timeout_ms,
        } => {
            let eoj = parse_eoj(&eoj)?;
            let epcs = resolve_epcs(eoj, &epc)?;
            commands::get(ip, eoj, &epcs, Duration::from_millis(timeout_ms), multicast)
        }
```

`Set` / `Describe` / `Raw` も同様（`commands::set(..., multicast)` / `commands::describe(..., multicast)` / `commands::raw(..., multicast)`）。

- [ ] **Step 5: テスト + フラグの見た目を確認**

Run: `cargo test`
Expected: 全 PASS

Run: `cargo run -q -- get --help 2>&1 | grep -A2 multicast`
Expected: `--multicast` とヘルプ文が表示される

Run: `cargo run -q -- get 192.0.2.99 029101 80 --multicast --timeout-ms 200; echo "exit=$?"`
Expected: 応答が無いので stderr にエラー JSON、`exit=3`（timeout。ネットワーク疎通に依存せず送信自体は成功する。multicast 送信不可の環境で exit=5 になった場合はその旨を記録して続行してよい）

- [ ] **Step 6: clippy + コミット**

```bash
cargo clippy -- -D warnings
git add src/commands.rs src/main.rs
git commit -m "feat: get/set/describe/raw に --multicast フラグを追加"
```

---

### Task 4: discover の sweep + multicast 常時併用と CIDR 省略緩和

**Files:**
- Modify: `src/commands.rs`（`discover` 本体、`sweep_hosts` 新設、テスト追加）
- Modify: `src/main.rs`（`Discover` のヘルプ文更新）

**Interfaces:**
- Consumes: Task 1 の `net::is_reply_candidate(data, tid)`。
- Produces: `fn sweep_hosts(cidr: Option<&str>, iface: Option<Ipv4Addr>) -> Result<Option<Vec<Ipv4Addr>>, AppError>`（`Ok(None)` = sweep スキップ）。`discover` のシグネチャは不変。

- [ ] **Step 1: 失敗するテストを書く**

`src/commands.rs` の `mod tests` に追加:

```rust
    #[test]
    fn sweep_hosts_none_when_unresolvable() {
        // --cidr / -i とも無し → sweep スキップ (multicast のみ) を表す None
        assert!(sweep_hosts(None, None).unwrap().is_none());
    }

    #[test]
    fn sweep_hosts_invalid_explicit_cidr_errors() {
        // 明示 --cidr の書式不正は握りつぶさずエラー
        assert!(sweep_hosts(Some("nope/24"), None).is_err());
    }

    #[test]
    fn sweep_hosts_resolves_from_iface() {
        let hosts = sweep_hosts(None, Some(Ipv4Addr::new(192, 0, 2, 130)))
            .unwrap()
            .unwrap();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts.first(), Some(&Ipv4Addr::new(192, 0, 2, 1)));
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test sweep_hosts 2>&1 | tail -5`
Expected: コンパイルエラー（`sweep_hosts` 未定義）

- [ ] **Step 3: `sweep_hosts` を実装**

`src/commands.rs` の `resolve_cidr` の直前に追加:

```rust
/// sweep 対象ホストを解決する。--cidr / -i とも無ければ sweep をスキップして
/// multicast のみで探索する (Ok(None))。明示 --cidr の書式不正は従来どおりエラー。
fn sweep_hosts(
    cidr: Option<&str>,
    iface: Option<Ipv4Addr>,
) -> Result<Option<Vec<Ipv4Addr>>, AppError> {
    match resolve_cidr(cidr, iface) {
        Ok((base, prefix)) => Ok(Some(enumerate_hosts(base, prefix))),
        Err(e) if cidr.is_some() => Err(e),
        Err(_) => Ok(None),
    }
}
```

- [ ] **Step 4: `discover` を書き換える**

`src/commands.rs` の `discover` (L32-118) の送信部を以下に置き換える（受信集約部の変更は次 Step）:

```rust
/// sweep + multicast 併用の discovery。
///
/// 指定 CIDR (or iface IP の /24) 内の全ホストへ unicast `Get 0EF001 D6` を送り、
/// さらに同一フレームを multicast (224.0.23.0) へ 1 発送信して、`window` の間に
/// 返ってきたノードを集約する。multicast にしか応答しない実機が存在するため
/// 常時併用する。--cidr / -i とも省略時は sweep をスキップし multicast のみ。
pub fn discover(
    cidr: Option<&str>,
    iface: Option<Ipv4Addr>,
    window: Duration,
) -> Result<Value, AppError> {
    let hosts = sweep_hosts(cidr, iface)?;
    let socket = net::open_socket()?;
    let tid = next_tid();
    let frame = Frame::standard(
        tid,
        CONTROLLER,
        NODE_PROFILE,
        Esv::Get,
        vec![Property::get(EPC_INSTANCE_LIST)],
    );
    let payload = codec::build(&frame);

    match &hosts {
        Some(hosts) => {
            tracing::info!(
                hosts = hosts.len(),
                window_ms = window.as_millis(),
                "sweep + multicast discovery 送信"
            );
            for h in hosts {
                let dst = SocketAddr::new(IpAddr::V4(*h), net::ECHONET_PORT);
                if let Err(e) = socket.send_to(&payload, dst) {
                    tracing::debug!(ip = %h, error = %e, "send_to 失敗 (continue)");
                }
            }
        }
        None => {
            tracing::warn!(
                window_ms = window.as_millis(),
                "CIDR 解決不能 (--cidr / -i なし)。sweep をスキップし multicast のみで探索"
            );
        }
    }

    // multicast にしか応答しない機器向けに同一フレーム (同一 TID) を 1 発。
    let mdst = SocketAddr::new(IpAddr::V4(net::MULTICAST_ADDR), net::ECHONET_PORT);
    if let Err(e) = socket.send_to(&payload, mdst) {
        // sweep も無い場合は何も送れていないので network エラーにする。
        if hosts.is_none() {
            return Err(AppError::new(
                ErrKind::Network,
                format!("multicast 送信失敗: {e}"),
            ));
        }
        tracing::warn!(error = %e, "multicast 送信失敗 (sweep のみで継続)");
    }

    let datagrams = net::collect_until(&socket, window)?;
    // ... 受信集約部 (次 Step)
```

旧コードの「CIDR に有効ホストなし」エラー分岐 (L44-49) と `resolve_cidr` 直呼び (L42) は削除する（`resolve_cidr` 自体と既存テストは残す）。

- [ ] **Step 5: 受信集約部に EHD/TID 検査を追加**

`discover` の受信ループ先頭（`by_ip.contains_key` チェックの直後、`codec::parse` の前）に追加:

```rust
        if !net::is_reply_candidate(&dg.data, tid) {
            tracing::debug!(from = %dg.from, "EHD/TID 不一致フレームをスキップ");
            continue;
        }
```

既存の ESV 応答判定・IP 重複排除・JSON 組み立て・ソートは一切変えない。

- [ ] **Step 6: main.rs の Discover ヘルプを更新**

`src/main.rs` の `Discover` バリアントの doc コメント (L41) を差し替える:

```rust
    /// ノードを探索 (CIDR sweep + multicast 併用)。
    ///
    /// CIDR 内全ホストへの unicast Get と 224.0.23.0 への multicast Get を
    /// 同時に投げて応答を集約する。--cidr / -i とも省略時は multicast のみ。
    Discover {
```

`cidr` フィールドのヘルプ (L43) も更新:

```rust
        /// sweep する CIDR (例: 192.0.2.0/24)。省略時は -i のローカル IP から /24 を
        /// 推定し、それも無ければ sweep せず multicast のみで探索する。
        #[arg(long)]
        cidr: Option<String>,
```

- [ ] **Step 7: テスト + 動作確認**

Run: `cargo test`
Expected: 全 PASS（`sweep_hosts_*` 3 件を含む。旧 `resolve_cidr_neither_errors` テストは `resolve_cidr` を直接テストしており、関数を残すのでそのまま通る）

Run: `cargo run -q -- discover --timeout-ms 500; echo "exit=$?"`
Expected: 引数なしでエラーにならず動く。stderr に「sweep をスキップし multicast のみで探索」の warn、stdout に `{"devices":[...]}`（実機の無い環境では空配列）、`exit=0`

- [ ] **Step 8: clippy + コミット**

```bash
cargo clippy -- -D warnings
git add src/commands.rs src/main.rs
git commit -m "feat: discover を sweep + multicast 常時併用にし CIDR 省略を許可"
```

---

### Task 5: ドキュメント更新（multicast 不採用記述の撤回）

**Files:**
- Modify: `src/net.rs:6-7`（冒頭コメント）
- Modify: `CLAUDE.md`（「マルチキャスト」セクション）
- Modify: `README.md`（3 箇所）

**Interfaces:** なし（ドキュメントのみ。コード変更なし）。

- [ ] **Step 1: net.rs 冒頭コメントを更新**

`src/net.rs` L6-7 の

```rust
//! discover は CIDR sweep (各ホストへ unicast Get) 方式。multicast は
//! WiFi/AP 環境 (IGMP snooping, multicast 抑制) で信頼性が低いため採用しない。
```

を以下に差し替える:

```rust
//! discover は CIDR sweep (各ホストへ unicast Get) と multicast (224.0.23.0) の
//! 常時併用。multicast にしか応答しない実機 (AIF 認証済み機器で確認) が存在する
//! ため sweep だけでは不十分。multicast の egress インタフェースは制御しない
//! (ルーティングテーブル任せ)。制御には socket2 等の依存追加が要るため、
//! 依存ゼロ方針を優先した既知の制約 (実需が出たら -i 連動で追加する)。
```

- [ ] **Step 2: CLAUDE.md の「マルチキャスト」セクションを更新**

`CLAUDE.md` の

```markdown
## マルチキャスト

- アドレス: `224.0.23.0:3610`
- 受信のため該当インタフェースで `join_multicast_v4` する。
- discovery は「マルチキャストに投げて一定時間 recv を集める」方式。タイムアウトは CLI フラグで調整可能に。
```

を以下に差し替える:

```markdown
## マルチキャスト

- アドレス: `224.0.23.0:3610`
- `listen` は受信のため該当インタフェースで `join_multicast_v4` する。送信系コマンドは join しない（応答は unicast で返るため不要。join しないので自分の送信フレームがループバックで戻る問題も起きない）。
- `discover` は「CIDR sweep + multicast 1 発」の常時併用。**multicast にしか応答しない実機が存在する**（unicast で 3610 に届くフレームを無視し、multicast 宛にのみ応答する AIF 認証済み機器を確認）ため、sweep だけでは発見できない。タイムアウトは CLI フラグで調整可能。
- `get` / `set` / `describe` / `raw` は `--multicast` で送信先だけ `224.0.23.0` に切り替えられる。`ip` 引数は「応答を期待する送信元」になる。自動フォールバック（unicast 失敗時の multicast 再試行）はしない — 所要時間が読めなくなり one-shot の透明性が下がるため明示フラグとする。
- 応答の採用条件は「期待 IP 一致 + EHD(0x1081) + TID 一致」。multicast は他コントローラのトラフィックと混線しうるため必須で、unicast にも適用する。
- multicast の egress インタフェースは制御しない（ルーティングテーブル任せ）。制御には `socket2`/`libc` の依存追加が必要なため、依存ゼロ方針を優先した既知の制約。multi-homed 環境で意図しないインタフェースに流れうる（実需が出たら `-i` 連動の egress 制御を追加する）。
```

- [ ] **Step 3: README.md を更新**

3 箇所:

1. L37 の Docker 注記の括弧内を差し替え:

変更前:
```markdown
Port 3610 must be owned by the process, so **host networking is required** (a bridge network can't receive device responses; `discover` also uses a per-host unicast CIDR sweep).
```
変更後:
```markdown
Port 3610 must be owned by the process, so **host networking is required** (a bridge network can't receive device responses; `discover` uses a per-host unicast CIDR sweep plus a multicast probe).
```

2. 「Subcommands & output schemas」の該当行を更新:

変更前:
```markdown
- `discover [--timeout-ms 3000]` — `{"devices":[{"ip","count","instances":[...]}]}`
- `get <ip> <eoj> <epc...> [--timeout-ms 2000]` — `{"ip","eoj","esv","properties":[{"epc","name?","pdc","edt_hex","value?"}]}`
- `set <ip> <eoj> <epc> <edt> [--timeout-ms 2000]` — `{"ip","eoj","esv","result":"accepted","properties":[...]}`
- `describe <ip> <eoj> [--timeout-ms 2000]` — `{"ip","eoj","esv","get_map":[{"epc","name?","values?"}],"set_map":[...],"inf_map":[...]}`. `values` lists the value range of enum-typed EPCs (`{"41":"open","42":"close",...}`); numeric / unsupported EPCs omit it.
- `raw <ip> <deoj> <esv> [epc[:edt]...] [--seoj 05FF01] [--timeout-ms 2000]` — send an arbitrary ESV/EPC/EDT frame. `{"ip","sent_hex","response_hex","frame?":{...}}`. SNA is returned as `response_hex` rather than an error (a debugging / unsupported-op escape hatch); a `parse_error` is included if the response can't be parsed. EPC/EDT are hex-only here.
```
変更後:
```markdown
- `discover [--cidr <CIDR>] [--timeout-ms 3000]` — `{"devices":[{"ip","count","instances":[...]}]}`. Sends a unicast CIDR sweep **plus** one multicast probe (some certified devices only answer multicast). With neither `--cidr` nor `-i`, the sweep is skipped and only multicast is used.
- `get <ip> <eoj> <epc...> [--multicast] [--timeout-ms 2000]` — `{"ip","eoj","esv","properties":[{"epc","name?","pdc","edt_hex","value?"}]}`
- `set <ip> <eoj> <epc> <edt> [--multicast] [--timeout-ms 2000]` — `{"ip","eoj","esv","result":"accepted","properties":[...]}`
- `describe <ip> <eoj> [--multicast] [--timeout-ms 2000]` — `{"ip","eoj","esv","get_map":[{"epc","name?","values?"}],"set_map":[...],"inf_map":[...]}`. `values` lists the value range of enum-typed EPCs (`{"41":"open","42":"close",...}`); numeric / unsupported EPCs omit it.
- `raw <ip> <deoj> <esv> [epc[:edt]...] [--seoj 05FF01] [--multicast] [--timeout-ms 2000]` — send an arbitrary ESV/EPC/EDT frame. `{"ip","sent_hex","response_hex","frame?":{...}}`. SNA is returned as `response_hex` rather than an error (a debugging / unsupported-op escape hatch); a `parse_error` is included if the response can't be parsed. EPC/EDT are hex-only here.

`--multicast` sends the frame to `224.0.23.0` instead of `<ip>` — for devices that only respond to multicast-addressed frames — while the response is still expected from `<ip>`. There is no automatic fallback; the flag is always explicit. The multicast egress interface is left to the routing table (a known limitation on multi-homed hosts).
```

3. 「Project layout」の `src/net.rs` 行を差し替え:

変更前:
```markdown
- `src/net.rs` — UDP socket layer (owns `0.0.0.0:3610`). `discover` is a CIDR sweep (unicast Get to each host); `listen` joins the `224.0.23.0` multicast group.
```
変更後:
```markdown
- `src/net.rs` — UDP socket layer (owns `0.0.0.0:3610`). `discover` is a CIDR sweep plus a multicast probe (some devices only answer multicast); `listen` joins the `224.0.23.0` multicast group.
```

- [ ] **Step 4: 検証 + コミット**

```bash
cargo test && cargo clippy -- -D warnings
git add src/net.rs CLAUDE.md README.md
git commit -m "docs: multicast 不採用の記述を sweep 併用に改める"
```

---

## 実機検証（実装完了後・ユーザー作業）

WSL2 では検証不可（mirrored networking がエフェメラル送信元ポートの UDP 応答を取りこぼす）。LAN 直結の Linux ホスト (aarch64) で:

1. `aarch64-unknown-linux-musl` をクロスビルド → scp。
2. `enl discover` — multicast-only 機器（リンクプラス）が発見できること。
3. `enl get <ip> <eoj> 80 --multicast` — GetRes が返ること。
4. `enl set <ip> <eoj> 80 30 --multicast` — SetRes + 実照明の点灯を確認。
5. `--multicast` なしの既存コマンドの挙動・出力・exit code が従来と一致すること。

実機の IP・MAC はコード・テスト・ドキュメントに残さないこと。
