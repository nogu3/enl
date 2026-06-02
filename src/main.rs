//! enl — ECHONET Lite 専用 CLI。ステートレス / one-shot。
//!
//! stdout: 純粋な構造化 JSON (1 コマンド = 1 JSON)。
//! stderr: tracing 構造化ログ + 機械可読エラー JSON。
//! exit code: 0 成功 / 2 引数 / 3 timeout / 4 device_rejected / 5 network|bind / 1 その他。

mod codec;
mod commands;
mod error;
mod net;
mod properties;

use std::net::{IpAddr, Ipv4Addr};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use codec::{Eoj, Esv, Property};
use error::{AppError, ErrKind};

#[derive(Parser)]
#[command(
    name = "enl",
    version,
    about = "ECHONET Lite 専用 CLI (ステートレス / one-shot)"
)]
struct Cli {
    /// ローカル IPv4 (discover で CIDR 省略時に /24 を自動推定するのに使う)。
    /// 例: -i 192.168.1.130 → 192.168.1.0/24 を sweep。
    #[arg(short = 'i', long = "iface", global = true)]
    iface: Option<Ipv4Addr>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// CIDR sweep でノードを探索 (各ホストへ unicast Get 0EF001 D6)。
    Discover {
        /// 探索する CIDR (例: 192.168.1.0/24)。省略時は -i のローカル IP から /24 を推定。
        #[arg(long)]
        cidr: Option<String>,
        /// 応答収集ウィンドウ (ミリ秒)。
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
    },
    /// 指定機器のプロパティを取得。
    Get {
        /// 機器 IP。
        ip: IpAddr,
        /// 対象 EOJ (6 hex 桁, 例 013001)。
        eoj: String,
        /// 取得する EPC (2 hex 桁, 複数可, 例 80 B0)。
        #[arg(required = true, num_args = 1..)]
        epc: Vec<String>,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
    /// 指定機器のプロパティを設定 (SetC)。
    Set {
        ip: IpAddr,
        /// 対象 EOJ (6 hex 桁)。
        eoj: String,
        /// EPC (2 hex 桁)。
        epc: String,
        /// 設定値 EDT (hex, 例 30)。
        edt: String,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
    /// プロパティマップ introspection (Get/Set/状変マップ)。
    Describe {
        ip: IpAddr,
        eoj: String,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
    /// 任意 ESV/EPC/EDT を生で送り、生応答 hex を返す (デバッグ / 未対応操作の逃げ道)。
    ///
    /// 規定形式 (EHD2=0x81) の Standard フレームを 1 本送る。SNA 応答もエラーにせず
    /// response_hex を返す。応答が来ない場合のみ timeout (exit 3)。
    Raw {
        ip: IpAddr,
        /// 宛先 EOJ (DEOJ, 6 hex 桁, 例 013001)。
        eoj: String,
        /// ESV (2 hex 桁, 例 62=Get 61=SetC 63=InfReq)。
        esv: String,
        /// EPC[:EDT] の組 (例 80 か 80:30)。Get 系は EDT 省略、複数可。
        #[arg(num_args = 0..)]
        props: Vec<String>,
        /// 送信元 EOJ (SEOJ, 6 hex 桁)。省略時はコントローラ 05FF01。
        #[arg(long)]
        seoj: Option<String>,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
}

fn main() -> ExitCode {
    // tracing は stderr へ。RUST_LOG で制御。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match run(cli) {
        Ok(value) => {
            // stdout は純粋な JSON のみ。
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(e) => ExitCode::from(e.emit_and_code() as u8),
    }
}

fn run(cli: Cli) -> Result<serde_json::Value, AppError> {
    let iface = cli.iface;
    match cli.command {
        Command::Discover { cidr, timeout_ms } => {
            commands::discover(cidr.as_deref(), iface, Duration::from_millis(timeout_ms))
        }
        Command::Get {
            ip,
            eoj,
            epc,
            timeout_ms,
        } => {
            let eoj = parse_eoj(&eoj)?;
            let epcs = resolve_epcs(eoj, &epc)?;
            commands::get(ip, eoj, &epcs, Duration::from_millis(timeout_ms))
        }
        Command::Set {
            ip,
            eoj,
            epc,
            edt,
            timeout_ms,
        } => {
            let eoj = parse_eoj(&eoj)?;
            let epc = resolve_epc(eoj, &epc)?;
            let edt = resolve_edt(eoj, epc, &edt)?;
            commands::set(ip, eoj, epc, edt, Duration::from_millis(timeout_ms))
        }
        Command::Describe {
            ip,
            eoj,
            timeout_ms,
        } => {
            let eoj = parse_eoj(&eoj)?;
            commands::describe(ip, eoj, Duration::from_millis(timeout_ms))
        }
        Command::Raw {
            ip,
            eoj,
            esv,
            props,
            seoj,
            timeout_ms,
        } => {
            let deoj = parse_eoj(&eoj)?;
            let esv = parse_esv(&esv)?;
            let seoj = seoj.as_deref().map(parse_eoj).transpose()?;
            let props = props
                .iter()
                .map(|s| parse_prop_arg(s))
                .collect::<Result<Vec<_>, _>>()?;
            commands::raw(
                ip,
                deoj,
                esv,
                seoj,
                props,
                Duration::from_millis(timeout_ms),
            )
        }
    }
}

/// "62" → Esv。1 バイト hex を ESV として解釈 (未知値は Esv::Unknown で通す)。
fn parse_esv(s: &str) -> Result<Esv, AppError> {
    let bytes = codec::hex_to_bytes(s)
        .map_err(|e| AppError::new(ErrKind::Internal, format!("ESV hex 不正: {e}")))?;
    if bytes.len() != 1 {
        return Err(AppError::new(
            ErrKind::Internal,
            "ESV は 1 バイト (2 hex 桁) 必須",
        ));
    }
    Ok(Esv::from_u8(bytes[0]))
}

/// "80" or "80:30" → Property。`:` 右が EDT (省略時 PDC=0)。
fn parse_prop_arg(s: &str) -> Result<Property, AppError> {
    let (epc_s, edt_s) = match s.split_once(':') {
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    let epc = parse_epc_one(epc_s)?;
    let edt = if edt_s.is_empty() {
        Vec::new()
    } else {
        codec::hex_to_bytes(edt_s)
            .map_err(|e| AppError::new(ErrKind::Internal, format!("EDT hex 不正 '{edt_s}': {e}")))?
    };
    Ok(Property::new(epc, edt))
}

fn parse_eoj(s: &str) -> Result<Eoj, AppError> {
    Eoj::from_hex(s).map_err(|e| AppError::new(ErrKind::Internal, format!("EOJ 不正: {e}")))
}

fn resolve_epcs(eoj: Eoj, items: &[String]) -> Result<Vec<u8>, AppError> {
    items.iter().map(|s| resolve_epc(eoj, s)).collect()
}

/// EPC トークンを解決する。正規名 (例 power, operation_mode) を優先し、
/// 名前に無ければ 2 hex 桁として解釈する。raw は生送信が目的なので対象外。
fn resolve_epc(eoj: Eoj, token: &str) -> Result<u8, AppError> {
    if let Some(epc) = properties::epc_for_name(eoj, token) {
        return Ok(epc);
    }
    parse_epc_one(token).map_err(|_| {
        AppError::new(
            ErrKind::Internal,
            format!("EPC 解決失敗 '{token}' (既知の名前でも 2 hex 桁でもない)"),
        )
    })
}

/// EDT トークンを解決する。enum 型 EPC の意味名 (例 close, on) を優先し、
/// 名前に無ければ hex として解釈する (複数バイト EDT は hex 指定)。
fn resolve_edt(eoj: Eoj, epc: u8, token: &str) -> Result<Vec<u8>, AppError> {
    if let Some(b) = properties::edt_for_name(eoj, epc, token) {
        return Ok(vec![b]);
    }
    codec::hex_to_bytes(token)
        .map_err(|e| AppError::new(ErrKind::Internal, format!("EDT hex 不正: {e}")))
}

fn parse_epc_one(s: &str) -> Result<u8, AppError> {
    let bytes = codec::hex_to_bytes(s)
        .map_err(|e| AppError::new(ErrKind::Internal, format!("EPC hex 不正: {e}")))?;
    if bytes.len() != 1 {
        return Err(AppError::new(
            ErrKind::Internal,
            "EPC は 1 バイト (2 hex 桁) 必須",
        ));
    }
    Ok(bytes[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_esv_known_and_unknown() {
        assert_eq!(parse_esv("62").unwrap(), Esv::Get);
        assert_eq!(parse_esv("61").unwrap(), Esv::SetC);
        assert_eq!(parse_esv("99").unwrap(), Esv::Unknown(0x99));
        assert!(parse_esv("6201").is_err()); // 2 バイトは不可
        assert!(parse_esv("zz").is_err());
    }

    #[test]
    fn parse_prop_arg_get_form() {
        // EDT 省略 → PDC=0
        assert_eq!(parse_prop_arg("80").unwrap(), Property::new(0x80, vec![]));
    }

    #[test]
    fn parse_prop_arg_set_form() {
        assert_eq!(
            parse_prop_arg("80:30").unwrap(),
            Property::new(0x80, vec![0x30])
        );
        // 複数バイト EDT
        assert_eq!(
            parse_prop_arg("d6:0101300 1".replace(' ', "").as_str()).unwrap(),
            Property::new(0xD6, vec![0x01, 0x01, 0x30, 0x01])
        );
    }

    #[test]
    fn parse_prop_arg_errors() {
        assert!(parse_prop_arg("8030:30").is_err()); // EPC は 1 バイト
        assert!(parse_prop_arg("80:zz").is_err()); // EDT hex 不正
    }

    #[test]
    fn resolve_epc_name_or_hex() {
        let aircon = Eoj::from_hex("013001").unwrap();
        // 正規名
        assert_eq!(resolve_epc(aircon, "operation_mode").unwrap(), 0xB0);
        assert_eq!(resolve_epc(aircon, "power").unwrap(), 0x80);
        // hex フォールバック
        assert_eq!(resolve_epc(aircon, "B0").unwrap(), 0xB0);
        assert_eq!(resolve_epc(aircon, "ff").unwrap(), 0xFF);
        // 名前でも hex でもない
        assert!(resolve_epc(aircon, "bogus").is_err());
    }

    #[test]
    fn resolve_epc_is_class_scoped() {
        // open_close_state は雨戸クラスのみ。エアコンでは解決不可。
        let aircon = Eoj::from_hex("013001").unwrap();
        let shutter = Eoj::from_hex("026301").unwrap();
        assert_eq!(resolve_epc(shutter, "open_close_state").unwrap(), 0xEA);
        assert!(resolve_epc(aircon, "open_close_state").is_err());
    }

    #[test]
    fn resolve_edt_name_or_hex() {
        let shutter = Eoj::from_hex("026301").unwrap();
        // enum 意味名
        assert_eq!(resolve_edt(shutter, 0xE0, "close").unwrap(), vec![0x42]);
        assert_eq!(resolve_edt(shutter, 0xE0, "open").unwrap(), vec![0x41]);
        // hex フォールバック
        assert_eq!(resolve_edt(shutter, 0xE0, "42").unwrap(), vec![0x42]);
        // 数値型 EPC は名前無し → hex のみ
        assert_eq!(resolve_edt(shutter, 0xE1, "32").unwrap(), vec![0x32]);
        // 名前でも hex でもない
        assert!(resolve_edt(shutter, 0xE0, "bogus").is_err());
    }
}
