# listen の 224.0.23.0:3610 バインド化 (Issue #3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `enl listen` を `224.0.23.0:3610`（multicast グループアドレス）バインドに変更し、one-shot の `get`/`set` が `0.0.0.0:3610` + `SO_REUSEADDR` で共存できるようにする（GitHub Issue #3）。

**Architecture:** listen はグループアドレスに bind するため multicast (INF) のみ受信し、unicast 応答は one-shot の wildcard ソケットに届く。共存には双方の `SO_REUSEADDR` が必要（socket2 で設定）。REUSEADDR 下では one-shot 同士の EADDRINUSE 排他が消え、wildcard の二重バインドは後着が unicast を横取りするため、flock（std 1.89 の `File::try_lock`）によるロックファイルで直列化する。**コミット順序が重要**: flock を先に導入してから REUSEADDR 化する（逆にすると中間コミットで one-shot 排他が消える）。

**Tech Stack:** Rust (std::net + socket2), clap, serde_json, tracing。socket2 は `SO_REUSEADDR` 設定のみに使う。

**検証済み事実（この設計の前提、Issue #3 + 本セッションで確認済み）:**
1. `224.0.23.0:PORT`（REUSEADDR + join）のソケットは multicast のみ受信し、unicast は wildcard ソケットに届く
2. 共存には双方の `SO_REUSEADDR` が必要（片方だけだと従来どおり EADDRINUSE）
3. REUSEADDR 下で wildcard を二重バインドすると後着が unicast を横取りする → flock 必須
4. wildcard ソケットは join しなくても multicast を受信しうる（`IP_MULTICAST_ALL` 既定 1）→ 既存の「期待 IP + EHD/TID 一致」フィルタで読み飛ばすので実害なし
5. 開発環境（WSL2）でも loopback 経由の multicast 送受・group/wildcard 共存・unicast 振り分けがすべて再現することを Python で確認済み → ユニットテスト化できる

## Global Constraints

- Rust >= 1.89 必須（`File::try_lock`）。手元 toolchain は 1.97.0 で確認済み。MSRV は宣言しない（現状どおり）。
- 依存追加は `socket2` のみ。tokio・libc の直接依存は追加しない。
- stdout JSON の出力スキーマは一切変更しない（`schema.rs` に触らない）。
- exit code のセマンティクス不変: バインド失敗もロック取得失敗も 5 (`ErrKind::Bind`)。
- 「30ms 間隔・最大 2000ms 待ち」のリトライセマンティクスを flock にも適用（既存の `BIND_RETRY_INTERVAL` / `BIND_RETRY_WINDOW` 定数を共用）。
- 既存の EADDRINUSE bind リトライは残す（REUSEADDR を立てない外部常駐 = HA・旧 enl との衝突は従来どおり exit 5 に落とす）。
- テスト・ドキュメントの IP サンプルは `192.0.2.0/24`（RFC 5737）か `127.0.0.1`。実機 IP を書かない。
- テスト用の固定ポートは 23611〜23613 を使う（テストごとに別ポート、cargo test の並列実行で衝突しないように）。
- コミットメッセージは既存スタイル（`feat(net): ...` 等、日本語）。各コミット末尾に Co-Authored-By / Claude-Session フッタ。

---

### Task 1: flock による one-shot 排他の導入（ExclusiveSocket）

REUSEADDR 化（Task 2）の**前に**入れる。この時点では既存の EADDRINUSE 排他と二重になるが無害で、逆順だと中間コミットで one-shot 排他が消える。

**Files:**
- Modify: `src/net.rs`（`open_socket` の戻り型変更 + ロック機構追加）
- Modify なし: `src/commands.rs`（`ExclusiveSocket` が `Deref<Target = UdpSocket>` を実装するため、`&socket` の deref coercion で呼び出し側は無変更でコンパイルが通る）

**Interfaces:**
- Consumes: 既存の `bind_with_retry(SocketAddrV4, Duration, Duration) -> Result<UdpSocket, AppError>`、`ErrKind::Bind`、定数 `BIND_RETRY_INTERVAL` / `BIND_RETRY_WINDOW`
- Produces:
  - `pub struct ExclusiveSocket`（`Deref<Target = UdpSocket>` 実装、フィールドは非公開）
  - `pub fn open_socket() -> Result<ExclusiveSocket, AppError>`（シグネチャの戻り型のみ変更）
  - `fn acquire_lock(path: &Path, window: Duration, interval: Duration) -> Result<File, AppError>`（private、テストから直接呼ぶ）
  - `fn lock_path() -> PathBuf`（private）

