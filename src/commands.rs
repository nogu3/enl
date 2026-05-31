//! 各サブコマンドの実装。出力は純粋な構造化 JSON (stdout)。

use std::net::{IpAddr, SocketAddr};
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

/// マルチキャストで Get 0xD6 を投げ、応答ノードを集約する。
pub fn discover(window: Duration) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let frame = Frame::standard(
        next_tid(),
        CONTROLLER,
        NODE_PROFILE,
        Esv::Get,
        vec![Property::get(EPC_INSTANCE_LIST)],
    );
    let dst = SocketAddr::new(IpAddr::V4(net::MULTICAST_ADDR), net::ECHONET_PORT);
    tracing::info!(?dst, window_ms = window.as_millis(), "discovery 送信");

    let datagrams = net::send_and_collect(&socket, dst, &codec::build(&frame), window)?;

    let mut devices = Vec::new();
    for dg in datagrams {
        let frame = match codec::parse(&dg.data) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(from = %dg.from, error = %e, "応答パース失敗、スキップ");
                continue;
            }
        };
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
        devices.push(device);
    }
    tracing::info!(devices = devices.len(), "discovery 完了");
    Ok(json!({ "devices": devices }))
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
    let properties: Vec<Value> = props.iter().map(|p| property_json(eoj, p.epc, &p.edt)).collect();

    Ok(json!({
        "ip": ip.to_string(),
        "eoj": eoj.to_hex(),
        "esv": esv.name(),
        "properties": properties,
    }))
}

/// IP / EOJ / EPC / EDT を指定して Set (SetC = 応答要求)。
pub fn set(ip: IpAddr, eoj: Eoj, epc: u8, edt: Vec<u8>, timeout: Duration) -> Result<Value, AppError> {
    let socket = net::open_socket()?;
    let frame = Frame::standard(next_tid(), CONTROLLER, eoj, Esv::SetC, vec![Property::new(epc, edt)]);
    let dst = SocketAddr::new(ip, net::ECHONET_PORT);
    tracing::info!(%ip, eoj = eoj.to_hex(), epc = format!("{epc:02X}"), "set 送信");

    let dg = net::send_and_recv_one(&socket, dst, &codec::build(&frame), timeout)?;
    let resp = parse_response(&dg.data)?;
    let (esv, props) = standard_or_reject(&resp, eoj)?;

    // Set_Res は EDT 無し (PDC=0) のプロパティが返る。
    let properties: Vec<Value> = props.iter().map(|p| property_json(eoj, p.epc, &p.edt)).collect();
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
                let rejected: Vec<String> = props.iter().map(|p| format!("{:02X}", p.epc)).collect();
                return Err(AppError::new(
                    ErrKind::DeviceRejected,
                    format!("機器が {} を拒否 (SNA)", esv.name()),
                )
                .with_extra(json!({ "eoj": eoj.to_hex(), "esv": esv.name(), "rejected_epc": rejected })));
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
