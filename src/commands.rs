//! 各サブコマンドの実装。出力は純粋な構造化 JSON (stdout)。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        "--cidr <CIDR> もしくは -i <IPv4> のいずれかが必要 (例: --cidr 192.0.2.0/24)",
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
pub fn get(
    ip: IpAddr,
    eoj: Eoj,
    epcs: &[u8],
    timeout: Duration,
    multicast: bool,
) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let props: Vec<Property> = epcs.iter().map(|&e| Property::get(e)).collect();
    let tid = next_tid();
    let frame = Frame::standard(tid, CONTROLLER, eoj, Esv::Get, props);
    let dst = dst_for(ip, multicast);
    tracing::info!(%ip, eoj = eoj.to_hex(), transport = transport_name(multicast), "get 送信");

    let dg = net::send_and_recv_one(&socket, dst, ip, tid, &codec::build(&frame), timeout)?;
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
    multicast: bool,
) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let tid = next_tid();
    let frame = Frame::standard(
        tid,
        CONTROLLER,
        eoj,
        Esv::SetC,
        vec![Property::new(epc, edt)],
    );
    let dst = dst_for(ip, multicast);
    tracing::info!(%ip, eoj = eoj.to_hex(), epc = format!("{epc:02X}"), transport = transport_name(multicast), "set 送信");

    let dg = net::send_and_recv_one(&socket, dst, ip, tid, &codec::build(&frame), timeout)?;
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
pub fn describe(
    ip: IpAddr,
    eoj: Eoj,
    timeout: Duration,
    multicast: bool,
) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let tid = next_tid();
    let frame = Frame::standard(
        tid,
        CONTROLLER,
        eoj,
        Esv::Get,
        vec![
            Property::get(EPC_GET_MAP),
            Property::get(EPC_SET_MAP),
            Property::get(EPC_INF_MAP),
        ],
    );
    let dst = dst_for(ip, multicast);
    tracing::info!(%ip, eoj = eoj.to_hex(), transport = transport_name(multicast), "describe 送信");

    let dg = net::send_and_recv_one(&socket, dst, ip, tid, &codec::build(&frame), timeout)?;
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
                out[key] = json!(epcs
                    .iter()
                    .map(|&e| {
                        let mut o = json!({ "epc": format!("{e:02X}") });
                        if let Some(name) = properties::epc_name(eoj, e) {
                            o["name"] = json!(name);
                        }
                        if let Some(values) = properties::epc_values(eoj, e) {
                            o["values"] = values;
                        }
                        o
                    })
                    .collect::<Vec<_>>());
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
    multicast: bool,
) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let seoj = seoj.unwrap_or(CONTROLLER);
    let tid = next_tid();
    let frame = Frame::standard(tid, seoj, deoj, esv, props);
    let sent = codec::build(&frame);
    let dst = dst_for(ip, multicast);
    tracing::info!(%ip, deoj = deoj.to_hex(), esv = esv.name(), transport = transport_name(multicast), "raw 送信");

    let dg = net::send_and_recv_one(&socket, dst, ip, tid, &sent, timeout)?;

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

/// listen の SEOJ フィルタ。4 hex 桁ならクラス一致、6 hex 桁なら完全一致。
pub enum EojFilter {
    Class(u8, u8),
    Exact(Eoj),
}

impl EojFilter {
    /// "0291" (クラス) もしくは "029101" (完全一致) から生成。
    pub fn from_hex(s: &str) -> Result<EojFilter, AppError> {
        let bytes = codec::hex_to_bytes(s)
            .map_err(|e| AppError::new(ErrKind::Internal, format!("EOJ フィルタ hex 不正: {e}")))?;
        match bytes.len() {
            2 => Ok(EojFilter::Class(bytes[0], bytes[1])),
            3 => Ok(EojFilter::Exact(Eoj([bytes[0], bytes[1], bytes[2]]))),
            _ => Err(AppError::new(
                ErrKind::Internal,
                "EOJ フィルタは 4 hex 桁 (クラス) か 6 hex 桁 (完全一致)",
            )),
        }
    }

    fn matches(&self, eoj: Eoj) -> bool {
        match self {
            EojFilter::Class(g, c) => eoj.class_group() == *g && eoj.class() == *c,
            EojFilter::Exact(e) => eoj == *e,
        }
    }
}

/// INF / INFC 通知を待ち受けて収集する (one-shot: count 件か deadline で終了)。
///
/// 3610 を bind し 224.0.23.0 に join して INF (0x73) / INFC (0x74) のみ採用する。
/// INFC には仕様上の応答 (INFC_Res) を best-effort で返す。
/// deadline までに 1 件も来なければ timeout (exit 3)、1 件以上あれば成功。
pub fn listen(
    iface: Option<Ipv4Addr>,
    count: usize,
    timeout: Option<Duration>,
    from: Option<IpAddr>,
    eoj_filter: Option<EojFilter>,
    epc: Option<u8>,
) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    net::join_multicast(&socket, iface)?;
    let deadline = timeout.map(|t| Instant::now() + t);
    tracing::info!(
        count,
        timeout_ms = timeout.map(|t| t.as_millis() as u64),
        "INF 待受開始"
    );

    let mut events = Vec::new();
    while events.len() < count {
        let dg = match net::recv_one(&socket, deadline)? {
            Some(dg) => dg,
            None => break, // deadline 到達
        };
        let frame = match codec::parse(&dg.data) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(from = %dg.from, error = %e, "パース不能フレームをスキップ");
                continue;
            }
        };
        match inf_event(
            dg.from.ip(),
            &frame,
            from.as_ref(),
            eoj_filter.as_ref(),
            epc,
        ) {
            Some(event) => {
                tracing::info!(from = %dg.from, "INF 受信");
                reply_infc_res(&socket, dg.from.ip(), &frame);
                events.push(event);
            }
            None => {
                tracing::debug!(from = %dg.from, "対象外フレームをスキップ");
            }
        }
    }

    if events.is_empty() {
        let ms = timeout.map(|t| t.as_millis()).unwrap_or(0);
        return Err(AppError::new(
            ErrKind::Timeout,
            format!("INF 通知なし ({ms}ms)"),
        ));
    }
    Ok(json!({ "events": events }))
}