- [ ] **Step 1: 失敗するテストを書く**

`src/net.rs` の `mod tests` 末尾に追加:

```rust
    /// テスト用ロックファイルパス (テスト間・プロセス間で衝突しないように)。
    fn test_lock_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("enl-test-{}-{}.lock", name, std::process::id()))
    }

    #[test]
    fn lock_times_out_while_held() {
        let path = test_lock_path("held");
        let _holder =
            acquire_lock(&path, Duration::from_millis(50), Duration::from_millis(10)).unwrap();
        let start = Instant::now();
        let err = acquire_lock(&path, Duration::from_millis(120), Duration::from_millis(10))
            .unwrap_err();
        assert_eq!(err.kind, crate::error::ErrKind::Bind);
        assert!(err.detail.contains("ロック"), "detail={}", err.detail);
        // 窓いっぱいまでは粘る
        assert!(start.elapsed() >= Duration::from_millis(120));
    }

    #[test]
    fn lock_acquired_after_release() {
        let path = test_lock_path("release");
        let holder =
            acquire_lock(&path, Duration::from_millis(50), Duration::from_millis(10)).unwrap();
        // 100ms 後に holder が解放する = 他の one-shot が終わる状況
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });
        let _lock =
            acquire_lock(&path, Duration::from_millis(2000), Duration::from_millis(10)).unwrap();
        t.join().unwrap();
    }
```

- [ ] **Step 2: テストが失敗する（コンパイルエラーになる）ことを確認**

Run: `cargo test --lib net`
Expected: FAIL — `acquire_lock` が未定義のコンパイルエラー

- [ ] **Step 3: 実装を書く**

`src/net.rs` の use 節を以下に変更:

```rust
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
```

既存の `open_socket` を以下に置き換え、直後にロック機構と `ExclusiveSocket` を追加:

```rust
/// one-shot 用の 3610 ソケット。flock (_lock) をソケットと同寿命で保持し、
/// one-shot 同士を直列化する。Deref<Target = UdpSocket> なので呼び出し側は
/// 通常の &UdpSocket として使える。
pub struct ExclusiveSocket {
    socket: UdpSocket,
    _lock: File,
}

impl Deref for ExclusiveSocket {
    type Target = UdpSocket;
    fn deref(&self) -> &UdpSocket {
        &self.socket
    }
}

/// 3610 を専有する one-shot 用ソケットを開く。flock で one-shot 同士を
/// 直列化した上で 0.0.0.0:3610 に bind する。AddrInUse は BIND_RETRY_WINDOW
/// までリトライし、それ以外のバインド失敗は即 bind エラー (exit 5)。
pub fn open_socket() -> Result<ExclusiveSocket, AppError> {
    let lock = acquire_lock(&lock_path(), BIND_RETRY_WINDOW, BIND_RETRY_INTERVAL)?;
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, ECHONET_PORT);
    let socket = bind_with_retry(bind_addr, BIND_RETRY_WINDOW, BIND_RETRY_INTERVAL)?;
    Ok(ExclusiveSocket {
        socket,
        _lock: lock,
    })
}

/// one-shot 排他ロックのファイルパス。SO_REUSEADDR 導入後は one-shot 同士の
/// EADDRINUSE 排他が働かない (wildcard の二重バインドが通り後着が unicast を
/// 横取りする、実機検証 2026-07-16) ため、flock で直列化する。
/// XDG_RUNTIME_DIR (per-user, systemd Linux 標準) を優先し、無ければ /tmp。
fn lock_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("enl-3610.lock")
}

/// path の flock を bind リトライと同じセマンティクス (interval 間隔・window
/// まで) で取得する。取れなければ bind エラー (exit 5)。
fn acquire_lock(path: &Path, window: Duration, interval: Duration) -> Result<File, AppError> {
    // /tmp フォールバック時、他ユーザーが作ったファイルは書込 open できない
    // ことがあるため読取専用 open にフォールバックする (flock は読取 fd でも取れる)。
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .or_else(|_| File::open(path))
        .map_err(|e| {
            AppError::new(
                ErrKind::Bind,
                format!("ロックファイル {} を開けない: {e}", path.display()),
            )
        })?;
    let deadline = Instant::now() + window;
    let mut waited = false;
    loop {
        match file.try_lock() {
            Ok(()) => {
                if waited {
                    tracing::info!(path = %path.display(), "ロック解放を確認、取得成功");
                }
                return Ok(file);
            }
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(AppError::new(
                        ErrKind::Bind,
                        format!(
                            "one-shot ロック {} を取得できず ({}ms 再試行)。他の enl one-shot が長時間専有していないか確認",
                            path.display(),
                            window.as_millis()
                        ),
                    ));
                }
                if !waited {
                    // 対話利用時に無言で待たないよう、待ち始めに 1 回だけ知らせる
                    tracing::info!(
                        path = %path.display(),
                        "他の one-shot がロック保持中。最大 {}ms 解放を待つ",
                        window.as_millis()
                    );
                    waited = true;
                }
                std::thread::sleep(interval);
            }
            Err(TryLockError::Error(e)) => {
                return Err(AppError::new(
                    ErrKind::Bind,
                    format!("ロック {} の取得失敗: {e}", path.display()),
                ));
            }
        }
    }
}
```

