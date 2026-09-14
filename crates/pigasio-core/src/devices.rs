//! 设备枚举与配置匹配。
//!
//! 配置里的设备是用“名字片段”或“正则”描述的,因为 Windows 上设备的
//! 持久标识符(`IMMDevice` 的 endpoint id)会随驱动更新而变,写死在
//! 配置里很容易失效。这里负责把描述解析成实际可以打开的 `cpal::Device`。

use crate::config::{DeviceRef, StreamConfig};
use crate::error::{Error, Result, StreamKind};

// cpal 把设备枚举做成了 trait 方法,需要显式引入。
use cpal::traits::{DeviceTrait, HostTrait};

/// 一次枚举得到的设备信息。
///
/// `cpal::Device` 内部是引用计数的句柄,克隆代价很低。
#[derive(Clone)]
pub struct DeviceInfo {
    /// 设备在系统里显示的名字。
    pub name: String,
    /// 该设备在查询方向上提供的最大通道数。
    pub max_channels: usize,
    /// 设备偏好(默认)的采样率。
    pub default_sample_rate: u32,
    /// 打开设备用的句柄。
    pub device: cpal::Device,
}

impl std::fmt::Debug for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceInfo")
            .field("name", &self.name)
            .field("max_channels", &self.max_channels)
            .field("default_sample_rate", &self.default_sample_rate)
            .finish_non_exhaustive()
    }
}

/// 枚举指定方向上的所有设备。
///
/// 单个端点查询失败不会中断整个流程:Windows 上偶尔会有端点处于异常
/// 状态,我们会跳过它并记一条日志,而不是让驱动初始化彻底失败。
pub fn enumerate(kind: StreamKind) -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();

    let devices = match kind {
        StreamKind::Input => host.input_devices(),
        StreamKind::Output => host.output_devices(),
    }
    .map_err(|e| Error::Backend(format!("枚举{}设备失败:{e}", kind.as_str())))?;

    let mut result = Vec::new();
    for device in devices {
        let name = match device.name() {
            Ok(n) => n,
            Err(e) => {
                log::warn!("跳过无法读取名字的{}设备:{e}", kind.as_str());
                continue;
            }
        };

        // 查询该方向上的默认配置。查询失败说明这个端点在当前方向不可用。
        let (max_channels, default_sample_rate) = match kind {
            StreamKind::Input => match device.default_input_config() {
                Ok(cfg) => (cfg.channels() as usize, cfg.sample_rate().0),
                Err(e) => {
                    log::debug!("设备 “{name}” 无法作为输入打开:{e}");
                    continue;
                }
            },
            StreamKind::Output => match device.default_output_config() {
                Ok(cfg) => (cfg.channels() as usize, cfg.sample_rate().0),
                Err(e) => {
                    log::debug!("设备 “{name}” 无法作为输出打开:{e}");
                    continue;
                }
            },
        };

        result.push(DeviceInfo {
            name,
            max_channels,
            default_sample_rate,
            device,
        });
    }

    Ok(result)
}

/// 同时枚举输入和输出设备。
pub fn enumerate_both() -> Result<(Vec<DeviceInfo>, Vec<DeviceInfo>)> {
    Ok((enumerate(StreamKind::Input)?, enumerate(StreamKind::Output)?))
}

/// 取得系统默认设备。
pub fn default_device(kind: StreamKind) -> Result<DeviceInfo> {
    let host = cpal::default_host();
    let device = match kind {
        StreamKind::Input => host.default_input_device(),
        StreamKind::Output => host.default_output_device(),
    }
    .ok_or_else(|| Error::DeviceNotFound {
        kind,
        spec: "default".to_string(),
        available: Vec::new(),
    })?;

    let name = device.name()?;
    let (max_channels, default_sample_rate) = match kind {
        StreamKind::Input => {
            let cfg = device.default_input_config()?;
            (cfg.channels() as usize, cfg.sample_rate().0)
        }
        StreamKind::Output => {
            let cfg = device.default_output_config()?;
            (cfg.channels() as usize, cfg.sample_rate().0)
        }
    };

    Ok(DeviceInfo {
        name,
        max_channels,
        default_sample_rate,
        device,
    })
}

/// 按配置描述在已枚举的设备里找到目标设备。
///
/// 匹配规则(与 FlexASIO 的语义保持一致):
/// * `Default` —— 系统默认设备;
/// * `Substring` —— 名字包含该片段,忽略大小写;
/// * `Regex` —— 名字匹配该正则(部分匹配);
/// * `None` —— 调用方应该在调用前过滤掉,这里会返回错误。
///
/// 匹配到多个设备时取第一个并记一条警告 —— 宁可可用也不要因为歧义
/// 让整个驱动起不来,同时日志里会留下线索。
pub fn resolve(reference: &DeviceRef, kind: StreamKind) -> Result<DeviceInfo> {
    match reference {
        DeviceRef::None => Err(Error::Config(format!(
            "{}设备被配置为 none,不应该走到解析流程",
            kind.as_str()
        ))),
        DeviceRef::Default => default_device(kind),
        DeviceRef::Substring(needle) => {
            let candidates = enumerate(kind)?;
            let needle_lower = needle.to_lowercase();
            let matches: Vec<_> = candidates
                .iter()
                .filter(|d| d.name.to_lowercase().contains(&needle_lower))
                .cloned()
                .collect();
            pick(&matches, needle, kind, &candidates)
        }
        DeviceRef::Regex(pattern) => {
            let re = regex::Regex::new(pattern)
                .map_err(|e| Error::Config(format!("无效的设备正则 “{pattern}”:{e}")))?;
            let candidates = enumerate(kind)?;
            let matches: Vec<_> = candidates
                .iter()
                .filter(|d| re.is_match(&d.name))
                .cloned()
                .collect();
            pick(&matches, pattern, kind, &candidates)
        }
    }
}

