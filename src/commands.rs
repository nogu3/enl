//! 各サブコマンドの実装。出力は純粋な構造化 JSON (stdout)。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::codec::{self, Edata, Eoj, Esv, Frame, Property};
use crate::error::{AppError, ErrKind};
use crate::net;
use crate::properties::{self, property_json};

/// コントローラ自身の SEOJ。
const CONTROLLER: Eoj = Eoj([0x05, 0xFF, 0x01]);
/// ノードプロファイルオブジェクト。
const NODE_PROFILE: Eoj = Eoj([0x0E, 0xF0, 0x01]);
/// 自ノードインスタンスリスト EPC。
const EPC_INSTANCE_LIST: u8 = 0xD6;
const EPC_GET_MAP: u8 = 0x9F;
const EPC_SET_MAP: u8 = 0x9E;
const EPC_INF_MAP: u8 = 0x9D;

/// TID 生成 (one-shot なので時刻下位 16bit で十分)。
fn next_tid() -> u16 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u16)
        .unwrap_or(1)
}

/// CIDR sweep ベースの discovery。
///
/// 指定 CIDR (or iface IP の /24) 内の全ホストへ unicast `Get 0EF001 D6` を送り、
/// `window` の間に返ってきたノードを集約する。
/// multicast (224.0.23.0) は WiFi/AP 環境で取りこぼしが多いため使わない。
pub fn discover(
    cidr: Option<&str>,
    iface: Option<Ipv4Addr>,
    window: Duration,
) -> Result<Value, AppError> {
    let (base, prefix) = resolve_cidr(cidr, iface)?;
    let hosts = enumerate_hosts(base, prefix);
    if hosts.is_empty() {
        return Err(AppError::new(
            ErrKind::Internal,
            format!("CIDR {}/{} に有効ホストなし", base, prefix),
        ));
    }
    let socket = net::open_socket()?;
    let frame = Frame::standard(
        next_tid(),
        CONTROLLER,
        NODE_PROFILE,
        Esv::Get,
        vec![Property::get(EPC_INSTANCE_LIST)],
    );
    let payload = codec::build(&frame);

    tracing::info!(
        base = %base,
        prefix,
        hosts = hosts.len(),
        window_ms = window.as_millis(),
        "sweep discovery 送信"
    );

    for h in &hosts {
        let dst = SocketAddr::new(IpAddr::V4(*h), net::ECHONET_PORT);
        if let Err(e) = socket.send_to(&payload, dst) {
            tracing::debug!(ip = %h, error = %e, "send_to 失敗 (continue)");
        }
    }

    let datagrams = net::collect_until(&socket, window)?;

    // 応答 (ESV=0x7X) のみ採用。リクエストフレーム (自身の 3610 への送信が
    // loopback で戻ってきたもの、他コントローラのトラフィック等) は無視する。
    // 同一 IP から複数応答が来た場合は最初の正常パースを採用。
    let mut by_ip: HashMap<IpAddr, Value> = HashMap::new();
    for dg in datagrams {
        if by_ip.contains_key(&dg.from.ip()) {
            continue;
        }
        let frame = match codec::parse(&dg.data) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(from = %dg.from, error = %e, "応答パース失敗、スキップ");
                continue;
            }
        };
        let esv = match &frame.edata {
            Edata::Standard { esv, .. } | Edata::SetGet { esv, .. } => *esv,
            Edata::Arbitrary(_) => continue,
        };
        if !esv.is_response() {
            tracing::debug!(from = %dg.from, esv = esv.name(), "非応答フレームをスキップ");
            continue;
        }
        let mut device = json!({ "ip": dg.from.ip().to_string() });
        if let Some(props) = frame.props() {
            for p in props {
                if p.epc == EPC_INSTANCE_LIST {
                    if let Some(v) = properties::decode(NODE_PROFILE, p.epc, &p.edt) {
                        device["instances"] = v["instances"].clone();
                        device["count"] = v["count"].clone();
                    }
                }
            }
        }
        by_ip.insert(dg.from.ip(), device);
    }

    let mut devices: Vec<Value> = by_ip.into_values().collect();
    devices.sort_by(|a, b| a["ip"].as_str().cmp(&b["ip"].as_str()));
    tracing::info!(devices = devices.len(), "discovery 完了");
    Ok(json!({ "devices": devices }))
}

