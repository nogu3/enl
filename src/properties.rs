//! プロパティ定義テーブル駆動の任意デコードレイヤ。
//!
//! 方針: 辞書にあれば人間可読値を併記、なければ生 hex のまま。
//! 未知 EPC で壊れない。codec コア(ダンプ層)とは分離する。

use crate::codec::{bytes_to_hex, Eoj};
use serde_json::{json, Value};

/// EDT を可能ならデコードして付加情報の JSON 値を返す。
/// デコードできなければ None (呼び出し側は edt_hex のみ出す)。
pub fn decode(eoj: Eoj, epc: u8, edt: &[u8]) -> Option<Value> {
    // スーパークラス共通 EPC (全機器)
    match epc {
        0x80 => return Some(decode_on_off(edt)),
        0x82 => return decode_version(eoj, edt),
        0x8A => return decode_manufacturer(edt),
        0x9D..=0x9F => return parse_property_map(edt).map(
            |m| json!({ "property_map": m.iter().map(|e| format!("{e:02X}")).collect::<Vec<_>>() }),
        ),
        _ => {}
    }
    // ノードプロファイル (0x0EF0xx) 固有
    if eoj.class_group() == 0x0E && eoj.class() == 0xF0 && epc == 0xD6 {
        return decode_instance_list(edt);
    }
    // 電動雨戸・シャッター (0x0263) 固有
    if eoj.class_group() == 0x02 && eoj.class() == 0x63 {
        if let Some(v) = decode_shutter(epc, edt) {
            return Some(v);
        }
    }
    // 家庭用エアコン (0x0130) 固有
    if eoj.class_group() == 0x01 && eoj.class() == 0x30 {
        if let Some(v) = decode_aircon(epc, edt) {
            return Some(v);
        }
    }
    None
}

/// 0x82 規格Version情報。
///
/// 機器オブジェクトスーパークラス: byte1,2=予約(0x00)、byte3=対応 APPENDIX の
/// Release 順を ASCII、byte4=リビジョン番号 (binary)。
/// 例: Release P rev2 → `00 00 50 02` → release "P", revision 2。
/// ノードプロファイル(0x0EF0): byte1=メジャー、byte2=マイナー、
/// byte3=電文形式(0x01 規定/0x02 任意)。
fn decode_version(eoj: Eoj, edt: &[u8]) -> Option<Value> {
    if edt.len() < 4 {
        return None;
    }
    if eoj.class_group() == 0x0E && eoj.class() == 0xF0 {
        let message_format = match edt[2] {
            0x01 => "specified",
            0x02 => "arbitrary",
            _ => "unknown",
        };
        Some(json!({
            "protocol_version": format!("{}.{}", edt[0], edt[1]),
            "message_format": message_format,
        }))
    } else {
        let release = edt[2];
        if release.is_ascii_graphic() {
            Some(json!({
                "release": (release as char).to_string(),
                "revision": edt[3],
            }))
        } else {
            None
        }
    }
}

/// 0x8A メーカコード: 3 バイト (ECHONET コンソーシアム規定)。
/// 常に code を hex で出し、既知メーカは社名を併記する。
fn decode_manufacturer(edt: &[u8]) -> Option<Value> {
    if edt.len() != 3 {
        return None;
    }
    let code = (u32::from(edt[0]) << 16) | (u32::from(edt[1]) << 8) | u32::from(edt[2]);
    let mut obj = json!({ "manufacturer_code": format!("{code:06X}") });
    if let Some(name) = manufacturer_name(code) {
        obj["manufacturer"] = json!(name);
    }
    Some(obj)
}

/// 既知メーカコード → 社名。公式「発行済メーカコード一覧」から主要社を抜粋。
/// 不明なら None (呼び出し側は code hex のみ出す)。
fn manufacturer_name(code: u32) -> Option<&'static str> {
    Some(match code {
        0x000005 => "シャープ",
        0x000006 => "三菱電機",
        0x000008 => "ダイキン工業",
        0x00000B => "パナソニック",
        0x000016 => "東芝",
        0x000022 => "日立グローバルライフソリューションズ",
        0x00003B => "京セラ",
        0x00003C => "デンソー",
        0x00003D => "住友電気工業",
        0x00004E => "富士通",
        0x000054 => "ノーリツ",
        0x000059 => "リンナイ",
        0x000064 => "オムロン ソーシアルソリューションズ",
        0x000067 => "コロナ",
        0x000069 => "東芝ライフスタイル",
        0x00006C => "ニチコン",
        0x0000C5 => "三和シヤッター工業",
        _ => return None,
    })
}

