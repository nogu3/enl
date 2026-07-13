//! 各サブコマンドの stdout JSON 出力スキーマ (JSON Schema draft 2020-12)。
//!
//! 出力スキーマは安定契約。LLM の function-calling / `jq` が依存するため、
//! 破壊的変更を避ける拠り所として機械可読スキーマをここに固定する。
//! `enl schema [cmd]` で取得でき、消費側はこれをスキーマ取得に使える。

use clap::ValueEnum;
use serde_json::{json, Value};

const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// `enl schema` の対象サブコマンド。clap が未知値を弾く (exit 2)。
/// ここで enum を持ち match を網羅させることで、サブコマンド追加時の
/// スキーマ実装漏れをコンパイルエラーで検出する。
#[derive(Clone, Copy, ValueEnum)]
pub enum SchemaTarget {
    Discover,
    Get,
    Set,
    Describe,
    Raw,
    Listen,
}

/// 名前指定があればそのスキーマ、無ければ全サブコマンドのスキーマ集約。
pub fn for_target(target: Option<SchemaTarget>) -> Value {
    match target {
        None => all(),
        Some(SchemaTarget::Discover) => discover(),
        Some(SchemaTarget::Get) => get(),
        Some(SchemaTarget::Set) => set(),
        Some(SchemaTarget::Describe) => describe(),
        Some(SchemaTarget::Raw) => raw(),
        Some(SchemaTarget::Listen) => listen(),
    }
}

/// 全サブコマンドのスキーマを 1 オブジェクトに集約。
fn all() -> Value {
    json!({
        "discover": discover(),
        "get": get(),
        "set": set(),
        "describe": describe(),
        "raw": raw(),
        "listen": listen(),
    })
}

/// get/set/raw frame で共通の 1 プロパティ表現。
/// `edt_hex` は常に存在、`name`/`value` は辞書にある場合のみ付加。
fn property() -> Value {
    json!({
        "type": "object",
        "properties": {
            "epc": { "type": "string", "description": "EPC (2 hex 桁, 大文字)" },
            "name": { "type": "string", "description": "既知 EPC の正規名 (辞書にある場合のみ)" },
            "pdc": { "type": "integer", "description": "EDT のバイト長" },
            "edt_hex": { "type": "string", "description": "EDT の生 hex (常に存在しロスレス)" },
            "value": { "description": "デコードできた場合のみ付加される人間可読値" }
        },
        "required": ["epc", "pdc", "edt_hex"],
        "additionalProperties": false
    })
}

/// `enl discover` の出力スキーマ。
fn discover() -> Value {
    json!({
        "$schema": DIALECT,
        "title": "enl discover output",
        "type": "object",
        "properties": {
            "devices": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "ip": { "type": "string" },
                        "count": { "type": "integer", "description": "インスタンス数 (応答パース時のみ)" },
                        "instances": {
                            "type": "array",
                            "items": { "type": "string", "description": "EOJ (6 hex 桁)" }
                        }
                    },
                    "required": ["ip"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["devices"],
        "additionalProperties": false
    })
}

/// `enl get` の出力スキーマ。
fn get() -> Value {
    json!({
        "$schema": DIALECT,
        "title": "enl get output",
        "type": "object",
        "properties": {
            "ip": { "type": "string" },
            "eoj": { "type": "string", "description": "EOJ (6 hex 桁)" },
            "esv": { "type": "string", "description": "応答 ESV 名 (例 Get_Res)" },
            "properties": { "type": "array", "items": property() }
        },
        "required": ["ip", "eoj", "esv", "properties"],
        "additionalProperties": false
    })
}

/// `enl set` の出力スキーマ。
fn set() -> Value {
    json!({
        "$schema": DIALECT,
        "title": "enl set output",
        "type": "object",
        "properties": {
            "ip": { "type": "string" },
            "eoj": { "type": "string", "description": "EOJ (6 hex 桁)" },
            "esv": { "type": "string", "description": "応答 ESV 名 (例 Set_Res)" },
            "result": { "type": "string", "const": "accepted" },
            "properties": { "type": "array", "items": property() }
        },
        "required": ["ip", "eoj", "esv", "result", "properties"],
        "additionalProperties": false
    })
}

/// describe のマップ 1 エントリ (get_map/set_map/inf_map の要素)。
fn map_entry() -> Value {
    json!({
        "type": "object",
        "properties": {
            "epc": { "type": "string", "description": "EPC (2 hex 桁, 大文字)" },
            "name": { "type": "string", "description": "既知 EPC の正規名 (辞書にある場合のみ)" },
            "values": {
                "type": "object",
                "description": "enum 型 EPC の値域 {hex: 意味名} (該当時のみ)"
            }
        },
        "required": ["epc"],
        "additionalProperties": false
    })
}

/// マップ値: 正常時は配列、壊れた/空マップ時は生 hex フォールバック。
fn map_value() -> Value {
    json!({
        "oneOf": [
            { "type": "array", "items": map_entry() },
            {
                "type": "object",
                "properties": { "edt_hex": { "type": "string" } },
                "required": ["edt_hex"],
                "additionalProperties": false
            }
        ]
    })
}

/// `enl describe` の出力スキーマ。
fn describe() -> Value {
    json!({
        "$schema": DIALECT,
        "title": "enl describe output",
        "type": "object",
        "properties": {
            "ip": { "type": "string" },
            "eoj": { "type": "string", "description": "EOJ (6 hex 桁)" },
            "esv": { "type": "string" },
            "get_map": map_value(),
            "set_map": map_value(),
            "inf_map": map_value()
        },
        "required": ["ip", "eoj", "esv"],
        "additionalProperties": false
    })
}

/// raw 応答に併記される best-effort パース済みフレーム。
fn frame() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ehd2": { "type": "string", "description": "EHD2 (2 hex 桁)" },
            "tid": { "type": "string", "description": "TID (4 hex 桁)" },
            "format": { "type": "string", "enum": ["standard", "setget", "arbitrary"] },
            "seoj": { "type": "string" },
            "deoj": { "type": "string" },
            "esv": { "type": "string" },
            "properties": { "type": "array", "items": property() },
            "set_properties": { "type": "array", "items": property() },
            "get_properties": { "type": "array", "items": property() },
            "edt_hex": { "type": "string", "description": "arbitrary 形式 (EHD2=0x82) の生 hex" }
        },
        "required": ["ehd2", "tid", "format"],
        "additionalProperties": false
    })
}

