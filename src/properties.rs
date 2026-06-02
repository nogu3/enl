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
        0x9D..=0x9F => return parse_property_map(edt).map(
            |m| json!({ "property_map": m.iter().map(|e| format!("{e:02X}")).collect::<Vec<_>>() }),
        ),
        _ => {}
    }
    // ノードプロファイル (0x0EF0xx) 固有
    if eoj.class_group() == 0x0E && eoj.class() == 0xF0 && epc == 0xD6 {
        return decode_instance_list(edt);
    }
    None
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
}