/// 家庭用エアコン (0x0130) 固有 EPC のデコード。
fn decode_aircon(epc: u8, edt: &[u8]) -> Option<Value> {
    match epc {
        // 0xB0 運転モード設定: 自動/冷房/暖房/除湿/送風/その他
        0xB0 => Some(json!({
            "operation_mode": match edt.first() {
                Some(0x41) => "auto",
                Some(0x42) => "cool",
                Some(0x43) => "heat",
                Some(0x44) => "dry",
                Some(0x45) => "fan",
                Some(0x40) => "other",
                _ => "unknown",
            }
        })),
        // 0xB3 温度設定値: unsigned ℃ (0x00-0x32)、0xFD=設定値不明
        0xB3 => edt.first().map(|&v| match v {
            0xFD => json!({ "temp_setpoint_c": Value::Null }),
            _ => json!({ "temp_setpoint_c": v }),
        }),
        // 0xBB 室内温度計測値: signed ℃
        0xBB => edt.first().map(|&v| json!({ "room_temp_c": v as i8 })),
        // 0xA0 風量設定: 自動=0x41 / レベル 0x31-0x38 (1-8)
        0xA0 => match edt.first() {
            Some(0x41) => Some(json!({ "air_flow": "auto" })),
            Some(&v @ 0x31..=0x38) => Some(json!({ "air_flow_level": v - 0x30 })),
            _ => None,
        },
        _ => None,
    }
}

/// 電動雨戸・シャッター (0x0263) 固有 EPC のデコード。
fn decode_shutter(epc: u8, edt: &[u8]) -> Option<Value> {
    match epc {
        // 0xE0 開閉動作設定: 開=0x41 / 閉=0x42 / 停止=0x43
        0xE0 => Some(json!({
            "operation": match edt.first() {
                Some(0x41) => "open",
                Some(0x42) => "close",
                Some(0x43) => "stop",
                _ => "unknown",
            }
        })),
        // 0xE1 開度レベル設定: 0x00-0x64 (0-100%)
        0xE1 => edt.first().map(|&v| json!({ "open_level_percent": v })),
        // 0xEA 開閉状態: 全開/全閉/開動作中/閉動作中/途中停止
        0xEA => Some(json!({
            "state": match edt.first() {
                Some(0x41) => "fully_open",
                Some(0x42) => "fully_closed",
                Some(0x43) => "opening",
                Some(0x44) => "closing",
                Some(0x45) => "stopped_midway",
                _ => "unknown",
            }
        })),
        _ => None,
    }
}

/// 0x80 動作状態: 0x30=ON / 0x31=OFF。
fn decode_on_off(edt: &[u8]) -> Value {
    match edt.first() {
        Some(0x30) => json!({ "power": "on" }),
        Some(0x31) => json!({ "power": "off" }),
        _ => json!({ "power": "unknown" }),
    }
}

/// 0xD6 自ノードインスタンスリスト: count(1) + EOJ(3)×count。
fn decode_instance_list(edt: &[u8]) -> Option<Value> {
    if edt.is_empty() {
        return None;
    }
    let count = edt[0] as usize;
    let mut instances = Vec::with_capacity(count);
    let mut i = 1;
    for _ in 0..count {
        if i + 3 > edt.len() {
            break; // 壊さず読めた分だけ
        }
        let eoj = Eoj([edt[i], edt[i + 1], edt[i + 2]]);
        instances.push(eoj.to_hex());
        i += 3;
    }
    Some(json!({ "count": count, "instances": instances }))
}

