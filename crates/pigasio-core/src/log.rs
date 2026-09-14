//! 轻量日志。
//!
//! 沿用 FlexASIO 的思路:日志默认关闭,只要用户目录下存在 `PigASIO.log`
//! 文件就自动开启并把所有细节写进去。音频驱动跑在宿主进程里,不能弹窗、
//! 不能依赖控制台,所以“文件存在即开启”是最省事的开关。
//!
//! 另外支持环境变量 `PIGASIO_LOG` 指向任意路径,便于把日志写到别处。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// 单个日志文件的上限。超过后停止写入,避免把用户的磁盘写满。
/// FlexASIO 用的是 1 GB,这里保持一致。
const MAX_LOG_BYTES: u64 = 1024 * 1024 * 1024;

static LOGGER: Logger = Logger::new();
static INSTALLED: AtomicBool = AtomicBool::new(false);

struct Logger {
    sink: Mutex<Option<Sink>>,
    /// 记录已经写入的字节数,用于执行 MAX_LOG_BYTES 限制。
    written: AtomicU64,
    /// 是否已经因为超限而停止写入,避免重复打印提示。
    truncated: AtomicBool,
}

enum Sink {
    File { file: File, #[allow(dead_code)] path: PathBuf },
    Stderr,
}

impl Logger {
    const fn new() -> Self {
        Logger {
            sink: Mutex::new(None),
            written: AtomicU64::new(0),
            truncated: AtomicBool::new(false),
        }
    }

    fn open(&self, path: &Path) -> std::io::Result<()> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let existing = file.metadata().map(|m| m.len()).unwrap_or(0);
        self.written.store(existing, Ordering::Relaxed);
        *self.sink.lock().unwrap() = Some(Sink::File {
            file,
            path: path.to_path_buf(),
        });
        Ok(())
    }

    fn use_stderr(&self) {
        *self.sink.lock().unwrap() = Some(Sink::Stderr);
    }

    fn is_enabled(&self) -> bool {
        self.sink.lock().map(|s| s.is_some()).unwrap_or(false)
    }
}

