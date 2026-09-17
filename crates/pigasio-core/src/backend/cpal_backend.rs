//! `cpal` 后端。
//!
//! 这里是从 `devices.rs` / `engine.rs` 搬过来的原有实现,**音频行为保持
//! 不变**。唯一的差别是把设备格式到 `f32` 的转换收拢到了这一层 —— 引擎
//! 侧那套 `build_input::<T>` 泛型因此可以去掉。

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::error::{Error, Result, StreamKind};

use super::{
    Backend, DeviceHandle, DeviceInfo, DeviceSampleFormat, ErrorCallback, InputCallback,
    OutputCallback, StreamFormat, StreamHandle, StreamRequest,
};

/// 基于 cpal 的后端。
pub struct CpalBackend;

impl CpalBackend {
    pub fn new() -> Self {
        CpalBackend
    }
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for CpalBackend {
    fn name(&self) -> &'static str {
        "cpal"
    }

    fn enumerate(&self, kind: StreamKind) -> Result<Vec<DeviceInfo>> {
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
                name: name.clone(),
                max_channels,
                default_sample_rate,
                handle: Arc::new(CpalDevice { name, device }),
            });
        }

        Ok(result)
    }

    fn default_device(&self, kind: StreamKind) -> Result<DeviceInfo> {
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

        let name = device
            .name()
            .map_err(|e| Error::Backend(format!("读取设备名称失败:{e}")))?;
        let (max_channels, default_sample_rate) = match kind {
            StreamKind::Input => {
                let cfg = device
                    .default_input_config()
                    .map_err(|e| Error::Backend(format!("读取设备默认格式失败:{e}")))?;
                (cfg.channels() as usize, cfg.sample_rate().0)
            }
            StreamKind::Output => {
                let cfg = device
                    .default_output_config()
                    .map_err(|e| Error::Backend(format!("读取设备默认格式失败:{e}")))?;
                (cfg.channels() as usize, cfg.sample_rate().0)
            }
        };

        Ok(DeviceInfo {
            name: name.clone(),
            max_channels,
            default_sample_rate,
            handle: Arc::new(CpalDevice { name, device }),
        })
    }
}

/// cpal 的设备句柄。
struct CpalDevice {
    /// 报错时要带上设备名。
    name: String,
    device: cpal::Device,
}

impl DeviceHandle for CpalDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn negotiate(&self, kind: StreamKind, request: &StreamRequest) -> Result<StreamFormat> {
        let target = cpal::SampleRate(request.sample_rate);

        let supported: Vec<_> = match kind {
            StreamKind::Input => self
                .device
                .supported_input_configs()
                .map_err(|e| Error::DeviceOpen {
                    name: self.name.clone(),
                    reason: format!("查询支持的输入格式失败:{e}"),
                })?
                .collect(),
            StreamKind::Output => self
                .device
                .supported_output_configs()
                .map_err(|e| Error::DeviceOpen {
                    name: self.name.clone(),
                    reason: format!("查询支持的输出格式失败:{e}"),
                })?
                .collect(),
        };

        // 优先挑**原生 f32**:它是 Windows 音频引擎内部用的格式,共享模式下
        // 几乎总是可用,而且省掉一次格式转换。找不到就退回设备默认配置。
        let pick = supported
            .iter()
            .find(|r| {
                r.sample_format() == cpal::SampleFormat::F32
                    && r.min_sample_rate() <= target
                    && target <= r.max_sample_rate()
            })
            .or_else(|| {
                supported
                    .iter()
                    .find(|r| r.sample_format() == cpal::SampleFormat::F32)
            });

        if let Some(range) = pick {
            let rate = clamp_rate(target, range.min_sample_rate(), range.max_sample_rate());
            if rate.0 != request.sample_rate {
                log::warn!(
                    "设备 “{}” 不支持 {} Hz,改用 {} Hz 并重采样",
                    self.name,
                    request.sample_rate,
                    rate.0
                );
            }
            return Ok(StreamFormat {
                sample_rate: rate.0,
                channels: range.channels() as usize,
                sample_format: DeviceSampleFormat::F32,
            });
        }

        let default = match kind {
            StreamKind::Input => self.device.default_input_config(),
            StreamKind::Output => self.device.default_output_config(),
        }
        .map_err(|e| Error::DeviceOpen {
            name: self.name.clone(),
            reason: format!("读取设备默认格式失败:{e}"),
        })?;

        let sample_format = match default.sample_format() {
            cpal::SampleFormat::F32 => DeviceSampleFormat::F32,
            cpal::SampleFormat::I16 => DeviceSampleFormat::I16,
            cpal::SampleFormat::I32 => DeviceSampleFormat::I32,
            cpal::SampleFormat::U16 => DeviceSampleFormat::U16,
            other => {
                return Err(Error::DeviceOpen {
                    name: self.name.clone(),
                    reason: format!(
                        "{kind}流不支持采样格式 {other:?};\
                         PigASIO 目前支持 f32 / i16 / i32 / u16"
                    ),
                });
            }
        };