注意: `src/commands.rs` は変更不要。`let socket = net::open_socket()?;` の後の `&socket` は deref coercion で `&UdpSocket` になり、`socket.send_to(...)` も auto-deref で通る。

- [ ] **Step 4: 全テストが通ることを確認**

Run: `cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS、clippy 警告なし

- [ ] **Step 5: コミット**

```bash
git add src/net.rs
git commit -m "feat(net): one-shot 排他を flock で直列化 (ExclusiveSocket)

SO_REUSEADDR 導入 (次コミット) 後は EADDRINUSE による one-shot 同士の
排他が働かなくなるため、先に flock による直列化を入れる。
リトライは既存 bind と同じ 30ms 間隔・最大 2000ms 窓。

Refs #3"
```

---

### Task 2: socket2 導入、3610 バインドに SO_REUSEADDR を付与

**Files:**
- Modify: `Cargo.toml`（socket2 依存追加）
- Modify: `src/net.rs`（`bind_reuse` 追加、`bind_with_retry` がそれを使う）

**Interfaces:**
- Consumes: Task 1 の状態（flock 排他が既に有効）
- Produces: `fn bind_reuse(addr: SocketAddrV4) -> io::Result<UdpSocket>`（private、Task 3 のテストからも使う）。`bind_with_retry` のシグネチャは不変。

- [ ] **Step 1: socket2 を追加**

Run: `cargo add socket2`
Expected: `Cargo.toml` の `[dependencies]` に socket2 の最新版が追加される

- [ ] **Step 2: 失敗するテストを書く**

`src/net.rs` の `mod tests` に追加:

```rust
    #[test]
    fn bind_reuse_allows_group_and_wildcard_coexistence() {
        // 共存の核: REUSEADDR 同士なら 224.0.23.0:P と 0.0.0.0:P を同時に bind できる
        const PORT: u16 = 23611;
        let group = bind_reuse(SocketAddrV4::new(MULTICAST_ADDR, PORT)).unwrap();
        let wildcard = bind_reuse(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT)).unwrap();
        assert_eq!(group.local_addr().unwrap().port(), PORT);
        assert_eq!(wildcard.local_addr().unwrap().port(), PORT);
    }

    #[test]
    fn bind_reuse_still_conflicts_with_plain_bind() {
        // REUSEADDR は双方に必要: 相手 (HA・旧 enl 相当) が plain bind なら従来どおり AddrInUse
        const PORT: u16 = 23612;
        let _holder = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT)).unwrap();
        let err = bind_reuse(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }
```

- [ ] **Step 3: テストが失敗する（コンパイルエラーになる）ことを確認**

Run: `cargo test --lib net`
Expected: FAIL — `bind_reuse` が未定義のコンパイルエラー

- [ ] **Step 4: 実装を書く**

`src/net.rs` に use を追加:

```rust
use socket2::{Domain, Protocol, Socket, Type};
```

`bind_with_retry` の直前に追加:

```rust
/// SO_REUSEADDR 付きで UDP ソケットを bind する。listen (224.0.23.0:3610) と
/// one-shot (0.0.0.0:3610) の共存には双方の REUSEADDR が必要 (実機検証
/// 2026-07-16)。socket2 はこの 1 点のためだけに使う。
fn bind_reuse(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::V4(addr).into())?;
    Ok(socket.into())
}
```

`bind_with_retry` のループ内 `UdpSocket::bind(addr)` を `bind_reuse(addr)` に置き換える（1 箇所）。

既存テストへの影響: `bind_retry_succeeds_after_release` / `bind_retry_times_out_when_never_released` / `bind_fails_immediately_on_non_addrinuse` は holder が plain bind（REUSEADDR なし）なので従来どおり AddrInUse / EADDRNOTAVAIL になり、変更なしで通る。

- [ ] **Step 5: 全テストが通ることを確認**

Run: `cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS、clippy 警告なし

