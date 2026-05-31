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

use std::net::IpAddr;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use codec::Eoj;
use error::{AppError, ErrKind};

#[derive(Parser)]
#[command(name = "enl", version, about = "ECHONET Lite 専用 CLI (ステートレス / one-shot)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// マルチキャストでノードを探索 (Get 0xD6)。
    Discover {
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
    match cli.command {
        Command::Discover { timeout_ms } => commands::discover(Duration::from_millis(timeout_ms)),
        Command::Get { ip, eoj, epc, timeout_ms } => {
            let eoj = parse_eoj(&eoj)?;
            let epcs = parse_epcs(&epc)?;
            commands::get(ip, eoj, &epcs, Duration::from_millis(timeout_ms))
        }
        Command::Set { ip, eoj, epc, edt, timeout_ms } => {
            let eoj = parse_eoj(&eoj)?;
            let epc = parse_epc_one(&epc)?;
            let edt = codec::hex_to_bytes(&edt)
                .map_err(|e| AppError::new(ErrKind::Internal, format!("EDT hex 不正: {e}")))?;
            commands::set(ip, eoj, epc, edt, Duration::from_millis(timeout_ms))
        }
        Command::Describe { ip, eoj, timeout_ms } => {
            let eoj = parse_eoj(&eoj)?;
            commands::describe(ip, eoj, Duration::from_millis(timeout_ms))
        }
    }
}

fn parse_eoj(s: &str) -> Result<Eoj, AppError> {
    Eoj::from_hex(s).map_err(|e| AppError::new(ErrKind::Internal, format!("EOJ 不正: {e}")))
}

fn parse_epcs(items: &[String]) -> Result<Vec<u8>, AppError> {
    items.iter().map(|s| parse_epc_one(s)).collect()
}

fn parse_epc_one(s: &str) -> Result<u8, AppError> {
    let bytes = codec::hex_to_bytes(s)
        .map_err(|e| AppError::new(ErrKind::Internal, format!("EPC hex 不正: {e}")))?;
    if bytes.len() != 1 {
        return Err(AppError::new(ErrKind::Internal, "EPC は 1 バイト (2 hex 桁) 必須"));
    }
    Ok(bytes[0])
}