/// `--cidr` 優先、無ければ iface IP から /24 を推定。両方無ければエラー。
fn resolve_cidr(cidr: Option<&str>, iface: Option<Ipv4Addr>) -> Result<(Ipv4Addr, u8), AppError> {
    if let Some(s) = cidr {
        return parse_cidr(s)
            .map_err(|e| AppError::new(ErrKind::Internal, format!("CIDR 不正 '{s}': {e}")));
    }
    if let Some(ip) = iface {
        let oct = ip.octets();
        return Ok((Ipv4Addr::new(oct[0], oct[1], oct[2], 0), 24));
    }
    Err(AppError::new(
        ErrKind::Internal,
        "--cidr <CIDR> もしくは -i <IPv4> のいずれかが必要 (例: --cidr 192.168.1.0/24)",
    ))
}

fn parse_cidr(s: &str) -> Result<(Ipv4Addr, u8), String> {
    let (addr, prefix) = s.split_once('/').ok_or_else(|| "'/' 無し".to_string())?;
    let addr: Ipv4Addr = addr.parse().map_err(|e| format!("IP: {e}"))?;
    let prefix: u8 = prefix.parse().map_err(|e| format!("prefix: {e}"))?;
    if prefix > 32 {
        return Err(format!("prefix {prefix} > 32"));
    }
    Ok((addr, prefix))
}

/// CIDR 内の探索対象ホスト IPv4 を列挙する。
/// /31, /32 はネットワーク/ブロードキャストの概念を使わず全 IP を返す。
/// それ以外はネットワーク・ブロードキャストアドレスを除外する。
fn enumerate_hosts(net: Ipv4Addr, prefix: u8) -> Vec<Ipv4Addr> {
    let mask: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let base = u32::from_be_bytes(net.octets()) & mask;
    let bcast = base | !mask;
    let range = if prefix >= 31 {
        base..=bcast
    } else {
        (base + 1)..=(bcast - 1)
    };
    range.map(|n| Ipv4Addr::from(n.to_be_bytes())).collect()
}

/// IP / EOJ / EPC を指定して Get。
pub fn get(ip: IpAddr, eoj: Eoj, epcs: &[u8], timeout: Duration) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let props: Vec<Property> = epcs.iter().map(|&e| Property::get(e)).collect();
    let frame = Frame::standard(next_tid(), CONTROLLER, eoj, Esv::Get, props);
    let dst = SocketAddr::new(ip, net::ECHONET_PORT);
    tracing::info!(%ip, eoj = eoj.to_hex(), "get 送信");

    let dg = net::send_and_recv_one(&socket, dst, &codec::build(&frame), timeout)?;
    let resp = parse_response(&dg.data)?;

    let (esv, props) = standard_or_reject(&resp, eoj)?;
    let properties: Vec<Value> = props
        .iter()
        .map(|p| property_json(eoj, p.epc, &p.edt))
        .collect();

    Ok(json!({
        "ip": ip.to_string(),
        "eoj": eoj.to_hex(),
        "esv": esv.name(),
        "properties": properties,
    }))
}

/// IP / EOJ / EPC / EDT を指定して Set (SetC = 応答要求)。
pub fn set(
    ip: IpAddr,
    eoj: Eoj,
    epc: u8,
    edt: Vec<u8>,
    timeout: Duration,
) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let frame = Frame::standard(
        next_tid(),
        CONTROLLER,
        eoj,
        Esv::SetC,
        vec![Property::new(epc, edt)],
    );
    let dst = SocketAddr::new(ip, net::ECHONET_PORT);
    tracing::info!(%ip, eoj = eoj.to_hex(), epc = format!("{epc:02X}"), "set 送信");

    let dg = net::send_and_recv_one(&socket, dst, &codec::build(&frame), timeout)?;
    let resp = parse_response(&dg.data)?;
    let (esv, props) = standard_or_reject(&resp, eoj)?;

    // Set_Res は EDT 無し (PDC=0) のプロパティが返る。
    let properties: Vec<Value> = props
        .iter()
        .map(|p| property_json(eoj, p.epc, &p.edt))
        .collect();
    Ok(json!({
        "ip": ip.to_string(),
        "eoj": eoj.to_hex(),
        "esv": esv.name(),
        "result": "accepted",
        "properties": properties,
    }))
}