/// プロパティマップ (EPC 0x9D/0x9E/0x9F) をパース → EPC 一覧。
///
/// 2 形式:
/// - プロパティ数 ≤ 15: `count(1) + EPC(1)×count` の素直な列挙。
/// - プロパティ数 ≥ 16: `count(1) + bitmap(16)`。
///   バイト添字 i(0..=0xF)=EPC 下位ニブル、bit b(0..=7)=EPC 上位ニブル(b+8)。
///   よって EPC = ((b + 8) << 4) | i。
pub fn parse_property_map(edt: &[u8]) -> Option<Vec<u8>> {
    if edt.is_empty() {
        return None;
    }
    let count = edt[0] as usize;
    if count < 16 {
        // 形式 1: そのまま列挙
        let body = &edt[1..];
        if body.len() < count {
            return None;
        }
        Some(body[..count].to_vec())
    } else {
        // 形式 2: 16 バイト bitmap
        if edt.len() < 17 {
            return None;
        }
        let bitmap = &edt[1..17];
        let mut epcs = Vec::with_capacity(count);
        for (i, &byte) in bitmap.iter().enumerate() {
            for b in 0..8u8 {
                if byte & (1 << b) != 0 {
                    let epc = ((b + 8) << 4) | (i as u8);
                    epcs.push(epc);
                }
            }
        }
        epcs.sort_unstable();
        Some(epcs)
    }
}

/// EPC 一覧 → プロパティマップ EDT を組み立てる (テスト / 対称性用)。
#[cfg(test)]
pub fn build_property_map(epcs: &[u8]) -> Vec<u8> {
    let count = epcs.len();
    if count < 16 {
        let mut out = Vec::with_capacity(1 + count);
        out.push(count as u8);
        out.extend_from_slice(epcs);
        out
    } else {
        let mut bitmap = [0u8; 16];
        for &epc in epcs {
            let i = (epc & 0x0F) as usize;
            let b = (epc >> 4).wrapping_sub(8);
            if b < 8 {
                bitmap[i] |= 1 << b;
            }
        }
        let mut out = Vec::with_capacity(17);
        out.push(count as u8);
        out.extend_from_slice(&bitmap);
        out
    }
}

