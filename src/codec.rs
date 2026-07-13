//! ECHONET Lite フレーム codec。依存ゼロの手書き。
//!
//! フレーム構造:
//! ```text
//! EHD1(1) EHD2(1) TID(2) | EDATA
//! EDATA(規定形式): SEOJ(3) DEOJ(3) ESV(1) OPC(1) [EPC(1) PDC(1) EDT(PDC)]×OPC
//! SETGET形式:      SEOJ(3) DEOJ(3) ESV(1) OPCSet(1) set… OPCGet(1) get…
//! ```
//!
//! 設計原則: 未知のクラス / EPC / ESV / EHD2 が来てもロスレスに保持し壊れない。
//! codec コアは「常に生 hex + PDC」を持つダンプ層。デコードは別レイヤ。

use std::fmt;

/// 規定電文形式 1。
pub const EHD1: u8 = 0x10;
/// EDATA が ECHONET Lite 規定電文形式（プロパティ列）。
pub const EHD2_FORMAT1: u8 = 0x81;
/// EDATA が任意電文形式（汎用パース不能）。
pub const EHD2_FORMAT2: u8 = 0x82;

/// ECHONET オブジェクト (クラスグループ / クラス / インスタンス)。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Eoj(pub [u8; 3]);

impl Eoj {
    pub fn class_group(&self) -> u8 {
        self.0[0]
    }
    pub fn class(&self) -> u8 {
        self.0[1]
    }
    #[allow(dead_code)] // EOJ アクセサの対称性のため公開 API として残す
    pub fn instance(&self) -> u8 {
        self.0[2]
    }
    /// "013001" のような 6 桁 hex から生成。
    pub fn from_hex(s: &str) -> Result<Eoj, CodecError> {
        let bytes = hex_to_bytes(s)?;
        if bytes.len() != 3 {
            return Err(CodecError::BadField("EOJ は 3 バイト (6 hex 桁) 必須"));
        }
        Ok(Eoj([bytes[0], bytes[1], bytes[2]]))
    }
    pub fn to_hex(self) -> String {
        bytes_to_hex(&self.0)
    }
}

impl fmt::Debug for Eoj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Eoj({})", self.to_hex())
    }
}

/// ECHONET Lite サービス (ESV)。既知値は名前付き、未知は Unknown で保持。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Esv {
    // 要求
    SetI,   // 0x60
    SetC,   // 0x61
    Get,    // 0x62
    InfReq, // 0x63
    SetGet, // 0x6E
    // 応答 / 通知
    SetRes,    // 0x71
    GetRes,    // 0x72
    Inf,       // 0x73
    InfC,      // 0x74
    InfCRes,   // 0x7A
    SetGetRes, // 0x7E
    // 不可応答 (SNA) = 機器リジェクト
    SetISna,   // 0x50
    SetCSna,   // 0x51
    GetSna,    // 0x52
    InfSna,    // 0x53
    SetGetSna, // 0x5E
    Unknown(u8),
}

impl Esv {
    pub fn from_u8(v: u8) -> Esv {
        match v {
            0x60 => Esv::SetI,
            0x61 => Esv::SetC,
            0x62 => Esv::Get,
            0x63 => Esv::InfReq,
            0x6E => Esv::SetGet,
            0x71 => Esv::SetRes,
            0x72 => Esv::GetRes,
            0x73 => Esv::Inf,
            0x74 => Esv::InfC,
            0x7A => Esv::InfCRes,
            0x7E => Esv::SetGetRes,
            0x50 => Esv::SetISna,
            0x51 => Esv::SetCSna,
            0x52 => Esv::GetSna,
            0x53 => Esv::InfSna,
            0x5E => Esv::SetGetSna,
            other => Esv::Unknown(other),
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Esv::SetI => 0x60,
            Esv::SetC => 0x61,
            Esv::Get => 0x62,
            Esv::InfReq => 0x63,
            Esv::SetGet => 0x6E,
            Esv::SetRes => 0x71,
            Esv::GetRes => 0x72,
            Esv::Inf => 0x73,
            Esv::InfC => 0x74,
            Esv::InfCRes => 0x7A,
            Esv::SetGetRes => 0x7E,
            Esv::SetISna => 0x50,
            Esv::SetCSna => 0x51,
            Esv::GetSna => 0x52,
            Esv::InfSna => 0x53,
            Esv::SetGetSna => 0x5E,
            Esv::Unknown(v) => v,
        }
    }

    /// SETGET 系 (二段構造)。
    pub fn is_setget(self) -> bool {
        matches!(self, Esv::SetGet | Esv::SetGetRes | Esv::SetGetSna)
    }

