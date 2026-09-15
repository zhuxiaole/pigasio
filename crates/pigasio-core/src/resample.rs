//! 变速重采样。
//!
//! 多设备场景下,每块声卡都有自己的晶振。哪怕标称都是 48 kHz,实际频率
//! 也可能相差几十到几百 ppm。如果只是简单地按帧搬运,这个差值会持续
//! 累积,几秒到几十秒之后缓冲区就会溢满或抽空,听感上就是周期性的爆音。
//!
//! 解决办法是让**除时钟主设备以外**的每个流都经过一个可以微调比率的
//! 重采样器,由 [`crate::drift::DriftController`] 根据缓冲区水位不断修正
//! 比率,把各设备的时钟“软锁”到主设备上。
//!
//! 这里封装了 rubato 的两种异步重采样器:
//!
//! * [`FixedOutResampler`] —— 每次产出固定帧数(ASIO 缓冲区大小),
//!   消费的帧数可变。用在「设备 → ASIO 输入缓冲」方向。
//! * [`FixedInResampler`] —— 每次消费固定帧数,产出的帧数可变。
//!   用在「ASIO 输出缓冲 → 设备」方向。

use rubato::{
    FastFixedIn, FastFixedOut, PolynomialDegree, Resampler, SincFixedIn, SincFixedOut,
    SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::config::ResampleQuality;
use crate::error::{Error, Result};

/// 构造重采样器需要的参数。
#[derive(Debug, Clone)]
pub struct ResamplerSpec {
    /// 质量档位。
    pub quality: ResampleQuality,
    /// 通道数。
    pub channels: usize,
    /// ASIO 侧每次处理的固定帧数,也就是 ASIO 的缓冲区大小。
    pub chunk_size: usize,
    /// 标称比率 = 输出采样率 / 输入采样率。两端采样率相同时为 1.0。
    pub nominal_ratio: f64,
    /// 允许的比率浮动范围(倍数,>= 1.0)。1.01 表示可以在标称值的
    /// 1/1.01 到 1.01 倍之间调整。
    pub max_relative: f64,
}

impl ResamplerSpec {
    /// 根据标称比率和最大漂移量推算需要的浮动范围。
    pub fn max_relative_for(drift_ppm: f64) -> f64 {
        // 留出 1% 的余量吸收启动阶段的粗调,以及量化误差。
        (1.0 + drift_ppm / 1_000_000.0 * 2.0).max(1.01)
    }
}

/// 高质量 sinc 插值的参数。
///
/// 我们的主要场景是补偿极小的时钟漂移(比率偏离 1.0 不到 0.1%),
/// 而不是真正的采样率转换。这种场景下滤波器只需要处理靠近 Nyquist
/// 的一小段过渡带,所以 `sinc_len` 取 128 就能有很好的阻带抑制,
/// 同时保持实时性。
fn sinc_params() -> SincInterpolationParameters {
    SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        oversampling_factor: 128,
        interpolation: SincInterpolationType::Cubic,
        window: WindowFunction::BlackmanHarris2,
    }
}

/// 一次重采样的结果统计。
#[derive(Debug, Clone, Copy, Default)]
pub struct ResampleOutcome {
    /// 实际消耗的输入帧数。
    pub frames_in: usize,
    /// 实际产出的输出帧数。
    pub frames_out: usize,
}

// ---------------------------------------------------------------------------
// 固定输出
// ---------------------------------------------------------------------------

enum FixedOutInner {
    /// 直通:不做重采样,输入输出帧数一致。
    PassThrough,
    Fast(FastFixedOut<f32>),
    Sinc(SincFixedOut<f32>),
}

/// 每次产出恰好 `chunk_size` 帧的重采样器。
///
/// 用在「设备数据 → ASIO 输入缓冲」:设备送来多少帧不确定,但 ASIO
/// 宿主每次都要读取固定大小的输入缓冲。
pub struct FixedOutResampler {
    inner: FixedOutInner,
    channels: usize,
    chunk_size: usize,
    /// 当前相对标称比率的倍数。rubato 没有公开的 getter,所以由我们自己
    /// 跟踪 —— 漂移诊断和控制面板都要用到它。
    current_relative: f64,
    /// 输入帧数的上界,取自 rubato 的缓冲区建议值。
    max_input_frames: usize,
    /// `PassThrough` 模式下的临时缓冲。
    passthrough: Vec<Vec<f32>>,
}