/// プロパティマップ introspection。Get/Set/状変マップを引く。
pub fn describe(ip: IpAddr, eoj: Eoj, timeout: Duration) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let frame = Frame::standard(
        next_tid(),
        CONTROLLER,
        eoj,
        Esv::Get,
        vec![
            Property::get(EPC_GET_MAP),
            Property::get(EPC_SET_MAP),
            Property::get(EPC_INF_MAP),
        ],
    );
    let dst = SocketAddr::new(ip, net::ECHONET_PORT);
    tracing::info!(%ip, eoj = eoj.to_hex(), "describe 送信");

    let dg = net::send_and_recv_one(&socket, dst, &codec::build(&frame), timeout)?;
    let resp = parse_response(&dg.data)?;
    let (esv, props) = standard_or_reject(&resp, eoj)?;

    let mut out = json!({ "ip": ip.to_string(), "eoj": eoj.to_hex(), "esv": esv.name() });
    for p in &props {
        let key = match p.epc {
            EPC_GET_MAP => "get_map",
            EPC_SET_MAP => "set_map",
            EPC_INF_MAP => "inf_map",
            _ => continue,
        };
        match properties::parse_property_map(&p.edt) {
            Some(epcs) => {
                out[key] = json!(epcs.iter().map(|e| format!("{e:02X}")).collect::<Vec<_>>());
            }
            // 壊れた / 空マップでも生 hex は残す。
            None => out[key] = json!({ "edt_hex": codec::bytes_to_hex(&p.edt) }),
        }
    }
    Ok(out)
}

/// 任意 ESV/EPC/EDT を生送信し生応答 hex を返す (デバッグ / 未対応操作の逃げ道)。
///
/// SNA も含め「応答が返れば成功」とし response_hex を必ず出す。
/// 応答パースは best-effort で、できれば frame に併記、ダメでも壊れない。
/// 応答が来ない場合のみ timeout (exit 3)。
pub fn raw(
    ip: IpAddr,
    deoj: Eoj,
    esv: Esv,
    seoj: Option<Eoj>,
    props: Vec<Property>,
    timeout: Duration,
) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let seoj = seoj.unwrap_or(CONTROLLER);
    let frame = Frame::standard(next_tid(), seoj, deoj, esv, props);
    let sent = codec::build(&frame);
    let dst = SocketAddr::new(ip, net::ECHONET_PORT);
    tracing::info!(%ip, deoj = deoj.to_hex(), esv = esv.name(), "raw 送信");

    let dg = net::send_and_recv_one(&socket, dst, &sent, timeout)?;

    let mut out = json!({
        "ip": ip.to_string(),
        "sent_hex": codec::bytes_to_hex(&sent),
        "response_hex": codec::bytes_to_hex(&dg.data),
    });
    // パースは付加情報。失敗しても response_hex があるので壊れない。
    match codec::parse(&dg.data) {
        Ok(frame) => out["frame"] = frame_to_json(&frame),
        Err(e) => {
            tracing::warn!(error = %e, "raw 応答パース失敗 (response_hex は出力済み)");
            out["parse_error"] = json!(e.to_string());
        }
    }
    Ok(out)
}

/// Frame をロスレスに JSON へダンプ (raw 用)。EDT デコードは応答 SEOJ 基準で best-effort。
fn frame_to_json(frame: &Frame) -> Value {
    let props_json = |seoj: Eoj, props: &[Property]| -> Vec<Value> {
        props
            .iter()
            .map(|p| property_json(seoj, p.epc, &p.edt))
            .collect()
    };
    let mut v = json!({
        "ehd2": format!("{:02x}", frame.ehd2),
        "tid": format!("{:04x}", frame.tid),
    });
    match &frame.edata {
        Edata::Standard {
            seoj,
            deoj,
            esv,
            props,
        } => {
            v["format"] = json!("standard");
            v["seoj"] = json!(seoj.to_hex());
            v["deoj"] = json!(deoj.to_hex());
            v["esv"] = json!(esv.name());
            v["properties"] = json!(props_json(*seoj, props));
        }
        Edata::SetGet {
            seoj,
            deoj,
            esv,
            set_props,
            get_props,
        } => {
            v["format"] = json!("setget");
            v["seoj"] = json!(seoj.to_hex());
            v["deoj"] = json!(deoj.to_hex());
            v["esv"] = json!(esv.name());
            v["set_properties"] = json!(props_json(*seoj, set_props));
            v["get_properties"] = json!(props_json(*seoj, get_props));
        }
        Edata::Arbitrary(bytes) => {
            v["format"] = json!("arbitrary");
            v["edt_hex"] = json!(codec::bytes_to_hex(bytes));
        }
    }
    v
}

