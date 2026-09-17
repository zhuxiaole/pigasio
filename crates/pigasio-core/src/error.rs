//! PigASIO 的错误类型。
//!
//! 引擎层在打开设备、协商格式、启动流时会产生各种失败。这里的错误类型
//! 会尽量携带足够的上下文,因为最终它们需要通过 ASIO 的 `getErrorMessage()`
//! 反馈给宿主软件 —— 而 ASIO 只允许返回 124 字节的文本。

use std::fmt;

/// PigASIO 引擎的统一错误类型。
#[derive(Debug)]
pub enum Error {
    /// 配置文件语法或语义错误。
    Config(String),
    /// 找不到符合描述的音频设备。
    DeviceNotFound {
        kind: StreamKind,
        spec: String,
        available: Vec<String>,
    },
    /// 设备存在,但无法按要求打开(通道数、采样率、独占模式等)。
    DeviceOpen { name: String, reason: String },
    /// 设备报告的通道数不足以满足配置请求。
    ChannelOutOfRange {
        name: String,
        requested: Vec<usize>,
        available: usize,
    },
    /// 无法构造重采样器。
    Resampler(String),
    /// 流已经启动,不能再重复启动。
    AlreadyRunning,
    /// 引擎尚未启动。
    NotRunning,
    /// 底层音频后端(当前为 cpal/WASAPI)报错。
    Backend(String),
    /// 系统调用失败(注册表、文件、DLL 注册等)。
    Platform(String),
    /// 用户配置中引用的时钟主设备不存在。
    ClockMasterNotFound(String),
    /// 内部逻辑错误,通常意味着有 bug。
    Internal(String),
}

/// 流的种类 —— 输入(录音)或输出(播放)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StreamKind {
    Input,
    Output,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamKind::Input => "输入",
            StreamKind::Output => "输出",
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(msg) => write!(f, "配置错误:{msg}"),
            Error::DeviceNotFound {
                kind,
                spec,
                available,
            } => {
                write!(f, "找不到{kind}设备 “{spec}”")?;
                if available.is_empty() {
                    write!(f, ";系统没有报告任何可用设备")
                } else {
                    write!(f, ";可用设备:{}", available.join(" | "))
                }
            }
            Error::DeviceOpen { name, reason } => write!(f, "打开设备 “{name}” 失败:{reason}"),
            Error::ChannelOutOfRange {
                name,
                requested,
                available,
            } => write!(
                f,
                "设备 “{name}” 只有 {available} 个通道,但配置请求了通道 {requested:?}"
            ),
            Error::Resampler(msg) => write!(f, "重采样器初始化失败:{msg}"),
            Error::AlreadyRunning => write!(f, "引擎已经在运行"),
            Error::NotRunning => write!(f, "引擎尚未启动"),
            Error::Backend(msg) => write!(f, "音频后端错误:{msg}"),
            Error::Platform(msg) => write!(f, "系统调用错误:{msg}"),
            Error::ClockMasterNotFound(spec) => {
                write!(f, "配置的时钟主设备 “{spec}” 不在已配置的设备列表中")
            }
            Error::Internal(msg) => write!(f, "内部错误:{msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rubato::ResamplerConstructionError> for Error {
    fn from(e: rubato::ResamplerConstructionError) -> Self {
        Error::Resampler(e.to_string())
    }
}

impl From<rubato::ResampleError> for Error {
    fn from(e: rubato::ResampleError) -> Self {
        Error::Resampler(format!("重采样失败:{e}"))
    }
}

/// 引擎使用的 Result 别名。
pub type Result<T> = std::result::Result<T, Error>;
