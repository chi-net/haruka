//! 进程内日志：同时输出到终端与有界内存缓冲，供开发者页查看最近日志。

use std::collections::VecDeque;
use std::sync::LazyLock;

#[derive(Clone, serde::Serialize)]
pub struct LogEntry {
    pub at: String,
    pub level: &'static str,
    pub message: String,
}

const CAPACITY: usize = 500;

static BUFFER: LazyLock<parking_lot::RwLock<VecDeque<LogEntry>>> =
    LazyLock::new(|| parking_lot::RwLock::new(VecDeque::with_capacity(CAPACITY)));

pub fn log(level: &'static str, message: impl AsRef<str>) {
    let message = message.as_ref();
    match level {
        "WARN" | "ERROR" => eprintln!("{message}"),
        _ => println!("{message}"),
    }
    let mut buffer = BUFFER.write();
    if buffer.len() == CAPACITY {
        buffer.pop_front();
    }
    buffer.push_back(LogEntry {
        at: chrono::Utc::now().to_rfc3339(),
        level,
        message: message.to_string(),
    });
}

pub fn info(message: impl AsRef<str>) {
    log("INFO", message);
}

pub fn warn(message: impl AsRef<str>) {
    log("WARN", message);
}

pub fn error(message: impl AsRef<str>) {
    log("ERROR", message);
}

/// 最近的日志，按时间正序。
pub fn entries() -> Vec<LogEntry> {
    BUFFER.read().iter().cloned().collect()
}