- [ ] **Step 6: コミット**

```bash
git add Cargo.toml Cargo.lock src/net.rs
git commit -m "feat(net): socket2 を導入し 3610 バインドに SO_REUSEADDR を付与

listen (224.0.23.0:3610) と one-shot (0.0.0.0:3610) の共存には双方の
REUSEADDR が必要 (実機検証済み)。one-shot 同士の排他は前コミットの
flock が担う。REUSEADDR を立てない外部プロセス (HA・旧 enl) との
衝突は従来どおり EADDRINUSE リトライ → exit 5。

Refs #3"
```

---

### Task 3: listen を 224.0.23.0:3610 バインドに変更

**Files:**
- Modify: `src/net.rs`（`open_listen_socket` 追加、`join_multicast` を private 化）
- Modify: `src/commands.rs`（`listen` のソケット確保差し替え、`reply_infc_res` をエフェメラル送信に変更）
- Modify: `src/main.rs`（`Listen` サブコマンドの doc comment 更新のみ）

**Interfaces:**
- Consumes: Task 2 の `bind_reuse`、既存の `bind_with_retry` / `join_multicast` / `open_ephemeral_socket`
- Produces: `pub fn open_listen_socket(iface: Option<Ipv4Addr>) -> Result<UdpSocket, AppError>`。`pub fn join_multicast` は `fn join_multicast` になる（外部から使うのは listen だけだったため）。`reply_infc_res` のシグネチャは `fn reply_infc_res(src: IpAddr, frame: &Frame)` に変わる（socket 引数が消える）。

- [ ] **Step 1: net 層の失敗するテストを書く**

`src/net.rs` の `mod tests` に追加:

```rust
    #[test]
    fn group_bound_socket_receives_multicast_and_ignores_unicast() {
        const PORT: u16 = 23613;
        let group = bind_reuse(SocketAddrV4::new(MULTICAST_ADDR, PORT)).unwrap();
        join_multicast(&group, Some(Ipv4Addr::LOCALHOST)).unwrap();
        let wildcard = bind_reuse(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT)).unwrap();

        // 送信側: lo 経由の multicast → 自ホストへループバックさせる
        let sender = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        sender.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
        sender.set_multicast_loop_v4(true).unwrap();
        let sender: UdpSocket = sender.into();
        sender.send_to(b"MCAST", (MULTICAST_ADDR, PORT)).unwrap();
        sender.send_to(b"UNI", (Ipv4Addr::LOCALHOST, PORT)).unwrap();

        // group ソケット: multicast は受かり、unicast は届かない
        let dg = recv_one(&group, Some(Instant::now() + Duration::from_millis(2000)))
            .unwrap()
            .expect("multicast 未達");
        assert_eq!(dg.data, b"MCAST");
        assert!(recv_one(&group, Some(Instant::now() + Duration::from_millis(300)))
            .unwrap()
            .is_none());

        // wildcard ソケット: unicast が届く (multicast も IP_MULTICAST_ALL で
        // 届きうるため、目当ての UNI が来るまで読み飛ばす)
        let deadline = Instant::now() + Duration::from_millis(2000);
        loop {
            let dg = recv_one(&wildcard, Some(deadline)).unwrap().expect("unicast 未達");
            if dg.data == b"UNI" {
                break;
            }
        }
    }
```

- [ ] **Step 2: net テストが通ることを確認（既存部品のみで書けているため）**

Run: `cargo test --lib net`
Expected: PASS（このテストは既存の `bind_reuse` / `join_multicast` / `recv_one` の組み合わせ検証。WSL2 の loopback multicast で成立することは事前検証済み。万一この環境で FAIL する場合は実装ではなく環境を疑い、jarvis 等の LAN 直結 Linux で再確認する）

- [ ] **Step 3: commands 層の失敗するテストを書く**

`src/commands.rs` の `mod tests` 冒頭（`use super::*;` の直後）に追加:

```rust
    use std::sync::Mutex;

    /// 127.0.0.1:3610 を bind するテストの直列化 (cargo test は並列実行のため)。
    static PORT_3610: Mutex<()> = Mutex::new(());
```

