//! UDP ソケット層。ステートレス / one-shot。
//!
//! 最重要: 仕様準拠機器は応答を送信元ポートでなく 3610 に返す。
//! よって送受信ソケットを 0.0.0.0:3610 にバインドして専有する。
//!
//! discover は CIDR sweep (各ホストへ unicast Get) と multicast (224.0.23.0) の
//! 常時併用。multicast は ECHONET Lite 標準の探索方式で、CIDR 不明でも引数なしで
//! 探索できる。multicast の egress インタフェースは制御しない
//! (ルーティングテーブル任せ)。制御には socket2 等の依存追加が要るため、
//! 依存ゼロ方針を優先した既知の制約 (実需が出たら -i 連動で追加する)。

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

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

/// 3610 を専有する UDP ソケットを開く。バインド失敗は bind エラー (exit 5)。
pub fn open_socket() -> Result<UdpSocket, AppError> {
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, ECHONET_PORT);
    UdpSocket::bind(bind_addr).map_err(|e| {
        AppError::new(
            ErrKind::Bind,
            format!(
                "0.0.0.0:{ECHONET_PORT} へのバインド失敗: {e}。HA 等が 3610 を専有していないか確認"
            ),
        )
    })
}

/// INF 通知の待受用に 224.0.23.0 へ join する。
/// iface 省略時は OS 既定のインタフェースで join する。
pub fn join_multicast(socket: &UdpSocket, iface: Option<Ipv4Addr>) -> Result<(), AppError> {
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
    let mut out = Vec::new();
    let deadline = Instant::now() + window;
    let mut buf = [0u8; 1500];
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => break,
        };
        socket
            .set_read_timeout(Some(remaining))
            .map_err(|e| AppError::new(ErrKind::Network, format!("set_read_timeout 失敗: {e}")))?;
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => out.push(Datagram {
                from,
                data: buf[..n].to_vec(),
            }),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                break
            }
            Err(e) => return Err(AppError::new(ErrKind::Network, format!("受信失敗: {e}"))),
        }
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
}