    /// SNA (機器リジェクト)。ESV 上位ニブル 0x5。
    pub fn is_sna(self) -> bool {
        (self.to_u8() & 0xF0) == 0x50
    }

    /// 応答 / 通知 (Set_Res, Get_Res, INF, INFC, INFC_Res, SetGet_Res)。ESV 上位ニブル 0x7。
    pub fn is_response(self) -> bool {
        (self.to_u8() & 0xF0) == 0x70
    }

    /// 仕様上の短い名前 (JSON 用)。
    pub fn name(self) -> String {
        match self {
            Esv::Unknown(v) => format!("Unknown(0x{v:02X})"),
            other => format!("{other:?}"),
        }
    }
}

/// 単一プロパティ。PDC は edt.len() で自明なので保持しない。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Property {
    pub epc: u8,
    pub edt: Vec<u8>,
}

impl Property {
    pub fn new(epc: u8, edt: Vec<u8>) -> Property {
        Property { epc, edt }
    }
    /// Get 要求用: EDT 無し (PDC=0)。
    pub fn get(epc: u8) -> Property {
        Property {
            epc,
            edt: Vec::new(),
        }
    }
    pub fn pdc(&self) -> u8 {
        self.edt.len() as u8
    }
}

/// EDATA 本体。形式ごとに型を分けて非対称バグを防ぐ。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Edata {
    /// 規定形式 (EHD2=0x81) の通常フレーム。
    Standard {
        seoj: Eoj,
        deoj: Eoj,
        esv: Esv,
        props: Vec<Property>,
    },
    /// SETGET 系 (ESV 0x6E/0x7E/0x5E) の二段構造。
    SetGet {
        seoj: Eoj,
        deoj: Eoj,
        esv: Esv,
        set_props: Vec<Property>,
        get_props: Vec<Property>,
    },
    /// 任意電文形式 (EHD2=0x82)。汎用パース不能 → 生バイト pass-through。
    Arbitrary(Vec<u8>),
}

/// ECHONET Lite フレーム全体。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Frame {
    pub ehd1: u8,
    pub ehd2: u8,
    pub tid: u16,
    pub edata: Edata,
}

impl Frame {
    /// 規定形式の通常フレームを組み立てる。
    pub fn standard(tid: u16, seoj: Eoj, deoj: Eoj, esv: Esv, props: Vec<Property>) -> Frame {
        Frame {
            ehd1: EHD1,
            ehd2: EHD2_FORMAT1,
            tid,
            edata: Edata::Standard {
                seoj,
                deoj,
                esv,
                props,
            },
        }
    }

    /// 受信フレームのうち通常プロパティ列を取り出す (Standard のみ)。
    pub fn props(&self) -> Option<&[Property]> {
        match &self.edata {
            Edata::Standard { props, .. } => Some(props),
            _ => None,
        }
    }