既存の `set_nowait_sends_seti_from_ephemeral_and_returns_sent` の先頭（`let dev = ...` の前）に 1 行追加:

```rust
        let _guard = PORT_3610.lock().unwrap();
```

`mod tests` 末尾に追加:

```rust
    #[test]
    fn reply_infc_res_replies_from_ephemeral_port() {
        use std::net::UdpSocket;
        use std::time::Duration;
        let _guard = PORT_3610.lock().unwrap();
        // 機器役: INFC の送信元として INFC_Res を 3610 で受ける。
        let dev = UdpSocket::bind("127.0.0.1:3610").expect("127.0.0.1:3610 が使用中");
        let mut buf = [0u8; 1500];

        // INF (応答不要) には何も返さない
        reply_infc_res("127.0.0.1".parse().unwrap(), &inf_frame(Esv::Inf));
        dev.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        assert!(dev.recv_from(&mut buf).is_err());

        // INFC には INFC_Res が返る
        reply_infc_res("127.0.0.1".parse().unwrap(), &inf_frame(Esv::InfC));
        dev.set_read_timeout(Some(Duration::from_millis(2000))).unwrap();
        let (n, from) = dev.recv_from(&mut buf).unwrap();
        // EDATA: ESV=0x7A (INFC_Res), OPC=1, EPC=0x80, PDC=0
        assert_eq!(&buf[10..n], &[0x7A, 0x01, 0x80, 0x00]);
        // listen ソケット (グループアドレス) ではなくエフェメラルポートから送られる
        assert_ne!(from.port(), 3610);
    }
```

- [ ] **Step 4: テストが失敗する（コンパイルエラーになる）ことを確認**

Run: `cargo test --lib commands`
Expected: FAIL — `reply_infc_res` の引数個数が合わないコンパイルエラー

- [ ] **Step 5: 実装を書く**

`src/net.rs`: `join_multicast` の `pub fn` を `fn` に変更し、その直前に追加:

```rust
/// listen 用ソケットを開く。wildcard ではなく 224.0.23.0:3610 (multicast
/// グループアドレスそのもの) に bind するため multicast (INF) のみ受信し、
/// 0.0.0.0:3610 が空いて one-shot と共存できる。トレードオフ: unicast 宛ての
/// INF/INFC は受けられない (状変アナウンスは multicast なので実害はほぼ無い)。
/// グループアドレスへの bind は Linux 前提。REUSEADDR 同士なので複数 listen も
/// 共存でき、各々に multicast が届く。
pub fn open_listen_socket(iface: Option<Ipv4Addr>) -> Result<UdpSocket, AppError> {
    let addr = SocketAddrV4::new(MULTICAST_ADDR, ECHONET_PORT);
    let socket = bind_with_retry(addr, BIND_RETRY_WINDOW, BIND_RETRY_INTERVAL)?;
    join_multicast(&socket, iface)?;
    Ok(socket)
}
```

`src/commands.rs` の `listen` 冒頭 2 行:

```rust
    let socket = net::open_socket()?;
    net::join_multicast(&socket, iface)?;
```

を以下に置き換え:

```rust
    let socket = net::open_listen_socket(iface)?;
```

同じく `listen` のループ内の呼び出しを変更:

```rust
                reply_infc_res(&socket, dg.from.ip(), &frame);
```

を:

```rust
                reply_infc_res(dg.from.ip(), &frame);
```

`reply_infc_res` を以下に置き換え:

```rust
/// INFC (0x74) は応答必須なので INFC_Res を返す。失敗しても収集は止めない。
/// listen ソケットは 224.0.23.0 に bind されており unicast 送信の送信元に
/// 使えない (送信元アドレスがグループアドレスになる) ため、エフェメラル
/// ソケットから送る。
fn reply_infc_res(src: IpAddr, frame: &Frame) {
    let (seoj, esv, props) = match &frame.edata {
        Edata::Standard {
            seoj, esv, props, ..
        } => (*seoj, *esv, props),
        _ => return,
    };
    if esv != Esv::InfC {
        return;
    }
    let socket = match net::open_ephemeral_socket() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%src, error = %e, "INFC_Res 用ソケット確保失敗 (continue)");
            return;
        }
    };
    let res = Frame::standard(
        frame.tid,
        CONTROLLER,
        seoj,
        Esv::InfCRes,
        props.iter().map(|p| Property::get(p.epc)).collect(),
    );
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
}
```