/// `enl raw` の出力スキーマ。SNA もエラーにせず response_hex として返す。
fn raw() -> Value {
    json!({
        "$schema": DIALECT,
        "title": "enl raw output",
        "type": "object",
        "properties": {
            "ip": { "type": "string" },
            "sent_hex": { "type": "string", "description": "送信フレームの生 hex" },
            "response_hex": { "type": "string", "description": "応答の生 hex (SNA 含め常に存在)" },
            "frame": frame(),
            "parse_error": { "type": "string", "description": "応答をパースできなかった場合のみ" }
        },
        "required": ["ip", "sent_hex", "response_hex"],
        "additionalProperties": false
    })
}

/// `enl listen` の出力スキーマ。INF / INFC 通知の収集結果。
fn listen() -> Value {
    json!({
        "$schema": DIALECT,
        "title": "enl listen output",
        "type": "object",
        "properties": {
            "events": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "ip": { "type": "string", "description": "通知の送信元 IP" },
                        "tid": { "type": "string", "description": "TID (4 hex 桁)" },
                        "seoj": { "type": "string", "description": "通知元 EOJ (6 hex 桁)" },
                        "deoj": { "type": "string" },
                        "esv": { "type": "string", "enum": ["Inf", "InfC"] },
                        "properties": { "type": "array", "items": property() }
                    },
                    "required": ["ip", "tid", "seoj", "deoj", "esv", "properties"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["events"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn all_covers_every_target() {
        let all = all();
        let obj = all.as_object().unwrap();
        let variants = SchemaTarget::value_variants();
        assert_eq!(obj.len(), variants.len());
        for t in variants {
            let name = t.to_possible_value().unwrap().get_name().to_string();
            assert!(obj.contains_key(&name), "{name} 欠落");
        }
    }

    #[test]
    fn each_schema_is_valid_object_with_dialect() {
        for t in SchemaTarget::value_variants() {
            let name = t.to_possible_value().unwrap().get_name().to_string();
            let s = for_target(Some(*t));
            assert_eq!(s["$schema"], DIALECT, "{name} に $schema 無し");
            assert_eq!(s["type"], "object", "{name} の type が object でない");
            assert!(s["properties"].is_object(), "{name} に properties 無し");
            assert!(s["required"].is_array(), "{name} に required 無し");
        }
    }

    #[test]
    fn none_returns_all() {
        assert_eq!(for_target(None), all());
    }

    #[test]
    fn get_property_items_lossless() {
        // 出力契約: properties[*] は最低限 epc/pdc/edt_hex を必ず持つ。
        let req = &get()["properties"]["properties"]["items"]["required"];
        let req: Vec<&str> = req
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for k in ["epc", "pdc", "edt_hex"] {
            assert!(req.contains(&k), "property に {k} 必須が無い");
        }
    }
}
