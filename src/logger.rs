//! 轻量日志：环形内存缓冲（供 GUI 日志页）+ 追加写文件（供排错）。
//! 不引入 tracing，GUI 可直接读取缓冲并实时刷新。

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_LINES: usize = 800;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

struct Logger {
    lines: VecDeque<String>,
    path: Option<PathBuf>,
    revision: u64,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

fn inner() -> &'static Mutex<Logger> {
    LOGGER.get_or_init(|| {
        Mutex::new(Logger {
            lines: VecDeque::with_capacity(MAX_LINES),
            path: None,
            revision: 0,
        })
    })
}

pub fn init(path: Option<PathBuf>) {
    let mut g = inner().lock().unwrap();
    g.path = path;
    g.revision += 1;
}

pub fn log(level: &str, message: &str) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let line = format!("[{ts}] {level:<5} {message}");
    let mut g = inner().lock().unwrap();
    g.revision += 1;
    if g.lines.len() >= MAX_LINES {
        g.lines.pop_front();
    }
    g.lines.push_back(line.clone());
    if let Some(path) = g.path.clone() {
        drop(g);
        write_file(&path, &line);
    }
}

fn write_file(path: &PathBuf, line: &str) {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > MAX_FILE_BYTES {
            let _ = fs::write(path, b"");
        }
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn lines() -> Vec<String> {
    inner().lock().unwrap().lines.iter().cloned().collect()
}

pub fn revision() -> u64 {
    inner().lock().unwrap().revision
}

pub fn clear() {
    let mut g = inner().lock().unwrap();
    g.lines.clear();
    g.revision += 1;
}

pub fn path_string() -> String {
    inner()
        .lock()
        .unwrap()
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "（仅内存）".into())
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logger::log("INFO", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logger::log("WARN", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::logger::log("ERROR", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::globals::verbose() { $crate::logger::log("DEBUG", &format!($($arg)*)) }
    };
}
