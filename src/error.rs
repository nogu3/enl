//! 機械可読エラー。stderr に JSON、終了コードに反映。

use serde_json::json;

/// エラー種別。stderr JSON の "kind" と exit code に対応。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    Timeout,        // exit 3
    DeviceRejected, // exit 4 (SNA)
    Network,        // exit 5
    Bind,           // exit 5
    Parse,          // exit 1
    Usage,          // exit 1 (enl 側で検出した入力不正。clap の exit 2 とは別)
}

impl ErrKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrKind::Timeout => "timeout",
            ErrKind::DeviceRejected => "device_rejected",
            ErrKind::Network => "network",
            ErrKind::Bind => "bind",
            ErrKind::Parse => "parse",
            ErrKind::Usage => "usage",
        }
    }

    /// cron / n8n が分岐できる exit code。
    pub fn exit_code(self) -> i32 {
        match self {
            ErrKind::Timeout => 3,
            ErrKind::DeviceRejected => 4,
            ErrKind::Network | ErrKind::Bind => 5,
            ErrKind::Parse | ErrKind::Usage => 1,
        }
    }
}

#[derive(Debug)]
pub struct AppError {
    pub kind: ErrKind,
    pub detail: String,
    /// 追加のコンテキスト (機器が返した SNA プロパティ等)。
    pub extra: Option<serde_json::Value>,
}

impl AppError {
    pub fn new(kind: ErrKind, detail: impl Into<String>) -> AppError {
        AppError {
            kind,
            detail: detail.into(),
            extra: None,
        }
    }

    pub fn with_extra(mut self, extra: serde_json::Value) -> AppError {
        self.extra = Some(extra);
        self
    }

    /// stderr に構造化 JSON エラーを出し、対応 exit code を返す。
    pub fn emit_and_code(&self) -> i32 {
        let mut err = json!({ "kind": self.kind.as_str(), "detail": self.detail });
        if let Some(extra) = &self.extra {
            err["context"] = extra.clone();
        }
        let payload = json!({ "error": err });
        // tracing でなく素の stderr JSON (機械可読 1 行)。
        eprintln!("{payload}");
        tracing::error!(kind = self.kind.as_str(), detail = %self.detail, "コマンド失敗");
        self.kind.exit_code()
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.detail)
    }
}

impl std::error::Error for AppError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_kind_maps_to_exit_1() {
        assert_eq!(ErrKind::Usage.as_str(), "usage");
        assert_eq!(ErrKind::Usage.exit_code(), 1);
    }
}