（`use std::net::UdpSocket` 型の引数が消えるので、関数シグネチャの `socket: &std::net::UdpSocket` 引数も削除されている点に注意。）

`src/commands.rs` の `listen` の doc comment を更新:

```rust
/// INF / INFC 通知を待ち受けて収集する (one-shot: count 件か deadline で終了)。
///
/// 224.0.23.0:3610 (multicast グループアドレス) に bind して INF (0x73) /
/// INFC (0x74) のみ採用する。0.0.0.0:3610 は使わないため one-shot の get/set と
/// 共存できる。unicast 宛ての通知は受けられない (既知のトレードオフ)。
/// INFC には仕様上の応答 (INFC_Res) を best-effort で返す。
/// deadline までに 1 件も来なければ timeout (exit 3)、1 件以上あれば成功。
```

`src/main.rs` の `Listen` バリアントの doc comment:

```rust
    /// INF / INFC 通知を待ち受ける (one-shot: count 件かタイムアウトで終了)。
    ///
    /// 224.0.23.0:3610 (multicast グループ) に bind して通知を収集する。
    /// 0.0.0.0:3610 は使わないため get/set の同時実行と共存できる。1 件以上
    /// 集まれば成功、0 件のままタイムアウトすると exit 3。状変連動
    /// (「照明が消えたら〜」) はこのコマンドを外部のループ (cron / n8n /
    /// シェル) から回して組む。
```

- [ ] **Step 6: 全テストが通ることを確認**

Run: `cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS、clippy 警告なし

- [ ] **Step 7: 共存の手動スモークテスト（loopback）**

```bash
cargo build
# listen をバックグラウンドで起動 (グループアドレス bind)
RUST_LOG=info ./target/debug/enl listen --timeout-ms 4000 & sleep 0.5
# listen 常駐中に one-shot が 3610 を bind できること。宛先は RFC 5737 の
# ダミー IP (127.0.0.1 宛てだと自分の wildcard ソケットに自フレームが届いて
# しまい検証にならない)。応答は無いので exit 3 = timeout が正解。
# 従来はここが exit 5 = Address in use になっていた。
./target/debug/enl get 192.0.2.10 013001 80 --timeout-ms 500; echo "exit=$?"
wait
```

Expected: `get` が `exit=3`（timeout。bind 失敗の 5 ではない）。listen は INF が来ないので exit 3 で終わる。

- [ ] **Step 8: コミット**

```bash
git add src/net.rs src/commands.rs src/main.rs
git commit -m "feat(listen): 224.0.23.0:3610 バインドに変更し one-shot と共存

INF は multicast で届くため listen はグループアドレスへの bind で足りる。
これで 0.0.0.0:3610 が空き、listen 常駐中でも get/set が成功する。
INFC_Res はグループアドレスを送信元にできないためエフェメラルポート
から送る。unicast 宛て INF/INFC は受けられない (既知のトレードオフ)。

Closes #3"
```

---

### Task 4: ドキュメント更新と 1.5.0 バンプ

**Files:**
- Modify: `Cargo.toml`（version 1.5.0）+ `Cargo.lock`（cargo build で追従）
- Modify: `CLAUDE.md`（3610 セクション・技術スタック表・マルチキャストセクション）
- Modify: `README.md`（注意書き・listen / --nowait 説明・source layout）
- Modify: `src/net.rs`（モジュール doc comment）

**Interfaces:**
- Consumes: Task 1〜3 の実装済み挙動
- Produces: なし（ドキュメントのみ）

- [ ] **Step 1: Cargo.toml の version を 1.5.0 に**

`version = "1.4.0"` → `version = "1.5.0"`。その後 `cargo build` で Cargo.lock を追従させる。

- [ ] **Step 2: src/net.rs のモジュール doc comment を更新**

冒頭コメントを以下に置き換え:

```rust
//! UDP ソケット層。ステートレス / one-shot。
//!
//! 最重要: 仕様準拠機器は応答を送信元ポートでなく 3610 に返す。
//! one-shot は 0.0.0.0:3610 に SO_REUSEADDR 付きで bind し、one-shot 同士は
//! flock で直列化する。listen は 224.0.23.0:3610 (multicast グループアドレス)
//! に bind するため 0.0.0.0:3610 が空き、one-shot と共存できる (実機検証
//! 2026-07-16)。REUSEADDR を立てない外部プロセス (HA 等) の専有は従来どおり
//! EADDRINUSE リトライの末に exit 5。
//!
//! discover は CIDR sweep (各ホストへ unicast Get) と multicast (224.0.23.0) の
//! 常時併用。multicast は ECHONET Lite 標準の探索方式で、CIDR 不明でも引数なしで
//! 探索できる。multicast の egress インタフェースは制御しない
//! (ルーティングテーブル任せ)。socket2 は導入済みだが egress 制御は実需が
//! 出るまで足さない (実需が出たら -i 連動で追加する)。
```

- [ ] **Step 3: CLAUDE.md を更新**

(a) 技術スタック表のネットワーク行:

```markdown
| ネットワーク | `std::net::UdpSocket` + `socket2` | socket2 は `SO_REUSEADDR` 設定のみに使う。**tokio は入れない**（下記参照） |
```

(b) 「⚠️ 最重要の落とし穴: ポート 3610 専有」セクションを以下に置き換え（見出しは「⚠️ 最重要の落とし穴: ポート 3610」に変更）:

```markdown
## ⚠️ 最重要の落とし穴: ポート 3610

