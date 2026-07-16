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

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::error::{AppError, ErrKind};

pub const ECHONET_PORT: u16 = 3610;
/// ECHONET Lite のマルチキャストアドレス。INF 通知はここへ送られる。
pub const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 23, 0);

/// 受信フレームが今回の要求への応答候補かを判定する。
/// EHD (0x1081) と TID の一致を要求する。multicast は他コントローラの
/// トラフィックと混線しうるため必須。unicast にも適用する
/// (ECHONET Lite 仕様上、応答 TID は要求 TID と一致する)。
pub fn is_reply_candidate(data: &[u8], tid: u16) -> bool {
    data.len() >= 4 && data[0..2] == [0x10, 0x81] && data[2..4] == tid.to_be_bytes()
}

/// bind リトライの間隔と最大待ち時間。本 CLI を定期実行する別プロセス
/// (cron / 常駐アプリの one-shot 呼び出し) との瞬間的な 3610 衝突を吸収する。
/// 恒常専有 (HA 等) は窓を使い切って従来どおり exit 5 になる。
/// 窓 2000ms の根拠: 相手側 one-shot が応答タイムアウト (既定 2000ms) いっぱい
/// 専有し続ける最悪ケースを 1 回分は跨げる長さ。
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(30);
const BIND_RETRY_WINDOW: Duration = Duration::from_millis(2000);

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
        .truncate(false)
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

/// SO_REUSEADDR 付きで UDP ソケットを bind する。listen (224.0.23.0:3610) と
/// one-shot (0.0.0.0:3610) の共存には双方の REUSEADDR が必要 (実機検証
/// 2026-07-16)。socket2 はこの 1 点のためだけに使う。
fn bind_reuse(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::V4(addr).into())?;
    Ok(socket.into())
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
        match bind_reuse(addr) {
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

/// INF 通知の待受用に 224.0.23.0 へ join する。
/// iface 省略時は OS 既定のインタフェースで join する。
fn join_multicast(socket: &UdpSocket, iface: Option<Ipv4Addr>) -> Result<(), AppError> {
    let iface = iface.unwrap_or(Ipv4Addr::UNSPECIFIED);
    socket
        .join_multicast_v4(&MULTICAST_ADDR, &iface)
        .map_err(|e| {
            AppError::new(
                ErrKind::Network,
                format!("{MULTICAST_ADDR} への join_multicast 失敗 (iface {iface}): {e}"),
            )
        })
}

/// 受信した 1 データグラム。
#[derive(Debug)]
pub struct Datagram {
    pub from: SocketAddr,
    pub data: Vec<u8>,
}

/// 1 データグラムを deadline まで待つ (listen 用)。deadline 到達は Ok(None)。
/// deadline が None なら無期限にブロックする。
pub fn recv_one(
    socket: &UdpSocket,
    deadline: Option<Instant>,
) -> Result<Option<Datagram>, AppError> {
    let timeout = match deadline {
        Some(d) => match d.checked_duration_since(Instant::now()) {
            Some(r) if !r.is_zero() => Some(r),
            _ => return Ok(None),
        },
        None => None,
    };
    socket
        .set_read_timeout(timeout)
        .map_err(|e| AppError::new(ErrKind::Network, format!("set_read_timeout 失敗: {e}")))?;
    let mut buf = [0u8; 1500];
    match socket.recv_from(&mut buf) {
        Ok((n, from)) => Ok(Some(Datagram {
            from,
            data: buf[..n].to_vec(),
        })),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            Ok(None)
        }
        Err(e) => Err(AppError::new(ErrKind::Network, format!("受信失敗: {e}"))),
    }
}

/// `window` の間 recv を集める (sweep discovery 用)。
/// 送信は呼び出し側で複数 send_to を済ませる前提。
pub fn collect_until(socket: &UdpSocket, window: Duration) -> Result<Vec<Datagram>, AppError> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    while let Some(dg) = recv_one(socket, Some(deadline))? {
        out.push(dg);
    }
    Ok(out)
}

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

    let deadline = Instant::now() + timeout;
    // 期待送信元以外や EHD/TID 不一致 (他機器・他コントローラのフレーム) は読み飛ばす。
    while let Some(dg) = recv_one(socket, Some(deadline))? {
        if dg.from.ip() == expect && is_reply_candidate(&dg.data, tid) {
            return Ok(dg);
        }
        tracing::debug!(from = %dg.from, len = dg.data.len(), "不一致フレームをスキップ (送信元/EHD/TID)");
    }
    Err(AppError::new(
        ErrKind::Timeout,
        format!("{expect} からの応答なし ({}ms)", timeout.as_millis()),
    ))
}

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
        let sock =
            bind_with_retry(addr, Duration::from_millis(2000), Duration::from_millis(10)).unwrap();
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
        let err = bind_with_retry(addr, Duration::from_millis(120), Duration::from_millis(10))
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
        let err = bind_with_retry(addr, Duration::from_millis(2000), Duration::from_millis(10))
            .unwrap_err();
        assert_eq!(err.kind, crate::error::ErrKind::Bind);
        assert!(!err.detail.contains("解放されず"), "detail={}", err.detail);
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn ephemeral_socket_binds_off_3610() {
        let s = open_ephemeral_socket().unwrap();
        let port = s.local_addr().unwrap().port();
        assert_ne!(port, 0);
        assert_ne!(port, ECHONET_PORT);
    }

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
}
