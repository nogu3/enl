//! プロパティ定義テーブル駆動の任意デコードレイヤ。
//!
//! 方針: 辞書にあれば人間可読値を併記、なければ生 hex のまま。
//! 未知 EPC で壊れない。codec コア(ダンプ層)とは分離する。

use crate::codec::{bytes_to_hex, Eoj};
use serde_json::{json, Value};

/// EDT を可能ならデコードして付加情報の JSON 値を返す。
/// デコードできなければ None (呼び出し側は edt_hex のみ出す)。
pub fn decode(eoj: Eoj, epc: u8, edt: &[u8]) -> Option<Value> {
    // enum 型 (共通 power + クラス固有) は値域テーブルを単一ソースに解釈する。
    if let Some(p) = enum_prop(eoj, epc) {
        return Some(decode_enum(p, edt));
    }
    // スーパークラス共通 EPC (全機器)
    match epc {
        0x82 => return decode_version(eoj, edt),
        0x8A => return decode_manufacturer(edt),
        0x9D..=0x9F => return parse_property_map(edt).map(
            |m| json!({ "property_map": m.iter().map(|e| format!("{e:02X}")).collect::<Vec<_>>() }),
        ),
        _ => {}
    }
    // クラス固有の数値型 EPC (enum 型は上の enum_prop 経由)
    (class_def(eoj)?.decode_numeric)(epc, edt)
}

/// 対応クラスごとの辞書一式。新クラス対応は CLASSES に 1 エントリ足すだけにする。
struct ClassDef {
    /// (クラスグループ, クラス)。
    class: (u8, u8),
    /// enum 型 EPC の値域テーブル (decode / describe values / set 値名の単一ソース)。
    enums: &'static [EnumProp],
    /// EPC 正規名テーブル (クラス固有分。共通は COMMON_EPC)。
    epc_names: &'static [(u8, &'static str)],
    /// 数値型など enum 以外のクラス固有 EPC のデコーダ。
    decode_numeric: fn(u8, &[u8]) -> Option<Value>,
}

const CLASSES: &[ClassDef] = &[
    // 家庭用エアコン
    ClassDef {
        class: (0x01, 0x30),
        enums: AIRCON_ENUM,
        epc_names: AIRCON_EPC,
        decode_numeric: decode_aircon,
    },
    // 電動雨戸・シャッター
    ClassDef {
        class: (0x02, 0x63),
        enums: SHUTTER_ENUM,
        epc_names: SHUTTER_EPC,
        decode_numeric: decode_shutter,
    },
    // ノードプロファイル
    ClassDef {
        class: (0x0E, 0xF0),
        enums: &[],
        epc_names: &[],
        decode_numeric: decode_node_profile,
    },
];

/// EOJ のクラスに対応する ClassDef。未対応クラスは None。
fn class_def(eoj: Eoj) -> Option<&'static ClassDef> {
    CLASSES
        .iter()
        .find(|c| c.class == (eoj.class_group(), eoj.class()))
}

/// enum 型 EPC の値域定義。decode の値解釈と describe の values 列挙の単一ソース。
/// ここに 1 度書けば「値→意味」(decode) と「意味の候補一覧」(describe) の両方が出る。
struct EnumProp {
    epc: u8,
    /// デコード時の JSON キー (例 "operation_mode")。
    key: &'static str,
    /// (バイト値, 意味) の対応。
    values: &'static [(u8, &'static str)],
}

/// 全機器共通 (スーパークラス) の enum 型 EPC。
const COMMON_ENUM: &[EnumProp] = &[EnumProp {
    epc: 0x80,
    key: "power",
    values: &[(0x30, "on"), (0x31, "off")],
}];

/// 家庭用エアコン (0x0130) 固有の enum 型 EPC。
const AIRCON_ENUM: &[EnumProp] = &[EnumProp {
    epc: 0xB0,
    key: "operation_mode",
    values: &[
        (0x41, "auto"),
        (0x42, "cool"),
        (0x43, "heat"),
        (0x44, "dry"),
        (0x45, "fan"),
        (0x40, "other"),
    ],
}];

/// 電動雨戸・シャッター (0x0263) 固有の enum 型 EPC。
const SHUTTER_ENUM: &[EnumProp] = &[
    EnumProp {
        epc: 0xE0,
        key: "operation",
        values: &[(0x41, "open"), (0x42, "close"), (0x43, "stop")],
    },
    EnumProp {
        epc: 0xEA,
        key: "state",
        values: &[
            (0x41, "fully_open"),
            (0x42, "fully_closed"),
            (0x43, "opening"),
            (0x44, "closing"),
            (0x45, "stopped_midway"),
        ],
    },
];