impl FixedOutResampler {
    pub fn new(spec: &ResamplerSpec) -> Result<Self> {
        if spec.channels == 0 || spec.chunk_size == 0 {
            return Err(Error::Internal(
                "重采样器需要至少 1 个通道和 1 帧的块大小".into(),
            ));
        }
        let inner = match spec.quality {
            ResampleQuality::None => FixedOutInner::PassThrough,
            ResampleQuality::Fast => FixedOutInner::Fast(FastFixedOut::<f32>::new(
                spec.nominal_ratio,
                spec.max_relative,
                PolynomialDegree::Septic,
                spec.chunk_size,
                spec.channels,
            )?),
            ResampleQuality::Sinc => FixedOutInner::Sinc(SincFixedOut::<f32>::new(
                spec.nominal_ratio,
                spec.max_relative,
                sinc_params(),
                spec.chunk_size,
                spec.channels,
            )?),
        };

        // 输入帧数的上界直接问 rubato 要 —— 它把 sinc 滤波器的历史长度
        // 和 max_relative 允许的最大比率都算进去了,比我们自己估更准。
        let max_input_frames = match &inner {
            FixedOutInner::PassThrough => spec.chunk_size,
            FixedOutInner::Fast(r) => buffer_frames(&r.input_buffer_allocate(true)),
            FixedOutInner::Sinc(r) => buffer_frames(&r.input_buffer_allocate(true)),
        }
        .max(spec.chunk_size)
        // rubato 的返回值理论上已经是上界,但输入端可能因为时钟漂移
        // 一次性多给一些数据,留一点余量避免越界。
        .saturating_add(64);

        Ok(FixedOutResampler {
            inner,
            channels: spec.channels,
            chunk_size: spec.chunk_size,
            current_relative: 1.0,
            max_input_frames,
            passthrough: vec![vec![0.0; spec.chunk_size]; spec.channels],
        })
    }

    /// 输出端的固定帧数。
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// 下一次调用 `process_into` 需要提供多少输入帧。
    ///
    /// 调用方应当保证输入里至少有这么多帧;不足时由调用方补静音。
    pub fn input_frames_next(&self) -> usize {
        match &self.inner {
            FixedOutInner::PassThrough => self.chunk_size,
            FixedOutInner::Fast(r) => r.input_frames_next(),
            FixedOutInner::Sinc(r) => r.input_frames_next(),
        }
    }

    /// 输入帧数的上界,用于预分配缓冲。
    pub fn max_input_frames(&self) -> usize {
        self.max_input_frames
    }

    /// 处理一个块。
    ///
    /// `input[i]` 是第 i 个通道的输入数据,长度必须 >= `input_frames_next()`。
    /// `output[i]` 是第 i 个通道的输出缓冲,长度必须 >= `chunk_size()`。
    ///
    /// 返回实际消耗/产出的帧数。
    pub fn process_into(
        &mut self,
        input: &[Vec<f32>],
        output: &mut [Vec<f32>],
    ) -> Result<ResampleOutcome> {
        if input.len() < self.channels || output.len() < self.channels {
            return Err(Error::Internal(format!(
                "重采样器通道数不匹配:期望 {},输入 {} 输出 {}",
                self.channels,
                input.len(),
                output.len()
            )));
        }

        match &mut self.inner {
            FixedOutInner::PassThrough => {
                let frames = self.chunk_size;
                for ch in 0..self.channels {
                    let src = &input[ch];
                    let n = frames.min(src.len());
                    output[ch][..n].copy_from_slice(&src[..n]);
                    // 输入不足的部分补静音 —— 这通常意味着设备侧欠载。
                    for s in output[ch][n..frames].iter_mut() {
                        *s = 0.0;
                    }
                }
                Ok(ResampleOutcome {
                    frames_in: frames,
                    frames_out: frames,
                })
            }
            FixedOutInner::Fast(r) => {
                let (used, produced) = r.process_into_buffer(input, output, None)?;
                Ok(ResampleOutcome {
                    frames_in: used,
                    frames_out: produced,
                })
            }
            FixedOutInner::Sinc(r) => {
                let (used, produced) = r.process_into_buffer(input, output, None)?;
                Ok(ResampleOutcome {
                    frames_in: used,
                    frames_out: produced,
                })
            }
        }
    }