/// edt を常に hex で、デコードできれば value を併記した JSON を返す。
pub fn property_json(eoj: Eoj, epc: u8, edt: &[u8]) -> Value {
    let mut obj = json!({
        "epc": format!("{epc:02X}"),
        "pdc": edt.len(),
        "edt_hex": bytes_to_hex(edt),
    });
    if let Some(value) = decode(eoj, epc, edt) {
        obj["value"] = value;
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_map_format1_roundtrip() {
        let epcs = vec![0x80, 0x9D, 0x9E, 0x9F, 0xD6];
        let edt = build_property_map(&epcs);
        assert_eq!(edt[0], 5);
        let parsed = parse_property_map(&edt).unwrap();
        assert_eq!(parsed, epcs);
    }

    #[test]
    fn property_map_format2_roundtrip() {
        // 16 個以上 → bitmap 形式
        let mut epcs: Vec<u8> = (0x80..=0x90).collect(); // 17 個
        let edt = build_property_map(&epcs);
        assert_eq!(edt.len(), 17);
        assert_eq!(edt[0], 17);
        let mut parsed = parse_property_map(&edt).unwrap();
        parsed.sort_unstable();
        epcs.sort_unstable();
        assert_eq!(parsed, epcs);
    }

    #[test]
    fn property_map_bit_layout() {
        // EPC 0x80 → byte index 0, bit 0
        let edt = build_property_map(&(0x80..=0x90).collect::<Vec<_>>());
        assert_eq!(edt[1] & 0x01, 0x01); // index0 bit0 = 0x80
    }

    #[test]
    fn instance_list_decode() {
        // count=2, EOJ 013001, 026001
        let edt = vec![0x02, 0x01, 0x30, 0x01, 0x02, 0x60, 0x01];
        let v = decode_instance_list(&edt).unwrap();
        assert_eq!(v["count"], 2);
        assert_eq!(v["instances"][0], "013001");
    }

    #[test]
    fn instance_list_truncated_no_panic() {
        let edt = vec![0x05, 0x01, 0x30]; // count=5 だが 1 個分も無い
        let v = decode_instance_list(&edt).unwrap();
        assert_eq!(v["count"], 5);
        assert_eq!(v["instances"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn on_off_decode() {
        assert_eq!(
            decode(Eoj([1, 0x30, 1]), 0x80, &[0x30]).unwrap()["power"],
            "on"
        );
        assert_eq!(
            decode(Eoj([1, 0x30, 1]), 0x80, &[0x31]).unwrap()["power"],
            "off"
        );
    }

    #[test]
    fn unknown_epc_returns_none() {
        assert!(decode(Eoj([1, 0x30, 1]), 0xFF, &[0xAB]).is_none());
    }

    #[test]
    fn version_device_release() {
        // 機器オブジェクト: Release P rev2 → 00 00 50 02
        let v = decode(Eoj([0x02, 0x63, 1]), 0x82, &[0x00, 0x00, 0x50, 0x02]).unwrap();
        assert_eq!(v["release"], "P");
        assert_eq!(v["revision"], 2);
    }

    #[test]
    fn version_node_profile_protocol() {
        // ノードプロファイル: 01 0d 01 00 → Ver1.13 規定電文形式
        let v = decode(Eoj([0x0E, 0xF0, 1]), 0x82, &[0x01, 0x0D, 0x01, 0x00]).unwrap();
        assert_eq!(v["protocol_version"], "1.13");
        assert_eq!(v["message_format"], "specified");
    }

    #[test]
    fn version_non_ascii_release_none() {
        assert!(decode(Eoj([0x02, 0x63, 1]), 0x82, &[0x00, 0x00, 0x00, 0x00]).is_none());
    }

    #[test]
    fn manufacturer_known() {
        let v = decode(Eoj([0x02, 0x63, 1]), 0x8A, &[0x00, 0x00, 0x0B]).unwrap();
        assert_eq!(v["manufacturer_code"], "00000B");
        assert_eq!(v["manufacturer"], "パナソニック");
    }

    #[test]
    fn manufacturer_unknown_hex_only() {
        let v = decode(Eoj([0x02, 0x63, 1]), 0x8A, &[0xAB, 0xCD, 0xEF]).unwrap();
        assert_eq!(v["manufacturer_code"], "ABCDEF");
        assert!(v.get("manufacturer").is_none());
    }

    #[test]
    fn shutter_operation_and_state() {
        let eoj = Eoj([0x02, 0x63, 1]);
        assert_eq!(decode(eoj, 0xE0, &[0x42]).unwrap()["operation"], "close");
        assert_eq!(decode(eoj, 0xE1, &[0x32]).unwrap()["open_level_percent"], 50);
        assert_eq!(decode(eoj, 0xEA, &[0x43]).unwrap()["state"], "opening");
    }

    #[test]
    fn shutter_epc_only_applies_to_shutter_class() {
        // 0xE0 は雨戸クラス以外では未知 EPC 扱い
        assert!(decode(Eoj([0x01, 0x30, 1]), 0xE0, &[0x41]).is_none());
    }

    #[test]
    fn aircon_operation_mode() {
        let eoj = Eoj([0x01, 0x30, 1]);
        assert_eq!(decode(eoj, 0xB0, &[0x42]).unwrap()["operation_mode"], "cool");
        assert_eq!(decode(eoj, 0xB0, &[0x43]).unwrap()["operation_mode"], "heat");
        assert_eq!(decode(eoj, 0xB0, &[0x40]).unwrap()["operation_mode"], "other");
    }

    #[test]
    fn aircon_temp_setpoint() {
        let eoj = Eoj([0x01, 0x30, 1]);
        assert_eq!(decode(eoj, 0xB3, &[0x1A]).unwrap()["temp_setpoint_c"], 26);
        // 0xFD = 設定値不明 → null
        assert!(decode(eoj, 0xB3, &[0xFD]).unwrap()["temp_setpoint_c"].is_null());
    }

    #[test]
    fn aircon_room_temp_signed() {
        let eoj = Eoj([0x01, 0x30, 1]);
        assert_eq!(decode(eoj, 0xBB, &[0x19]).unwrap()["room_temp_c"], 25);
        // 0xFB = -5℃ (signed)
        assert_eq!(decode(eoj, 0xBB, &[0xFB]).unwrap()["room_temp_c"], -5);
    }

    #[test]
    fn aircon_air_flow() {
        let eoj = Eoj([0x01, 0x30, 1]);
        assert_eq!(decode(eoj, 0xA0, &[0x41]).unwrap()["air_flow"], "auto");
        assert_eq!(decode(eoj, 0xA0, &[0x33]).unwrap()["air_flow_level"], 3);
    }
}