/// EPC に対応する enum 値域定義。クラス固有を優先し、無ければ共通から引く。
fn enum_prop(eoj: Eoj, epc: u8) -> Option<&'static EnumProp> {
    class_def(eoj)
        .map(|c| c.enums)
        .unwrap_or(&[])
        .iter()
        .chain(COMMON_ENUM)
        .find(|p| p.epc == epc)
}

/// enum EDT を {key: 意味} にデコード。未知バイトは "unknown"。
fn decode_enum(p: &EnumProp, edt: &[u8]) -> Value {
    let meaning = edt
        .first()
        .and_then(|b| p.values.iter().find(|(v, _)| v == b).map(|(_, m)| *m))
        .unwrap_or("unknown");
    let mut obj = serde_json::Map::new();
    obj.insert(p.key.to_string(), json!(meaning));
    Value::Object(obj)
}

/// enum 型 EPC の意味名 → バイト値 (例 close → 0x42)。set の値名指定用。
/// 数値型・未対応 EPC や未知の名前は None。
pub fn edt_for_name(eoj: Eoj, epc: u8, name: &str) -> Option<u8> {
    enum_prop(eoj, epc)?
        .values
        .iter()
        .find(|(_, m)| *m == name)
        .map(|(v, _)| *v)
}

/// enum 型 EPC の値域 (バイト hex → 意味) を JSON で返す。describe の values 用。
/// 数値型・未対応 EPC は None。
pub fn epc_values(eoj: Eoj, epc: u8) -> Option<Value> {
    enum_prop(eoj, epc).map(|p| {
        let mut obj = serde_json::Map::new();
        for (v, m) in p.values {
            obj.insert(format!("{v:02X}"), json!(m));
        }
        Value::Object(obj)
    })
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

/// 家庭用エアコン (0x0130) 固有の数値型 EPC のデコード (enum 型は enum_prop 経由)。
fn decode_aircon(epc: u8, edt: &[u8]) -> Option<Value> {
    match epc {
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

/// 電動雨戸・シャッター (0x0263) 固有の数値型 EPC のデコード (enum 型は enum_prop 経由)。
fn decode_shutter(epc: u8, edt: &[u8]) -> Option<Value> {
    match epc {
        // 0xE1 開度レベル設定: 0x00-0x64 (0-100%)
        0xE1 => edt.first().map(|&v| json!({ "open_level_percent": v })),
        _ => None,
    }
}

/// ノードプロファイル (0x0EF0) 固有 EPC のデコード。
fn decode_node_profile(epc: u8, edt: &[u8]) -> Option<Value> {
    match epc {
        0xD6 => decode_instance_list(edt),
        _ => None,
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

/// 全機器共通 EPC (スーパークラス) の正規名。
const COMMON_EPC: &[(u8, &str)] = &[
    (0x80, "power"),
    (0x82, "standard_version"),
    (0x8A, "manufacturer"),
    (0x9D, "status_change_map"),
    (0x9E, "set_property_map"),
    (0x9F, "get_property_map"),
];

/// 家庭用エアコン (0x0130) 固有 EPC の正規名。
const AIRCON_EPC: &[(u8, &str)] = &[
    (0xA0, "air_flow"),
    (0xB0, "operation_mode"),
    (0xB3, "target_temperature"),
    (0xBB, "room_temperature"),
];

/// 電動雨戸・シャッター (0x0263) 固有 EPC の正規名。
const SHUTTER_EPC: &[(u8, &str)] = &[
    (0xE0, "open_close_operation"),
    (0xE1, "open_level"),
    (0xEA, "open_close_state"),
];

/// EOJ のクラスに対応する固有 EPC 名テーブル。未対応クラスは空。
fn class_epc_table(eoj: Eoj) -> &'static [(u8, &'static str)] {
    class_def(eoj).map(|c| c.epc_names).unwrap_or(&[])
}

/// EPC → 正規名。クラス固有を優先し、無ければ共通から引く。未知は None。
pub fn epc_name(eoj: Eoj, epc: u8) -> Option<&'static str> {
    class_epc_table(eoj)
        .iter()
        .chain(COMMON_EPC)
        .find(|(e, _)| *e == epc)
        .map(|(_, n)| *n)
}

/// 正規名 → EPC。クラス固有を優先。未知は None。
pub fn epc_for_name(eoj: Eoj, name: &str) -> Option<u8> {
    class_epc_table(eoj)
        .iter()
        .chain(COMMON_EPC)
        .find(|(_, n)| *n == name)
        .map(|(e, _)| *e)
}

/// describe のマップ 1 エントリ: EPC hex + 辞書にあれば name、enum 型なら values。
pub fn map_entry_json(eoj: Eoj, epc: u8) -> Value {
    let mut obj = json!({ "epc": format!("{epc:02X}") });
    if let Some(name) = epc_name(eoj, epc) {
        obj["name"] = json!(name);
    }
    if let Some(values) = epc_values(eoj, epc) {
        obj["values"] = values;
    }
    obj
}

/// edt を常に hex で、名前/デコード値があれば併記した JSON を返す。
pub fn property_json(eoj: Eoj, epc: u8, edt: &[u8]) -> Value {
    let mut obj = json!({
        "epc": format!("{epc:02X}"),
        "pdc": edt.len(),
        "edt_hex": bytes_to_hex(edt),
    });
    if let Some(name) = epc_name(eoj, epc) {
        obj["name"] = json!(name);
    }
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
        assert_eq!(
            decode(eoj, 0xE1, &[0x32]).unwrap()["open_level_percent"],
            50
        );
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
        assert_eq!(
            decode(eoj, 0xB0, &[0x42]).unwrap()["operation_mode"],
            "cool"
        );
        assert_eq!(
            decode(eoj, 0xB0, &[0x43]).unwrap()["operation_mode"],
            "heat"
        );
        assert_eq!(
            decode(eoj, 0xB0, &[0x40]).unwrap()["operation_mode"],
            "other"
        );
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

    #[test]
    fn epc_name_common_and_class() {
        let shutter = Eoj([0x02, 0x63, 1]);
        let aircon = Eoj([0x01, 0x30, 1]);
        // 共通
        assert_eq!(epc_name(shutter, 0x8A), Some("manufacturer"));
        // クラス固有
        assert_eq!(epc_name(shutter, 0xEA), Some("open_close_state"));
        assert_eq!(epc_name(aircon, 0xB0), Some("operation_mode"));
        // 雨戸の EA はエアコンでは未知
        assert_eq!(epc_name(aircon, 0xEA), None);
        // 未知 EPC
        assert_eq!(epc_name(shutter, 0x77), None);
    }

    #[test]
    fn epc_for_name_roundtrip() {
        let aircon = Eoj([0x01, 0x30, 1]);
        assert_eq!(epc_for_name(aircon, "operation_mode"), Some(0xB0));
        assert_eq!(epc_for_name(aircon, "power"), Some(0x80));
        assert_eq!(epc_for_name(aircon, "open_close_state"), None);
        assert_eq!(epc_for_name(aircon, "bogus"), None);
        // name → epc → name の往復
        let epc = epc_for_name(aircon, "room_temperature").unwrap();
        assert_eq!(epc_name(aircon, epc), Some("room_temperature"));
    }

    #[test]
    fn epc_values_enum_only() {
        let shutter = Eoj([0x02, 0x63, 1]);
        let aircon = Eoj([0x01, 0x30, 1]);
        // enum 型は値域辞書を返す
        let v = epc_values(shutter, 0xE0).unwrap();
        assert_eq!(v["41"], "open");
        assert_eq!(v["42"], "close");
        assert_eq!(v["43"], "stop");
        // 共通 power
        assert_eq!(epc_values(aircon, 0x80).unwrap()["30"], "on");
        // 数値型 (開度%・温度) は None
        assert!(epc_values(shutter, 0xE1).is_none());
        assert!(epc_values(aircon, 0xB3).is_none());
        // 未対応 EPC は None
        assert!(epc_values(shutter, 0x77).is_none());
    }

    #[test]
    fn enum_decode_matches_values_catalog() {
        // 単一ソース担保: epc_values の各エントリを decode に通すと同じ意味が出る
        let shutter = Eoj([0x02, 0x63, 1]);
        let values = epc_values(shutter, 0xEA).unwrap();
        for (hex, meaning) in values.as_object().unwrap() {
            let byte = u8::from_str_radix(hex, 16).unwrap();
            let decoded = decode(shutter, 0xEA, &[byte]).unwrap();
            assert_eq!(decoded["state"], *meaning);
        }
    }

    #[test]
    fn property_json_includes_name() {
        let v = property_json(Eoj([0x01, 0x30, 1]), 0xB0, &[0x42]);
        assert_eq!(v["epc"], "B0");
        assert_eq!(v["name"], "operation_mode");
        assert_eq!(v["value"]["operation_mode"], "cool");
    }

    #[test]
    fn map_entry_json_shapes() {
        let shutter = Eoj([0x02, 0x63, 1]);
        // enum 型: name + values 併記
        let v = map_entry_json(shutter, 0xE0);
        assert_eq!(v["epc"], "E0");
        assert_eq!(v["name"], "open_close_operation");
        assert_eq!(v["values"]["42"], "close");
        // 数値型: name のみ (values 無し)
        let v = map_entry_json(shutter, 0xE1);
        assert_eq!(v["name"], "open_level");
        assert!(v.get("values").is_none());
        // 未知 EPC: epc のみ
        let v = map_entry_json(shutter, 0x77);
        assert_eq!(v["epc"], "77");
        assert!(v.get("name").is_none());
        assert!(v.get("values").is_none());
    }
}
