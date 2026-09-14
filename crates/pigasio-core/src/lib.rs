//! PigASIO 引擎核心。
//!
//! PigASIO 是一个通用 ASIO 驱动,和 FlexASIO 一样把 ASIO 接口桥接到
//! Windows 的通用音频 API 上。区别在于它**不限制设备的数量**:
//! FlexASIO 通过 PortAudio 的 `Pa_OpenStream` 工作,而那个 API 只允许
//! 一个输入设备加一个输出设备;PigASIO 改为“每个设备一条独立的流”,
//! 再用环形缓冲区和变速重采样把它们的时钟对齐。
//!
//! 数据流大致是这样:
//!
//! ```text
//!   ASIO 宿主                     PigASIO 引擎                    音频设备
//!  ┌─────────┐   bufferSwitch   ┌───────────────┐   ring    ┌──────────────┐
//!  │ 宿主线程 │ ───────────────► │ 混流/重采样    │ ◄───────► │ 输入设备回调  │
//!  │         │ ◄─────────────── │               │           └──────────────┘
//!  └─────────┘                  │               │   ring    ┌──────────────┐
//!                               │               │ ◄───────► │ 输出设备回调  │
//!                               └───────────────┘           └──────────────┘
//! ```
//!
//! 时钟基准取“时钟主设备”的回调,其余设备通过调整重采样比率跟随它。

pub mod channel_name;
pub mod config;
pub mod devices;
pub mod drift;
pub mod engine;
pub mod error;
pub mod log;
pub mod resample;
pub mod ring;

pub use channel_name::ChannelNames;
pub use config::{AsioSampleType, Config, DeviceRef, StreamConfig};
pub use engine::{AsioBufferSet, Engine, EngineStatus, StreamInfo, StreamStatusSnapshot};
pub use error::{Error, Result, StreamKind};

/// 驱动版本号。会写进日志,也会通过 ASIO 的 `getDriverVersion()` 返回。
pub const DRIVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// ASIO 驱动在宿主里显示的名字。
///
/// 注意 ASIO 只给 32 字节存放这个名字,超长会被截断。
pub const DRIVER_NAME: &str = "PigASIO";