fn pick(
    matches: &[DeviceInfo],
    spec: &str,
    kind: StreamKind,
    all: &[DeviceInfo],
) -> Result<DeviceInfo> {
    match matches {
        [] => Err(Error::DeviceNotFound {
            kind,
            spec: spec.to_string(),
            available: all.iter().map(|d| d.name.clone()).collect(),
        }),
        [only] => Ok(only.clone()),
        [first, rest @ ..] => {
            log::warn!(
                "“{spec}” 匹配到 {} 个{}设备,使用第一个 “{}”(其他: {})",
                matches.len(),
                kind.as_str(),
                first.name,
                rest.iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Ok(first.clone())
        }
    }
}

/// 检查设备是否有配置要求的那些通道。
///
/// 只依赖设备名和可用通道数,不碰设备句柄,方便单测。
pub fn check_channels(device_name: &str, available: usize, cfg: &StreamConfig) -> Result<()> {
    let requested = cfg.channels.expand();
    if let Some(&max) = requested.iter().max() {
        if max >= available {
            return Err(Error::ChannelOutOfRange {
                name: device_name.to_string(),
                requested,
                available,
            });
        }
    }
    Ok(())
}

/// 检查同一方向上是否有设备被重复配置且通道重叠。
///
/// 同一个物理设备在同一方向上打开两条流在 WASAPI 共享模式下是允许的,
/// 但会让同一路信号被采集/播放两次,还会给时钟同步制造无谓的负担。
/// 通道完全不重叠时放行 —— 那种配置是有意义的(比如一块多路声卡
/// 的不同物理接口分别接到不同的 ASIO 通道组)。
pub fn check_duplicates(resolved: &[(&str, &StreamConfig)], kind: StreamKind) -> Result<()> {
    let mut seen: Vec<(&str, Vec<usize>)> = Vec::new();
    for (name, cfg) in resolved {
        let mut channels = cfg.channels.expand();
        if let Some(entry) = seen.iter_mut().find(|(n, _)| *n == *name) {
            let overlap: Vec<_> = channels
                .iter()
                .copied()
                .filter(|c| entry.1.contains(c))
                .collect();
            if !overlap.is_empty() {
                return Err(Error::Config(format!(
                    "{}设备 “{}” 被重复配置,且通道 {overlap:?} 冲突;\
                     如果确实想用同一声卡的不同接口,请让通道互不重叠",
                    kind.as_str(),
                    name
                )));
            }
            entry.1.append(&mut channels);
        } else {
            seen.push((name, channels));
        }
    }
    Ok(())
}

/// 生成一份人类可读的设备清单,用于控制面板和 `pigasio devices`。
pub fn describe_all() -> Result<String> {
    let mut out = String::new();
    for kind in [StreamKind::Input, StreamKind::Output] {
        out.push_str(&format!("=== {}设备 ===\n", kind.as_str()));
        let devices = enumerate(kind)?;
        if devices.is_empty() {
            out.push_str("  (无)\n");
        }
        for (i, d) in devices.iter().enumerate() {
            out.push_str(&format!(
                "  [{i}] {} —— {} 通道,默认 {} Hz\n",
                d.name, d.max_channels, d.default_sample_rate
            ));
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelSelection;

    #[test]
    fn 通道越界会被拒绝() {
        let ok = StreamConfig {
            channels: ChannelSelection::List(vec![0, 1]),
            ..StreamConfig::default()
        };
        assert!(check_channels("测试设备", 2, &ok).is_ok());

        let bad = StreamConfig {
            channels: ChannelSelection::List(vec![0, 5]),
            ..StreamConfig::default()
        };
        assert!(matches!(
            check_channels("测试设备", 2, &bad),
            Err(Error::ChannelOutOfRange { .. })
        ));
    }

    #[test]
    fn 同方向重复设备且通道重叠会被拒绝() {
        let a = StreamConfig {
            channels: ChannelSelection::Count(2),
            ..StreamConfig::default()
        };
        let resolved = vec![("声卡", &a), ("声卡", &a)];
        assert!(check_duplicates(&resolved, StreamKind::Output).is_err());
    }

    #[test]
    fn 同设备但通道不重叠时放行() {
        let a = StreamConfig {
            channels: ChannelSelection::List(vec![0, 1]),
            ..StreamConfig::default()
        };
        let b = StreamConfig {
            channels: ChannelSelection::List(vec![2, 3]),
            ..StreamConfig::default()
        };
        let resolved = vec![("声卡", &a), ("声卡", &b)];
        assert!(check_duplicates(&resolved, StreamKind::Output).is_ok());
    }

    #[test]
    fn 不同设备互不干扰() {
        let a = StreamConfig {
            channels: ChannelSelection::Count(2),
            ..StreamConfig::default()
        };
        let resolved = vec![("声卡 A", &a), ("声卡 B", &a)];
        assert!(check_duplicates(&resolved, StreamKind::Output).is_ok());
    }
}