/// 把日志写到文件。
///
/// 时间戳使用毫秒精度的 UNIX 时间。音频线程对时间敏感,所以这里刻意
/// 避免任何可能阻塞的格式化操作 —— 单次 `write_all` 到一个已经打开的
/// 文件句柄通常足够快。
impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        if !self.is_enabled() {
            return false;
        }
        should_record(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.is_enabled() || !should_record(record.metadata()) {
            return;
        }
        let mut guard = match self.sink.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(sink) = guard.as_mut() else {
            return;
        };

        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        // 音频回调里不能用 println!,这里统一走一个格式化缓冲再一次性写出。
        let line = format!(
            "[{millis:>13}][{:<5}][{}] {}\n",
            record.level(),
            record.target(),
            record.args()
        );

        match sink {
            Sink::Stderr => {
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
            Sink::File { file, .. } => {
                if self.written.load(Ordering::Relaxed) >= MAX_LOG_BYTES {
                    if !self.truncated.swap(true, Ordering::Relaxed) {
                        let _ = file.write_all(
                            b"[PigASIO] log file reached 1 GB, further messages are dropped\n",
                        );
                        let _ = file.flush();
                    }
                    return;
                }
                let _ = file.write_all(line.as_bytes());
                self.written.fetch_add(line.len() as u64, Ordering::Relaxed);
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.sink.lock() {
            if let Some(Sink::File { file, .. }) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// 日志初始化结果,方便调用方在控制台提示用户。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogStatus {
    /// 未开启日志(这是默认状态)。
    Disabled,
    /// 已开始写入指定路径。
    Enabled(PathBuf),
    /// 用户要求写日志,但文件打不开 —— 附带原因。
    Failed(PathBuf, String),
}

/// 决定日志文件路径。
///
/// 优先级:`PIGASIO_LOG` 环境变量 > 用户目录下的 `PigASIO.log`。
/// 两者都不存在则返回 `None`,表示不开启日志。
pub fn resolve_log_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("PIGASIO_LOG") {
        if !explicit.trim().is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }

    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    let candidate = home.join("PigASIO.log");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// 安装全局日志器。重复调用是安全的,但只有第一次生效。
pub fn init() -> LogStatus {
    let Some(path) = resolve_log_path() else {
        return LogStatus::Disabled;
    };

    if let Err(e) = LOGGER.open(&path) {
        return LogStatus::Failed(path, e.to_string());
    }

    install();
    // 这一行不能省:`log` crate 的默认最大级别是 `Off`,不显式设置的话
    // 即使 logger 装上了,所有记录也会被丢弃 —— 文件创建了,但永远是空的。
    log::set_max_level(file_log_level());
    LogStatus::Enabled(path)
}

/// 文件日志的级别,可用 `PIGASIO_LOG_LEVEL` 覆盖。
///
/// 默认 `Trace`。驱动日志本来就是给排错用的,用户既然手动创建了
/// `PigASIO.log`,想看的显然是全部细节。
fn file_log_level() -> LevelFilter {
    match std::env::var("PIGASIO_LOG_LEVEL") {
        Ok(value) => parse_level(&value).unwrap_or_else(|| {
            log::warn!("PIGASIO_LOG_LEVEL 的值 “{value}” 无法识别,改用 trace");
            LevelFilter::Trace
        }),
        Err(_) => LevelFilter::Trace,
    }
}

/// 解析日志级别名。大小写不敏感。
fn parse_level(s: &str) -> Option<LevelFilter> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" | "warning" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}

/// 当 `init()` 没有开启文件日志时,提供一个把日志送到 stderr 的兜底方案。
///
/// 命令行工具和自检会用到它;驱动 DLL 不应该调用,因为宿主进程里没有
/// 可供用户查看的控制台。
pub fn init_stderr(level: LevelFilter) {
    LOGGER.use_stderr();
    install();
    log::set_max_level(level);
}

fn install() {
    if !INSTALLED.swap(true, Ordering::SeqCst) {
        let _ = log::set_logger(&LOGGER);
    }
}

/// 判断一条记录是否值得写进日志。
///
/// 我们自己 crate 的记录全收;第三方(eframe、winit、glutin……)只收
/// 警告及以上。那些库在 TRACE 级别会刷出海量内容 —— 实测控制面板启动
/// 8 秒就能写出 117 KB —— 足以把真正有用的驱动日志淹掉,日志文件也会
/// 涨得飞快。
///
/// 需要看第三方细节时(排查渲染、窗口创建之类的问题),设
/// `PIGASIO_LOG_ALL=1` 关掉这个过滤。
fn should_record(metadata: &Metadata) -> bool {
    if all_targets_enabled() {
        return true;
    }
    if metadata.target().starts_with("pigasio") {
        return true;
    }
    metadata.level() <= Level::Warn
}

/// 是否关闭了按来源的过滤。首次调用时读环境变量,之后走缓存。
fn all_targets_enabled() -> bool {
    static ALL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALL.get_or_init(|| {
        std::env::var("PIGASIO_LOG_ALL")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}

/// 记录一条驱动启动横幅,方便在日志里快速定位一次运行。
pub fn banner(driver_version: &str) {
    log::info!("================================================================");
    log::info!("PigASIO {driver_version} —— 多设备 ASIO 驱动");
    log::info!("================================================================");
}

/// 供日志级别过滤使用。
pub fn level_enabled(level: Level) -> bool {
    log::log_enabled!(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 日志级别名解析大小写不敏感且容忍空格() {
        assert_eq!(parse_level("trace"), Some(LevelFilter::Trace));
        assert_eq!(parse_level("  DEBUG "), Some(LevelFilter::Debug));
        assert_eq!(parse_level("Warn"), Some(LevelFilter::Warn));
        assert_eq!(parse_level("warning"), Some(LevelFilter::Warn));
        assert_eq!(parse_level("Off"), Some(LevelFilter::Off));
        assert_eq!(parse_level(""), None);
        assert_eq!(parse_level("乱写的"), None);
    }
}
