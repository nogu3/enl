//! UDP ソケット層。ステートレス / one-shot。
//!
//! 最重要: 仕様準拠機器は応答を送信元ポートでなく 3610 に返す。
//! よって送受信ソケットを 0.0.0.0:3610 にバインドして専有する。
//!
//! discover は CIDR sweep (各ホストへ unicast Get) 方式。multicast は
//! WiFi/AP 環境 (IGMP snooping, multicast 抑制) で信頼性が低いため採用しない。

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use crate::error::{AppError, ErrKind};

pub const ECHONET_PORT: u16 = 3610;
/// ECHONET Lite のマルチキャストアドレス。INF 通知はここへ送られる。
pub const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 23, 0);

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

/// フレームを送り、最初の 1 応答を待つ (get/set 用)。
/// タイムアウト内に応答が無ければ timeout エラー (exit 3)。
pub fn send_and_recv_one(
    socket: &UdpSocket,
    dst: SocketAddr,
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
        // 自分宛て以外 (他機器の無関係フレーム) は読み飛ばす。
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if from.ip() == dst.ip() {
                    return Ok(Datagram {
                        from,
                        data: buf[..n].to_vec(),
                    });
                }
                // 宛先以外。残り時間で再試行。
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
        format!("{} からの応答なし ({}ms)", dst.ip(), timeout.as_millis()),
    ))
}