    /// 调整比率。`relative` 是相对标称比率的倍数,例如 1.0002 表示
    /// 让输出比标称快 200 ppm。
    ///
    /// `ramp = true` 会让 rubato 平滑过渡到新比率,避免比率突变引入咔哒声。
    pub fn set_relative_ratio(&mut self, relative: f64, ramp: bool) {
        let relative = relative.clamp(0.5, 2.0);
        self.current_relative = relative;
        match &mut self.inner {
            FixedOutInner::PassThrough => {}
            FixedOutInner::Fast(r) => {
                if let Err(e) = r.set_resample_ratio_relative(relative, ramp) {
                    log::warn!("调整重采样比率失败:{e}");
                }
            }
            FixedOutInner::Sinc(r) => {
                if let Err(e) = r.set_resample_ratio_relative(relative, ramp) {
                    log::warn!("调整重采样比率失败:{e}");
                }
            }
        }
    }

    /// 当前比率相对标称值的倍数,用于诊断。
    pub fn current_relative_ratio(&self) -> f64 {
        self.current_relative
    }

    /// 直通模式下借用内部缓冲,便于调用方直接读设备数据。
    pub fn passthrough_buffers(&mut self) -> Option<&mut Vec<Vec<f32>>> {
        match self.inner {
            FixedOutInner::PassThrough => Some(&mut self.passthrough),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 固定输入
// ---------------------------------------------------------------------------

enum FixedInInner {
    PassThrough,
    Fast(FastFixedIn<f32>),
    Sinc(SincFixedIn<f32>),
}

/// 每次消耗恰好 `chunk_size` 帧的重采样器。
///
/// 用在「ASIO 输出缓冲 → 设备」:ASIO 每次给出固定大小的输出缓冲,
/// 而设备侧回调想要多少帧是不确定的。
pub struct FixedInResampler {
    inner: FixedInInner,
    channels: usize,
    chunk_size: usize,
    /// 输出帧数的预分配上界。
    max_output_frames: usize,
    /// 当前相对标称比率的倍数,理由同 [`FixedOutResampler`]。
    current_relative: f64,
}

impl FixedInResampler {
    pub fn new(spec: &ResamplerSpec) -> Result<Self> {
        if spec.channels == 0 || spec.chunk_size == 0 {
            return Err(Error::Internal(
                "重采样器需要至少 1 个通道和 1 帧的块大小".into(),
            ));
        }
        let inner = match spec.quality {
            ResampleQuality::None => FixedInInner::PassThrough,
            ResampleQuality::Fast => FixedInInner::Fast(FastFixedIn::<f32>::new(
                spec.nominal_ratio,
                spec.max_relative,
                PolynomialDegree::Septic,
                spec.chunk_size,
                spec.channels,
            )?),
            ResampleQuality::Sinc => FixedInInner::Sinc(SincFixedIn::<f32>::new(
                spec.nominal_ratio,
                spec.max_relative,
                sinc_params(),
                spec.chunk_size,
                spec.channels,
            )?),
        };

        // 输出帧数的上界同样取自 rubato 的建议值。
        let max_output_frames = match &inner {
            FixedInInner::PassThrough => spec.chunk_size,
            FixedInInner::Fast(r) => buffer_frames(&r.output_buffer_allocate(true)),
            FixedInInner::Sinc(r) => buffer_frames(&r.output_buffer_allocate(true)),
        }
        .max(spec.chunk_size)
        .saturating_add(64);

        Ok(FixedInResampler {
            inner,
            channels: spec.channels,
            chunk_size: spec.chunk_size,
            max_output_frames,
            current_relative: 1.0,
        })
    }

    /// 输入端的固定帧数。
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// 输出帧数的预分配上界。
    pub fn max_output_frames(&self) -> usize {
        self.max_output_frames
    }

    /// 下一次 `process_into` 会产出多少帧。用于决定往环形缓冲写多少。
    pub fn output_frames_next(&self) -> usize {
        match &self.inner {
            FixedInInner::PassThrough => self.chunk_size,
            FixedInInner::Fast(r) => r.output_frames_next(),
            FixedInInner::Sinc(r) => r.output_frames_next(),
        }
    }

    /// 处理一个块。
    ///
    /// `input[i]` 长度必须 >= `chunk_size()`。
    /// `output[i]` 长度必须 >= `max_output_frames()`。
    ///
    /// 返回实际消耗/产出的帧数。
    pub fn process_into(
        &mut self,
        input: &[Vec<f32>],
        output: &mut [Vec<f32>],
    ) -> Result<ResampleOutcome> {
        if input.len() < self.channels || output.len() < self.channels {
            return Err(Error::Internal(format!(
                "重采样器通道数不匹配:期望 {},输入 {} 输出 {}",
                self.channels,
                input.len(),
                output.len()
            )));
        }

        match &mut self.inner {
            FixedInInner::PassThrough => {
                let frames = self.chunk_size;
                for ch in 0..self.channels {
                    let src = &input[ch];
                    let n = frames.min(src.len());
                    output[ch][..n].copy_from_slice(&src[..n]);
                    for s in output[ch][n..frames].iter_mut() {
                        *s = 0.0;
                    }
                }
                Ok(ResampleOutcome {
                    frames_in: frames,
                    frames_out: frames,
                })
            }
            FixedInInner::Fast(r) => {
                let (used, produced) = r.process_into_buffer(input, output, None)?;
                Ok(ResampleOutcome {
                    frames_in: used,
                    frames_out: produced,
                })
            }
            FixedInInner::Sinc(r) => {
                let (used, produced) = r.process_into_buffer(input, output, None)?;
                Ok(ResampleOutcome {
                    frames_in: used,
                    frames_out: produced,
                })
            }
        }
    }

    pub fn set_relative_ratio(&mut self, relative: f64, ramp: bool) {
        let relative = relative.clamp(0.5, 2.0);
        self.current_relative = relative;
        match &mut self.inner {
            FixedInInner::PassThrough => {}
            FixedInInner::Fast(r) => {
                if let Err(e) = r.set_resample_ratio_relative(relative, ramp) {
                    log::warn!("调整重采样比率失败:{e}");
                }
            }
            FixedInInner::Sinc(r) => {
                if let Err(e) = r.set_resample_ratio_relative(relative, ramp) {
                    log::warn!("调整重采样比率失败:{e}");
                }
            }
        }
    }

    /// 当前比率相对标称值的倍数,用于诊断。
    pub fn current_relative_ratio(&self) -> f64 {
        self.current_relative
    }
}

/// 预分配一组分离通道的缓冲。
pub fn allocate_planes(channels: usize, frames: usize) -> Vec<Vec<f32>> {
    vec![vec![0.0f32; frames]; channels]
}

/// 从 rubato 建议的缓冲布局里取出「每个通道多少帧」。
///
/// rubato 的 `*_buffer_allocate()` 返回的是「每个通道一个 Vec」的形式,
/// 长度对每个通道都相同;这里只是把那个长度取出来。
fn buffer_frames(buf: &[Vec<f32>]) -> usize {
    buf.first().map(|v| v.len()).unwrap_or(0)
}

/// 把交错数据 deinterleave 到分离平面。
///
/// `planes` 每个通道的长度决定了最多写入多少帧。
pub fn deinterleave(src: &[f32], src_channels: usize, planes: &mut [Vec<f32>], frames: usize) {
    if src_channels == 0 || planes.is_empty() {
        return;
    }
    let avail = src.len() / src_channels;
    let frames = frames.min(avail);
    for f in 0..frames {
        let base = f * src_channels;
        for (c, plane) in planes.iter_mut().enumerate() {
            if c < src_channels && f < plane.len() {
                plane[f] = src[base + c];
            }
        }
    }
}

/// 把分离平面交织成一段连续数据。
pub fn interleave(planes: &[Vec<f32>], frames: usize, dst: &mut [f32]) {
    let ch = planes.len();
    if ch == 0 {
        return;
    }
    let max_frames = planes
        .iter()
        .map(|p| p.len())
        .min()
        .unwrap_or(0)
        .min(frames);
    for f in 0..max_frames {
        let base = f * ch;
        for (c, plane) in planes.iter().enumerate() {
            if base + c < dst.len() {
                dst[base + c] = plane[f];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(quality: ResampleQuality, channels: usize, chunk: usize) -> ResamplerSpec {
        ResamplerSpec {
            quality,
            channels,
            chunk_size: chunk,
            nominal_ratio: 1.0,
            max_relative: 1.01,
        }
    }

    #[test]
    fn 直通模式保持样本不变() {
        let mut r = FixedOutResampler::new(&spec(ResampleQuality::None, 2, 4)).unwrap();
        let input = vec![vec![1.0, 2.0, 3.0, 4.0], vec![5.0, 6.0, 7.0, 8.0]];
        let mut output = allocate_planes(2, 4);
        let out = r.process_into(&input, &mut output).unwrap();
        assert_eq!(out.frames_in, 4);
        assert_eq!(out.frames_out, 4);
        assert_eq!(output[0], vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(output[1], vec![5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn sinc_重采样输出恰好一个块() {
        let mut r = FixedOutResampler::new(&spec(ResampleQuality::Sinc, 2, 64)).unwrap();
        let need = r.input_frames_next();
        assert!(need > 0);
        let input = allocate_planes(2, need);
        let mut output = allocate_planes(2, 64);
        let out = r.process_into(&input, &mut output).unwrap();
        assert_eq!(out.frames_out, 64);
    }

    #[test]
    fn 调整比率会改变输入需求() {
        // rubato 的比率定义是「输出率 / 输入率」。对固定输出的重采样器来说,
        // 比率调高意味着同样多的输出帧只需要更少的输入帧。
        let mut r = FixedOutResampler::new(&spec(ResampleQuality::Sinc, 1, 256)).unwrap();
        let base = r.input_frames_next();
        r.set_relative_ratio(1.005, false);
        let faster = r.input_frames_next();
        assert!(
            faster <= base,
            "比率提高后输入需求不应增加:base={base}, faster={faster}"
        );

        // 反方向:比率调低,需要的输入帧变多。
        r.set_relative_ratio(0.995, false);
        let slower = r.input_frames_next();
        assert!(
            slower >= base,
            "比率降低后输入需求不应减少:base={base}, slower={slower}"
        );
    }

    #[test]
    fn 输入上限足够覆盖实际需求() {
        let mut r = FixedOutResampler::new(&spec(ResampleQuality::Sinc, 2, 512)).unwrap();
        // 在最极端的比率下,实际需求也不能超过我们声明的上界。
        r.set_relative_ratio(1.0 / 1.01, false);
        assert!(
            r.input_frames_next() <= r.max_input_frames(),
            "需求 {} 超过上界 {}",
            r.input_frames_next(),
            r.max_input_frames()
        );
    }

    #[test]
    fn 输出上限足够覆盖实际产出() {
        let mut r = FixedInResampler::new(&spec(ResampleQuality::Sinc, 2, 512)).unwrap();
        r.set_relative_ratio(1.01, false);
        assert!(
            r.output_frames_next() <= r.max_output_frames(),
            "产出 {} 超过上界 {}",
            r.output_frames_next(),
            r.max_output_frames()
        );
    }

    #[test]
    fn 固定输入模式产出可变帧数() {
        let mut r = FixedInResampler::new(&spec(ResampleQuality::Sinc, 2, 128)).unwrap();
        let input = allocate_planes(2, 128);
        let mut output = allocate_planes(2, r.max_output_frames());
        let out = r.process_into(&input, &mut output).unwrap();
        assert_eq!(out.frames_in, 128);
        assert!(out.frames_out > 0);
    }

    #[test]
    fn 交错与分离相互转换() {
        let src = vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut planes = allocate_planes(2, 3);
        deinterleave(&src, 2, &mut planes, 3);
        assert_eq!(planes[0], vec![1.0, 3.0, 5.0]);
        assert_eq!(planes[1], vec![2.0, 4.0, 6.0]);

        let mut dst = vec![0.0f32; 6];
        interleave(&planes, 3, &mut dst);
        assert_eq!(dst, src);
    }

    #[test]
    fn 直通模式输入不足时补静音() {
        let mut r = FixedOutResampler::new(&spec(ResampleQuality::None, 1, 4)).unwrap();
        let input = vec![vec![1.0, 2.0]]; // 只有 2 帧
        let mut output = allocate_planes(1, 4);
        r.process_into(&input, &mut output).unwrap();
        assert_eq!(output[0], vec![1.0, 2.0, 0.0, 0.0]);
    }
}