/// 受信フレームが採用すべき INF / INFC 通知なら event JSON にする。
/// 非通知 ESV・フィルタ不一致は None。
fn inf_event(
    src: IpAddr,
    frame: &Frame,
    from: Option<&IpAddr>,
    eoj_filter: Option<&EojFilter>,
    epc: Option<u8>,
) -> Option<Value> {
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
    if from.is_some_and(|ip| *ip != src) {
        return None;
    }
    if eoj_filter.is_some_and(|f| !f.matches(seoj)) {
        return None;
    }
    if epc.is_some_and(|e| !props.iter().any(|p| p.epc == e)) {
        return None;
    }
    let properties: Vec<Value> = props
        .iter()
        .map(|p| property_json(seoj, p.epc, &p.edt))
        .collect();
    Some(json!({
        "ip": src.to_string(),
        "tid": format!("{:04x}", frame.tid),
        "seoj": seoj.to_hex(),
        "deoj": deoj.to_hex(),
        "esv": esv.name(),
        "properties": properties,
    }))
}

/// INFC (0x74) は応答必須なので INFC_Res を返す。失敗しても収集は止めない。
fn reply_infc_res(socket: &std::net::UdpSocket, src: IpAddr, frame: &Frame) {
    let (seoj, esv, props) = match &frame.edata {
        Edata::Standard {
            seoj, esv, props, ..
        } => (*seoj, *esv, props),
        _ => return,
    };
    if esv != Esv::InfC {
        return;
    }
    let res = Frame::standard(
        frame.tid,
        CONTROLLER,
        seoj,
        Esv::InfCRes,
        props.iter().map(|p| Property::get(p.epc)).collect(),
    );
    let dst = SocketAddr::new(src, net::ECHONET_PORT);
    if let Err(e) = socket.send_to(&codec::build(&res), dst) {
        tracing::warn!(%src, error = %e, "INFC_Res 送信失敗 (continue)");
    }
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
    fn dst_for_unicast_and_multicast() {
        let ip: IpAddr = "192.0.2.22".parse().unwrap();
        assert_eq!(dst_for(ip, false).to_string(), "192.0.2.22:3610");
        assert_eq!(dst_for(ip, true).to_string(), "224.0.23.0:3610");
    }

    #[test]
    fn parse_cidr_basic() {
        assert_eq!(
            parse_cidr("192.0.2.0/24").unwrap(),
            (Ipv4Addr::new(192, 0, 2, 0), 24)
        );
        assert_eq!(
            parse_cidr("10.0.0.0/8").unwrap(),
            (Ipv4Addr::new(10, 0, 0, 0), 8)
        );
    }

    #[test]
    fn parse_cidr_errors() {
        assert!(parse_cidr("192.0.2.0").is_err());
        assert!(parse_cidr("192.0.2.0/33").is_err());
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

    #[test]
    fn eoj_filter_class_and_exact() {
        let light = Eoj([0x02, 0x91, 0x01]);
        let class = EojFilter::from_hex("0291").unwrap();
        assert!(class.matches(light));
        assert!(class.matches(Eoj([0x02, 0x91, 0x02]))); // 別インスタンスも一致
        assert!(!class.matches(Eoj([0x01, 0x30, 0x01])));

        let exact = EojFilter::from_hex("029101").unwrap();
        assert!(exact.matches(light));
        assert!(!exact.matches(Eoj([0x02, 0x91, 0x02])));

        assert!(EojFilter::from_hex("02").is_err());
        assert!(EojFilter::from_hex("zz").is_err());
    }

    /// 単機能照明 (029101) からの INF 0x80=OFF 通知フレーム。
    fn inf_frame(esv: Esv) -> Frame {
        Frame::standard(
            0x00AB,
            Eoj([0x02, 0x91, 0x01]),
            NODE_PROFILE,
            esv,
            vec![Property::new(0x80, vec![0x31])],
        )
    }

    #[test]
    fn inf_event_accepts_inf_and_decodes() {
        let src: IpAddr = "192.0.2.20".parse().unwrap();
        let ev = inf_event(src, &inf_frame(Esv::Inf), None, None, None).unwrap();
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
        assert!(inf_event(src, &inf_frame(Esv::GetRes), None, None, None).is_none());
        assert!(inf_event(src, &inf_frame(Esv::Get), None, None, None).is_none());
    }

    #[test]
    fn inf_event_filters() {
        let src: IpAddr = "192.0.2.20".parse().unwrap();
        let other: IpAddr = "192.0.2.99".parse().unwrap();
        let f = inf_frame(Esv::Inf);
        // from フィルタ
        assert!(inf_event(src, &f, Some(&src), None, None).is_some());
        assert!(inf_event(src, &f, Some(&other), None, None).is_none());
        // eoj フィルタ (クラス / 完全一致 / 不一致)
        let class = EojFilter::from_hex("0291").unwrap();
        let miss = EojFilter::from_hex("013001").unwrap();
        assert!(inf_event(src, &f, None, Some(&class), None).is_some());
        assert!(inf_event(src, &f, None, Some(&miss), None).is_none());
        // epc フィルタ
        assert!(inf_event(src, &f, None, None, Some(0x80)).is_some());
        assert!(inf_event(src, &f, None, None, Some(0xB0)).is_none());
    }
}