仕様準拠の機器の多くは、応答を**送信元エフェメラルポートではなく 3610 番に返す**。確実に応答を受けるには 3610 で受信する必要がある。

- **応答先が 3610 固定であることは実機検証済み (2026-07-16)**: エフェメラル送信元ポートで unicast Get を送ると全機器が無応答になり、tcpdump では応答が常に送信元ポートでなく 3610 宛てに返ることを確認した。「送信系をエフェメラル化して応答も待つ」設計は成立しない。
- **enl 同士の 3610 共存モデル (v1.5.0、実機検証済み)**:
  - `listen` は `224.0.23.0:3610`（multicast グループアドレスそのもの）に `SO_REUSEADDR` 付きで bind して join する。このソケットは multicast しか受けない。グループアドレスへの bind は Linux 前提。
  - one-shot（get/set/discover/describe/raw）は `0.0.0.0:3610` に `SO_REUSEADDR` 付きで bind する。unicast 応答は wildcard 側に届くため listen と共存できる。共存には**双方**の REUSEADDR が必要。
  - one-shot 同士は flock（`$XDG_RUNTIME_DIR/enl-3610.lock`、無ければ `/tmp`）で直列化する。REUSEADDR 下では wildcard の二重バインドが通ってしまい、後着ソケットが unicast を横取りするため（実機検証済み）。ロック取得は 30ms 間隔・最大 2000ms 待ちで、枯渇したら exit 5。
  - トレードオフ: listen は unicast 宛ての INF/INFC を受けられない（状変アナウンスは multicast なので実害はほぼ無い）。
  - wildcard ソケットは join しなくても multicast を受信しうる（`IP_MULTICAST_ALL` 既定 1）。one-shot は既存の「期待 IP + EHD/TID 一致」フィルタで読み飛ばすので実害なし。