        log::warn!(
            "设备 “{}” 没有可用的 f32 共享模式格式,改用 {sample_format:?}",
            self.name
        );
        Ok(StreamFormat {
            sample_rate: default.sample_rate().0,
            channels: default.channels() as usize,
            sample_format,
        })
    }

    fn open_input(
        &self,
        format: &StreamFormat,
        on_data: InputCallback,
        on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>> {
        match format.sample_format {
            DeviceSampleFormat::F32 => self.build_input::<f32>(format, on_data, on_error),
            DeviceSampleFormat::I16 => self.build_input::<i16>(format, on_data, on_error),
            DeviceSampleFormat::I32 => self.build_input::<i32>(format, on_data, on_error),
            DeviceSampleFormat::U16 => self.build_input::<u16>(format, on_data, on_error),
        }
    }

    fn open_output(
        &self,
        format: &StreamFormat,
        on_data: OutputCallback,
        on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>> {
        match format.sample_format {
            DeviceSampleFormat::F32 => self.build_output::<f32>(format, on_data, on_error),
            DeviceSampleFormat::I16 => self.build_output::<i16>(format, on_data, on_error),
            DeviceSampleFormat::I32 => self.build_output::<i32>(format, on_data, on_error),
            DeviceSampleFormat::U16 => self.build_output::<u16>(format, on_data, on_error),
        }
    }
}

impl CpalDevice {
    fn config_of(&self, format: &StreamFormat) -> cpal::StreamConfig {
        cpal::StreamConfig {
            channels: format.channels as u16,
            sample_rate: cpal::SampleRate(format.sample_rate),
            // 共享模式下缓冲大小由系统音频引擎决定,请求具体值没有意义 ——
            // 这也是 cpal 后端拿不到低延迟 period 的原因,见模块文档。
            buffer_size: cpal::BufferSize::Default,
        }
    }

    /// 输入方向:设备原生格式 → `f32` 交错 → 引擎。
    fn build_input<T>(
        &self,
        format: &StreamFormat,
        mut on_data: InputCallback,
        mut on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>>
    where
        T: cpal::SizedSample + cpal::Sample + Send + 'static,
        f32: cpal::FromSample<T>,
    {
        use cpal::Sample as _;
        let config = self.config_of(format);
        let mut scratch: Vec<f32> = Vec::new();

        let stream = self
            .device
            .build_input_stream::<T, _, _>(
                &config,
                move |data: &[T], _info: &cpal::InputCallbackInfo| {
                    if scratch.len() < data.len() {
                        scratch.resize(data.len(), 0.0);
                    }
                    for (dst, src) in scratch.iter_mut().zip(data.iter()) {
                        *dst = f32::from_sample(*src);
                    }
                    // 只把**这一块**这么多交出去。scratch 是复用的,它可能比本块
                    // 长(设备块变小过),而引擎是按长度反推设备块大小的 ——
                    // 多传一个样本,它就会以为设备一次给了那么多帧。
                    on_data(&scratch[..data.len()]);
                },
                move |e| on_error(e.to_string()),
                None,
            )
            .map_err(|e| Error::DeviceOpen {
                name: self.name.clone(),
                reason: e.to_string(),
            })?;

        Ok(Box::new(CpalStream { stream }))
    }

    /// 输出方向:引擎的 `f32` 交错 → 设备原生格式。
    fn build_output<T>(
        &self,
        format: &StreamFormat,
        mut on_data: OutputCallback,
        mut on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>>
    where
        T: cpal::SizedSample + cpal::Sample + cpal::FromSample<f32> + Send + 'static,
    {
        let config = self.config_of(format);
        let mut scratch: Vec<f32> = Vec::new();

        let stream = self
            .device
            .build_output_stream::<T, _, _>(
                &config,
                move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                    if scratch.len() < data.len() {
                        scratch.resize(data.len(), 0.0);
                    }
                    // 交给引擎的长度必须与设备要的**完全一致**:它按长度反推
                    // 帧数,然后照着这个数从 ring 里取数据。多一个样本,它就会
                    // 多取走一份音频 —— 那些数据直接丢失,水位被抽干。
                    let len = data.len();
                    on_data(&mut scratch[..len]);
                    for (dst, src) in data.iter_mut().zip(scratch[..len].iter()) {
                        *dst = T::from_sample(*src);
                    }
                },
                move |e| on_error(e.to_string()),
                None,
            )
            .map_err(|e| Error::DeviceOpen {
                name: self.name.clone(),
                reason: e.to_string(),
            })?;

        Ok(Box::new(CpalStream { stream }))
    }
}

/// 用 cpal 打开的流。
struct CpalStream {
    stream: cpal::Stream,
}

impl StreamHandle for CpalStream {
    fn play(&self) -> Result<()> {
        self.stream
            .play()
            .map_err(|e| Error::Backend(format!("启动音频流失败:{e}")))
    }

    fn pause(&self) -> Result<()> {
        self.stream
            .pause()
            .map_err(|e| Error::Backend(format!("暂停音频流失败:{e}")))
    }
}

fn clamp_rate(
    target: cpal::SampleRate,
    min: cpal::SampleRate,
    max: cpal::SampleRate,
) -> cpal::SampleRate {
    if target < min {
        min
    } else if target > max {
        max
    } else {
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 采样率会被夹到设备支持范围内() {
        assert_eq!(
            clamp_rate(
                cpal::SampleRate(96_000),
                cpal::SampleRate(44_100),
                cpal::SampleRate(48_000)
            ),
            cpal::SampleRate(48_000)
        );
        assert_eq!(
            clamp_rate(
                cpal::SampleRate(48_000),
                cpal::SampleRate(44_100),
                cpal::SampleRate(192_000)
            ),
            cpal::SampleRate(48_000)
        );
    }
}