    #[allow(dead_code)] // 受信フレームの ESV 検査に使う公開 API
    pub fn esv(&self) -> Option<Esv> {
        match &self.edata {
            Edata::Standard { esv, .. } | Edata::SetGet { esv, .. } => Some(*esv),
            Edata::Arbitrary(_) => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    /// フレームが途中で切れている。
    Truncated(&'static str),
    /// EHD1 が規定値でない。
    BadEhd1(u8),
    /// hex 文字列など入力フィールドが不正。
    BadField(&'static str),
    /// hex 文字列パース失敗。
    BadHex,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Truncated(w) => write!(f, "フレームが途中で切れている: {w}"),
            CodecError::BadEhd1(v) => write!(f, "EHD1 が不正: 0x{v:02X} (期待 0x10)"),
            CodecError::BadField(w) => write!(f, "入力フィールド不正: {w}"),
            CodecError::BadHex => write!(f, "hex 文字列が不正"),
        }
    }
}

impl std::error::Error for CodecError {}

// ───────────────────────── parse ─────────────────────────

/// バイト列を Frame にパースする。
pub fn parse(buf: &[u8]) -> Result<Frame, CodecError> {
    let mut c = Cursor::new(buf);
    let ehd1 = c.u8("EHD1")?;
    if ehd1 != EHD1 {
        return Err(CodecError::BadEhd1(ehd1));
    }
    let ehd2 = c.u8("EHD2")?;
    let tid = c.u16("TID")?;

    if ehd2 == EHD2_FORMAT2 {
        // 任意電文形式: 残り全部を生で保持。
        return Ok(Frame {
            ehd1,
            ehd2,
            tid,
            edata: Edata::Arbitrary(c.rest().to_vec()),
        });
    }
    // EHD2_FORMAT1 もしくは未知の EHD2 → 規定形式としてパース試行
    // (未知 EHD2 でも壊さず規定形式で読む)。
    let seoj = c.eoj("SEOJ")?;
    let deoj = c.eoj("DEOJ")?;
    let esv = Esv::from_u8(c.u8("ESV")?);

    if esv.is_setget() {
        let set_props = parse_block(&mut c, "OPCSet")?;
        let get_props = parse_block(&mut c, "OPCGet")?;
        Ok(Frame {
            ehd1,
            ehd2,
            tid,
            edata: Edata::SetGet {
                seoj,
                deoj,
                esv,
                set_props,
                get_props,
            },
        })
    } else {
        let props = parse_block(&mut c, "OPC")?;
        Ok(Frame {
            ehd1,
            ehd2,
            tid,
            edata: Edata::Standard {
                seoj,
                deoj,
                esv,
                props,
            },
        })
    }
}

/// OPC(1) + [EPC(1) PDC(1) EDT(PDC)]×OPC を読む。
fn parse_block(c: &mut Cursor, label: &'static str) -> Result<Vec<Property>, CodecError> {
    let opc = c.u8(label)?;
    let mut props = Vec::with_capacity(opc as usize);
    for _ in 0..opc {
        let epc = c.u8("EPC")?;
        let pdc = c.u8("PDC")?;
        let edt = c.take(pdc as usize, "EDT")?.to_vec();
        props.push(Property { epc, edt });
    }
    Ok(props)
}

// ───────────────────────── build ─────────────────────────

/// Frame をバイト列にシリアライズする。
pub fn build(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.push(frame.ehd1);
    out.push(frame.ehd2);
    out.extend_from_slice(&frame.tid.to_be_bytes());
    match &frame.edata {
        Edata::Arbitrary(bytes) => out.extend_from_slice(bytes),
        Edata::Standard {
            seoj,
            deoj,
            esv,
            props,
        } => {
            out.extend_from_slice(&seoj.0);
            out.extend_from_slice(&deoj.0);
            out.push(esv.to_u8());
            build_block(&mut out, props);
        }
        Edata::SetGet {
            seoj,
            deoj,
            esv,
            set_props,
            get_props,
        } => {
            out.extend_from_slice(&seoj.0);
            out.extend_from_slice(&deoj.0);
            out.push(esv.to_u8());
            build_block(&mut out, set_props);
            build_block(&mut out, get_props);
        }
    }
    out
}

fn build_block(out: &mut Vec<u8>, props: &[Property]) {
    out.push(props.len() as u8);
    for p in props {
        out.push(p.epc);
        out.push(p.pdc());
        out.extend_from_slice(&p.edt);
    }
}

// ───────────────────────── 小物 ─────────────────────────

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Cursor<'a> {
        Cursor { buf, pos: 0 }
    }
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], CodecError> {
        if self.pos + n > self.buf.len() {
            return Err(CodecError::Truncated(what));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self, what: &'static str) -> Result<u8, CodecError> {
        Ok(self.take(1, what)?[0])
    }
    fn u16(&mut self, what: &'static str) -> Result<u16, CodecError> {
        let s = self.take(2, what)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    fn eoj(&mut self, what: &'static str) -> Result<Eoj, CodecError> {
        let s = self.take(3, what)?;
        Ok(Eoj([s[0], s[1], s[2]]))
    }
    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
}

/// "01a3" → [0x01, 0xA3]。空白・先頭 0x は許容。
pub fn hex_to_bytes(s: &str) -> Result<Vec<u8>, CodecError> {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(CodecError::BadHex);
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    let bytes = cleaned.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, CodecError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(CodecError::BadHex),
    }
}