- Home Assistant の ECHONET 統合など REUSEADDR を立てない外部プロセスが 3610 を握っている間は従来どおり応答が吸われる／bind できない。`EADDRINUSE` に限り 30ms 間隔・最大 2000ms リトライしてから exit 5 となり、stderr の detail で「再試行しても解放されず」と区別できる。最終的に本 CLI を唯一のコントローラにする（元のゴールと一致）。
- バインド失敗・ロック取得失敗は専用 exit code (5) で切り分けられること。
- `set --nowait` は SetI (0x60, 応答不要) をエフェメラルポートから送信のみ行う。3610 にもロックにも触れない最速経路として残る。機器リジェクト (SetI_SNA) は検知できず、exit 0 は送信成功のみを意味する（従来どおり）。
```

(c) マルチキャストセクションの listen 行と egress 行を更新:

```markdown
- `listen` は `224.0.23.0:3610`（グループアドレスそのもの）に bind して `join_multicast_v4` する。送信系コマンドは join しない（応答は unicast で返るため不要。join しないので自分の送信フレームがループバックで戻る問題も起きない）。
```

```markdown
- multicast の egress インタフェースは制御しない（ルーティングテーブル任せ）。socket2 は SO_REUSEADDR のために導入済みだが、egress 制御は実需が出るまで足さない（YAGNI）。multi-homed 環境で意図しないインタフェースに流れうる（実需が出たら `-i` 連動の egress 制御を追加する）。
```

- [ ] **Step 4: README.md を更新**

(a) 83〜84 行目の blockquote を以下に置き換え:

```markdown
> ⚠️ If an ECHONET integration (Home Assistant, etc.) holds port 3610, it will steal the responses. Stop it while testing.
> `enl` processes coexist with each other (v1.5.0): `listen` binds the multicast group address `224.0.23.0:3610` instead of the wildcard, one-shots bind `0.0.0.0:3610` with `SO_REUSEADDR`, and overlapping one-shots are serialized with a lock file (`$XDG_RUNTIME_DIR/enl-3610.lock`, falling back to `/tmp`; 30 ms interval, up to 2 s). Non-`enl` holders of 3610 still cause exit 5 after the `EADDRINUSE` retry window.
> Sample IPs use the RFC 5737 documentation range `192.0.2.0/24` — replace them with your real device IPs.
```

(b) `--nowait` 段落（124 行目）を以下に置き換え:

```markdown
`--nowait` sends SetI (0x60, no response requested) from an ephemeral port and exits 0 as soon as the datagram is sent, without binding port 3610 or taking the one-shot lock. Since v1.5.0 a plain `set` also coexists with a running `listen`, so `--nowait` is now just the fastest fire-and-forget path. The trade-off: device rejections (SetI_SNA) are undetectable, because devices reply to port 3610 regardless of the request's source port (verified with real devices). `"result":"sent"` means "sent", not "executed" — confirm via the INF that `listen` receives, or a follow-up `get`.
```

(c) `listen` の説明（131 行目）の括弧書き `(binds 3610, joins `224.0.23.0`)` を `(binds the multicast group address `224.0.23.0:3610` — the wildcard `0.0.0.0:3610` stays free, so `get`/`set` run concurrently)` に変更し、段落末尾に追記:

```markdown
Because the socket is bound to the group address, unicast-addressed INF/INFC cannot be received (state-change announcements are multicast, so this rarely matters); binding a multicast address is Linux-specific. Multiple concurrent `listen` processes each receive a copy of every notification.
```

(d) source layout の `src/net.rs` 行（170 行目）を以下に置き換え:

```markdown
- `src/net.rs` — UDP socket layer. One-shots own `0.0.0.0:3610` with `SO_REUSEADDR` (serialized among themselves via flock, `EADDRINUSE` binds retried for up to 2 s against non-`enl` holders); `listen` binds the multicast group address `224.0.23.0:3610` so both coexist. `discover` is a CIDR sweep plus a multicast probe (the standard ECHONET Lite discovery method).
```

- [ ] **Step 5: ビルド・テスト・クリップィで確認**

Run: `cargo build && cargo test && cargo clippy -- -D warnings`
Expected: 全 PASS（Cargo.lock の version が 1.5.0 に追従していることも確認）

- [ ] **Step 6: コミット**

```bash
git add Cargo.toml Cargo.lock CLAUDE.md README.md src/net.rs
git commit -m "docs: 3610 共存モデル (listen=グループ bind + one-shot=REUSEADDR+flock) を反映、1.5.0 に bump

Refs #3"
```

---

## 受け入れ条件との対応 (Issue #3)

| 受け入れ条件 | 担保するもの |
|---|---|
| `enl listen` 常駐中に `enl get` / `enl set`（--nowait なし）が成功する | Task 2 `bind_reuse_allows_group_and_wildcard_coexistence` + Task 3 の unicast 振り分けテストと手動スモーク（最終確認は jarvis 実機） |
| listen が INF（multicast）を従来どおり受信できる | Task 3 `group_bound_socket_receives_multicast_and_ignores_unicast`（最終確認は jarvis 実機のリンクプラス INF） |
| one-shot 同士の同時実行が flock で直列化される | Task 1 `lock_times_out_while_held` / `lock_acquired_after_release` |
| REUSEADDR 非対応の外部プロセスが 3610 を専有している場合は従来どおり exit 5 | Task 2 `bind_reuse_still_conflicts_with_plain_bind` + 既存の bind リトライテスト |
| codec / net の既存テストが全て通る | 各タスクの `cargo test` |

## デプロイ時の注意（実装後、リポジトリ外の作業）

- jarvis の `/usr/local/bin/enl` を新バイナリに差し替えた後、**listen を抱える常駐（casad 等）の再起動が必要**。旧バイナリの listen が `0.0.0.0:3610` を握り続けている間は共存効果が出ない。
- mando の無期限 listen も同様に要再起動。