/// 受信バイトを Frame にパース (失敗は parse エラー)。
fn parse_response(data: &[u8]) -> Result<Frame, AppError> {
    codec::parse(data).map_err(|e| {
        AppError::new(ErrKind::Parse, format!("応答パース失敗: {e}"))
            .with_extra(json!({ "raw_hex": codec::bytes_to_hex(data) }))
    })
}

/// Standard フレームならプロパティを返す。SNA なら device_rejected (exit 4)。
fn standard_or_reject(frame: &Frame, eoj: Eoj) -> Result<(Esv, Vec<Property>), AppError> {
    match &frame.edata {
        Edata::Standard { esv, props, .. } => {
            if esv.is_sna() {
                let rejected: Vec<String> =
                    props.iter().map(|p| format!("{:02X}", p.epc)).collect();
                return Err(AppError::new(
                    ErrKind::DeviceRejected,
                    format!("機器が {} を拒否 (SNA)", esv.name()),
                )
                .with_extra(
                    json!({ "eoj": eoj.to_hex(), "esv": esv.name(), "rejected_epc": rejected }),
                ));
            }
            Ok((*esv, props.clone()))
        }
        Edata::SetGet { esv, .. } => {
            if esv.is_sna() {
                return Err(AppError::new(
                    ErrKind::DeviceRejected,
                    format!("機器が {} を拒否 (SNA)", esv.name()),
                ));
            }
            // SETGET 応答は本ツールでは未送出だが、来たら parse エラーで明示。
            Err(AppError::new(ErrKind::Parse, "想定外の SETGET 応答"))
        }
        Edata::Arbitrary(bytes) => Err(AppError::new(
            ErrKind::Parse,
            "任意電文形式 (EHD2=0x82) の応答は解釈不能",
        )
        .with_extra(json!({ "raw_hex": codec::bytes_to_hex(bytes) }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cidr_basic() {
        assert_eq!(
            parse_cidr("192.168.1.0/24").unwrap(),
            (Ipv4Addr::new(192, 168, 1, 0), 24)
        );
        assert_eq!(
            parse_cidr("10.0.0.0/8").unwrap(),
            (Ipv4Addr::new(10, 0, 0, 0), 8)
        );
    }

    #[test]
    fn parse_cidr_errors() {
        assert!(parse_cidr("192.168.1.0").is_err());
        assert!(parse_cidr("192.168.1.0/33").is_err());
        assert!(parse_cidr("nope/24").is_err());
    }

    #[test]
    fn enumerate_hosts_slash24() {
        let hosts = enumerate_hosts(Ipv4Addr::new(192, 168, 1, 0), 24);
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts.first(), Some(&Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(hosts.last(), Some(&Ipv4Addr::new(192, 168, 1, 254)));
    }

    #[test]
    fn enumerate_hosts_slash32() {
        let hosts = enumerate_hosts(Ipv4Addr::new(192, 168, 1, 42), 32);
        assert_eq!(hosts, vec![Ipv4Addr::new(192, 168, 1, 42)]);
    }

    #[test]
    fn enumerate_hosts_slash30() {
        // base=.0, bcast=.3 → host=.1, .2
        let hosts = enumerate_hosts(Ipv4Addr::new(10, 0, 0, 0), 30);
        assert_eq!(
            hosts,
            vec![Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2)]
        );
    }

    #[test]
    fn resolve_cidr_from_iface() {
        let (base, prefix) = resolve_cidr(None, Some(Ipv4Addr::new(192, 168, 1, 130))).unwrap();
        assert_eq!(base, Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(prefix, 24);
    }

    #[test]
    fn resolve_cidr_explicit_wins() {
        let (base, prefix) =
            resolve_cidr(Some("10.0.0.0/16"), Some(Ipv4Addr::new(192, 168, 1, 130))).unwrap();
        assert_eq!(base, Ipv4Addr::new(10, 0, 0, 0));
        assert_eq!(prefix, 16);
    }

    #[test]
    fn resolve_cidr_neither_errors() {
        assert!(resolve_cidr(None, None).is_err());
    }
}