/// [0x01, 0xA3] → "01a3"。
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0F) as u32, 16).unwrap());
    }
    s
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// codec の砦: parse → build → parse が一致 (ラウンドトリップ)。
    fn roundtrip(buf: &[u8]) {
        let f1 = parse(buf).expect("parse 1");
        let rebuilt = build(&f1);
        assert_eq!(rebuilt, buf, "build がバイト一致しない");
        let f2 = parse(&rebuilt).expect("parse 2");
        assert_eq!(f1, f2, "parse→build→parse が不一致");
    }

    #[test]
    fn roundtrip_get_request() {
        // Get 要求: コントローラ(05FF01) → ノードプロファイル(0EF001), EPC D6
        let buf = build(&Frame::standard(
            0x0001,
            Eoj([0x05, 0xFF, 0x01]),
            Eoj([0x0E, 0xF0, 0x01]),
            Esv::Get,
            vec![Property::get(0xD6)],
        ));
        roundtrip(&buf);
    }

    #[test]
    fn roundtrip_get_response_multi_prop() {
        let buf = build(&Frame::standard(
            0x1234,
            Eoj([0x0E, 0xF0, 0x01]),
            Eoj([0x05, 0xFF, 0x01]),
            Esv::GetRes,
            vec![
                Property::new(0xD6, vec![0x01, 0x01, 0x30, 0x01]),
                Property::new(0x80, vec![0x30]),
            ],
        ));
        roundtrip(&buf);
    }

    #[test]
    fn roundtrip_setget() {
        let frame = Frame {
            ehd1: EHD1,
            ehd2: EHD2_FORMAT1,
            tid: 0x00FF,
            edata: Edata::SetGet {
                seoj: Eoj([0x05, 0xFF, 0x01]),
                deoj: Eoj([0x01, 0x30, 0x01]),
                esv: Esv::SetGet,
                set_props: vec![Property::new(0x80, vec![0x30])],
                get_props: vec![Property::get(0xB0), Property::get(0xBB)],
            },
        };
        roundtrip(&build(&frame));
    }

    #[test]
    fn roundtrip_arbitrary_format() {
        // EHD2=0x82 任意形式: 生バイトを保持
        let buf = vec![0x10, 0x82, 0xAB, 0xCD, 0xDE, 0xAD, 0xBE, 0xEF];
        roundtrip(&buf);
        let f = parse(&buf).unwrap();
        assert!(matches!(f.edata, Edata::Arbitrary(_)));
    }

    #[test]
    fn parse_sna_rejection() {
        // Get_SNA (0x52) = 機器リジェクト
        let buf = build(&Frame::standard(
            0x0001,
            Eoj([0x01, 0x30, 0x01]),
            Eoj([0x05, 0xFF, 0x01]),
            Esv::GetSna,
            vec![Property::get(0xFF)],
        ));
        let f = parse(&buf).unwrap();
        assert_eq!(f.esv(), Some(Esv::GetSna));
        assert!(f.esv().unwrap().is_sna());
    }

    #[test]
    fn unknown_esv_does_not_break() {
        let buf = build(&Frame::standard(
            0x0001,
            Eoj([0x01, 0x30, 0x01]),
            Eoj([0x05, 0xFF, 0x01]),
            Esv::Unknown(0x99),
            vec![Property::get(0x80)],
        ));
        roundtrip(&buf);
        assert_eq!(parse(&buf).unwrap().esv(), Some(Esv::Unknown(0x99)));
    }

    #[test]
    fn truncated_frame_errors_not_panics() {
        assert!(matches!(parse(&[0x10]), Err(CodecError::Truncated(_))));
        assert!(matches!(parse(&[]), Err(CodecError::Truncated(_))));
        // PDC が EDT 長を超過
        let bad = vec![
            0x10, 0x81, 0x00, 0x01, 0x01, 0x30, 0x01, 0x05, 0xFF, 0x01, 0x62, 0x01, 0x80, 0x05,
            0x01,
        ];
        assert!(matches!(parse(&bad), Err(CodecError::Truncated(_))));
    }

    #[test]
    fn bad_ehd1_errors() {
        assert!(matches!(
            parse(&[0x20, 0x81, 0, 0]),
            Err(CodecError::BadEhd1(0x20))
        ));
    }

    #[test]
    fn esv_u8_roundtrip_all_values() {
        // from_u8 / to_u8 の手書き 2 表が食い違うと ESV 追加時に静かに壊れる。
        // 全 256 値 (既知 + Unknown) でラウンドトリップを担保する。
        for v in 0x00..=0xFFu8 {
            assert_eq!(Esv::from_u8(v).to_u8(), v, "ESV 0x{v:02X} が不一致");
        }
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(hex_to_bytes("01a3").unwrap(), vec![0x01, 0xA3]);
        assert_eq!(hex_to_bytes("0x0EF001").unwrap(), vec![0x0E, 0xF0, 0x01]);
        assert_eq!(bytes_to_hex(&[0x0E, 0xF0, 0x01]), "0ef001");
        assert!(hex_to_bytes("xyz").is_err());
        assert!(hex_to_bytes("abc").is_err()); // 奇数長
    }

    #[test]
    fn eoj_hex_roundtrip() {
        let e = Eoj::from_hex("013001").unwrap();
        assert_eq!(e.to_hex(), "013001");
        assert_eq!(e.class_group(), 0x01);
        assert!(Eoj::from_hex("0130").is_err());
    }
}
