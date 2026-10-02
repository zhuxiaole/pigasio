//! ASIO 采样类型转换。
//!
//! ASIO 宿主与驱动之间支持多种采样格式（小端序 LSB）：
//! - `Float32` (ASIOSTFloat32LSB, 4 字节 IEEE-754 浮点)
//! - `Int32` (ASIOSTInt32LSB, 4 字节带符号 32 位整型)
//! - `Int24` (ASIOSTInt24LSB, 3 字节密集打包带符号 24 位整型)
//! - `Int16` (ASIOSTInt16LSB, 2 字节带符号 16 位整型)
//!
//! 引擎内部一律以 `f32` 进行混音、环形缓冲与变速重采样。
//! 本模块负责在交给 ASIO 宿主或从宿主读取缓冲区时，完成 `f32` 与整型目标格式之间的转换。

use crate::config::AsioSampleType;

/// 2^15 - 1
const INT16_MAX_F32: f32 = 32767.0;
/// -2^15
const INT16_MIN_F32: f32 = -32768.0;
/// 2^23 - 1
const INT24_MAX_F32: f32 = 8388607.0;
/// -2^23
const INT24_MIN_F32: f32 = -8388608.0;
/// 2^31 - 1
const INT32_MAX_F64: f64 = 2147483647.0;

/// 实时音频安全的轻量级伪随机数生成器（XorShift32）。
///
/// 特性：
/// - 纯位运算，无任何系统调用、无堆内存分配、无互斥锁。
/// - 单周期即可产出一个高质量随机数。
/// - 支持为每个音频通道独立播种，消除通道间噪声相关性。
#[derive(Debug, Clone, Copy)]
pub struct DitherPrng {
    state: u32,
}

impl DitherPrng {
    /// 用非零种子创建一个 PRNG。若种子为 0，会自动替换为默认非零值。
    pub const fn new(seed: u32) -> Self {
        let state = if seed == 0 { 0x6a09e667 } else { seed };
        Self { state }
    }

    /// 为指定通道索引创建独立的 PRNG 实例，利用黄金分割常数散射种子。
    pub const fn for_channel(channel: usize) -> Self {
        // 使用黄金分割常数 0x9e3779b9 构造通道间独立且互不重合的初始状态
        let seed = 0x85ebca6b ^ ((channel as u32).wrapping_mul(0x9e3779b9));
        Self::new(seed)
    }

    /// 生成一个 [0, 2^32 - 1] 的均匀无符号整数。
    #[inline(always)]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// 生成一个幅度在 (-1.0, 1.0) LSB 之间、均值为 0、方差为 1/6 的 TPDF 随机抖动值。
    ///
    /// TPDF（三角形概率密度函数）由两个均匀分布变量相减得到：
    /// R1 - R2，其中 R1, R2 ~ Uniform(-0.5, 0.5)。
    #[inline(always)]
    pub fn next_tpdf(&mut self) -> f32 {
        // 取两个 16 位的伪随机数以快速构造两个 [0, 1) 浮点
        let r1 = (self.next_u32() >> 16) as f32 / 65536.0;
        let r2 = (self.next_u32() >> 16) as f32 / 65536.0;
        r1 - r2
    }
}

impl Default for DitherPrng {
    fn default() -> Self {
        Self::new(0x6a09e667)
    }
}

/// 强制 16 字节对齐的块，用于确保宿主缓冲区基址的自然对齐。
#[repr(align(16))]
#[derive(Clone, Copy, Default)]
struct Block16(#[allow(dead_code)] pub [u8; 16]);

/// 具备 16 字节对齐保证的连续字节缓冲。
#[derive(Clone, Default)]
pub struct HostBuffer {
    raw: Vec<Block16>,
    len: usize,
}

impl HostBuffer {
    pub fn new(bytes: usize) -> Self {
        let blocks = (bytes + 15) / 16;
        Self {
            raw: vec![Block16([0u8; 16]); blocks],
            len: bytes,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        let ptr = self.raw.as_ptr() as *const u8;
        unsafe { std::slice::from_raw_parts(ptr, self.len) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let ptr = self.raw.as_mut_ptr() as *mut u8;
        unsafe { std::slice::from_raw_parts_mut(ptr, self.len) }
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.raw.as_mut_ptr() as *mut u8
    }
}

/// 将内部 `f32` 切片转换为目标 ASIO 采样格式并写入原生字节切片（无抖动版本，兼容旧接口与测试）。
///
/// # Panics
/// 若 `dst` 字节长度小于 `src.len() * sample_type.size_of()` 则 panic。
pub fn convert_f32_to_asio(src: &[f32], dst: &mut [u8], sample_type: AsioSampleType) {
    convert_f32_to_asio_dithered(src, dst, sample_type, None);
}

/// 将内部 `f32` 切片转换为目标 ASIO 采样格式并写入原生字节切片，支持可选的 TPDF 量化抖动。
///
/// # 抖动规则
/// - 仅当 `sample_type` 为 `Int16` 或 `Int24` 且 `prng` 为 `Some(...)` 时应用 TPDF 抖动；
/// - `Float32`（无需量化）和 `Int32`（量化底噪低于 -190 dB）不执行抖动。
///
/// # Panics
/// 若 `dst` 字节长度小于 `src.len() * sample_type.size_of()` 则 panic。
pub fn convert_f32_to_asio_dithered(
    src: &[f32],
    dst: &mut [u8],
    sample_type: AsioSampleType,
    mut prng: Option<&mut DitherPrng>,
) {
    let sample_size = sample_type.size_of();
    assert!(
        dst.len() >= src.len() * sample_size,
        "目标缓冲区空间不足以容纳转换后的采样"
    );

    match sample_type {
        AsioSampleType::Float32 => {
            for (i, &s) in src.iter().enumerate() {
                let val = if s.is_nan() { 0.0f32 } else { s };
                let bytes = val.to_le_bytes();
                let off = i * 4;
                dst[off..off + 4].copy_from_slice(&bytes);
            }
        }
        AsioSampleType::Int16 => {
            match prng {
                Some(ref mut rng) => {
                    for (i, &s) in src.iter().enumerate() {
                        let clamped = if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) };
                        let scaled = if clamped >= 0.0 {
                            clamped * INT16_MAX_F32
                        } else {
                            clamped * 32768.0
                        };
                        let dither = rng.next_tpdf();
                        let dithered = (scaled + dither).clamp(INT16_MIN_F32, INT16_MAX_F32);
                        let val = dithered.round() as i16;
                        let bytes = val.to_le_bytes();
                        let off = i * 2;
                        dst[off..off + 2].copy_from_slice(&bytes);
                    }
                }
                None => {
                    for (i, &s) in src.iter().enumerate() {
                        let clamped = if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) };
                        let val = if clamped >= 0.0 {
                            (clamped * INT16_MAX_F32).round() as i16
                        } else {
                            (clamped * 32768.0).round() as i16
                        };
                        let bytes = val.to_le_bytes();
                        let off = i * 2;
                        dst[off..off + 2].copy_from_slice(&bytes);
                    }
                }
            }
        }
        AsioSampleType::Int24 => {
            match prng {
                Some(ref mut rng) => {
                    for (i, &s) in src.iter().enumerate() {
                        let clamped = if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) };
                        let scaled = if clamped >= 0.0 {
                            clamped * INT24_MAX_F32
                        } else {
                            clamped * 8388608.0
                        };
                        let dither = rng.next_tpdf();
                        let dithered = (scaled + dither).clamp(INT24_MIN_F32, INT24_MAX_F32);
                        let val = dithered.round() as i32;
                        let bytes = val.to_le_bytes(); // 小端序取低 3 字节
                        let off = i * 3;
                        dst[off] = bytes[0];
                        dst[off + 1] = bytes[1];
                        dst[off + 2] = bytes[2];
                    }
                }
                None => {
                    for (i, &s) in src.iter().enumerate() {
                        let clamped = if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) };
                        let val = if clamped >= 0.0 {
                            (clamped * INT24_MAX_F32).round() as i32
                        } else {
                            (clamped * 8388608.0).round() as i32
                        };
                        let bytes = val.to_le_bytes(); // 小端序取低 3 字节
                        let off = i * 3;
                        dst[off] = bytes[0];
                        dst[off + 1] = bytes[1];
                        dst[off + 2] = bytes[2];
                    }
                }
            }
        }
        AsioSampleType::Int32 => {
            for (i, &s) in src.iter().enumerate() {
                let clamped = if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) };
                let val = if clamped >= 0.0 {
                    (clamped as f64 * INT32_MAX_F64).round() as i32
                } else {
                    (clamped as f64 * 2147483648.0).round() as i32
                };
                let bytes = val.to_le_bytes();
                let off = i * 4;
                dst[off..off + 4].copy_from_slice(&bytes);
            }
        }
    }
}

/// 从宿主原生字节切片中读取采样并转换为内部 `f32` 切片。
///
/// # Panics
/// 若 `src` 字节长度小于 `dst.len() * sample_type.size_of()` 则 panic。
pub fn convert_asio_to_f32(src: &[u8], dst: &mut [f32], sample_type: AsioSampleType) {
    let sample_size = sample_type.size_of();
    assert!(
        src.len() >= dst.len() * sample_size,
        "源缓冲区字节不足以解码目标采样的数量"
    );

    match sample_type {
        AsioSampleType::Float32 => {
            for (i, d) in dst.iter_mut().enumerate() {
                let off = i * 4;
                let mut b = [0u8; 4];
                b.copy_from_slice(&src[off..off + 4]);
                let val = f32::from_le_bytes(b);
                *d = if val.is_nan() { 0.0 } else { val };
            }
        }
        AsioSampleType::Int16 => {
            for (i, d) in dst.iter_mut().enumerate() {
                let off = i * 2;
                let mut b = [0u8; 2];
                b.copy_from_slice(&src[off..off + 2]);
                let val = i16::from_le_bytes(b);
                *d = if val >= 0 {
                    val as f32 / INT16_MAX_F32
                } else {
                    val as f32 / 32768.0
                };
            }
        }
        AsioSampleType::Int24 => {
            for (i, d) in dst.iter_mut().enumerate() {
                let off = i * 3;
                let b0 = src[off];
                let b1 = src[off + 1];
                let b2 = src[off + 2];
                // 符号位扩展到 32 位：若 b2 最高位为 1，高 8 位填 0xFF
                let b3 = if (b2 & 0x80) != 0 { 0xFF } else { 0x00 };
                let val = i32::from_le_bytes([b0, b1, b2, b3]);
                *d = if val >= 0 {
                    val as f32 / INT24_MAX_F32
                } else {
                    val as f32 / 8388608.0
                };
            }
        }
        AsioSampleType::Int32 => {
            for (i, d) in dst.iter_mut().enumerate() {
                let off = i * 4;
                let mut b = [0u8; 4];
                b.copy_from_slice(&src[off..off + 4]);
                let val = i32::from_le_bytes(b);
                *d = if val >= 0 {
                    (val as f64 / INT32_MAX_F64) as f32
                } else {
                    (val as f64 / 2147483648.0) as f32
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_float32_roundtrip() {
        let original = vec![0.0f32, 0.5f32, -0.5f32, 1.0f32, -1.0f32];
        let mut bytes = vec![0u8; original.len() * 4];
        convert_f32_to_asio(&original, &mut bytes, AsioSampleType::Float32);

        let mut reconstructed = vec![0.0f32; original.len()];
        convert_asio_to_f32(&bytes, &mut reconstructed, AsioSampleType::Float32);

        assert_eq!(original, reconstructed);
    }

    #[test]
    fn test_int16_roundtrip() {
        let original = vec![0.0f32, 0.5f32, -0.5f32, 1.0f32, -1.0f32];
        let mut bytes = vec![0u8; original.len() * 2];
        convert_f32_to_asio(&original, &mut bytes, AsioSampleType::Int16);

        let mut reconstructed = vec![0.0f32; original.len()];
        convert_asio_to_f32(&bytes, &mut reconstructed, AsioSampleType::Int16);

        for (a, b) in original.iter().zip(reconstructed.iter()) {
            assert!((a - b).abs() < 1e-4, "left: {a}, right: {b}");
        }
    }

    #[test]
    fn test_int24_roundtrip() {
        let original = vec![0.0f32, 0.5f32, -0.5f32, 1.0f32, -1.0f32];
        let mut bytes = vec![0u8; original.len() * 3];
        convert_f32_to_asio(&original, &mut bytes, AsioSampleType::Int24);

        let mut reconstructed = vec![0.0f32; original.len()];
        convert_asio_to_f32(&bytes, &mut reconstructed, AsioSampleType::Int24);

        for (a, b) in original.iter().zip(reconstructed.iter()) {
            assert!((a - b).abs() < 1e-6, "left: {a}, right: {b}");
        }
    }

    #[test]
    fn test_int32_roundtrip() {
        let original = vec![0.0f32, 0.5f32, -0.5f32, 1.0f32, -1.0f32];
        let mut bytes = vec![0u8; original.len() * 4];
        convert_f32_to_asio(&original, &mut bytes, AsioSampleType::Int32);

        let mut reconstructed = vec![0.0f32; original.len()];
        convert_asio_to_f32(&bytes, &mut reconstructed, AsioSampleType::Int32);

        for (a, b) in original.iter().zip(reconstructed.iter()) {
            assert!((a - b).abs() < 1e-6, "left: {a}, right: {b}");
        }
    }

    #[test]
    fn test_dither_prng_distribution() {
        let mut prng = DitherPrng::new(12345);
        let n = 100_000;
        let mut sum = 0.0f64;
        let mut sum_sq = 0.0f64;

        for _ in 0..n {
            let d = prng.next_tpdf();
            assert!(d > -1.0 && d < 1.0, "TPDF 必须落在 (-1.0, 1.0) 范围内: {d}");
            sum += d as f64;
            sum_sq += (d as f64) * (d as f64);
        }

        let mean = sum / n as f64;
        let variance = (sum_sq / n as f64) - (mean * mean);

        // 理论均值: 0.0，理论方差: 1/6 ≈ 0.166667
        assert!(mean.abs() < 0.01, "均值应接近 0: {mean}");
        assert!((variance - (1.0 / 6.0)).abs() < 0.01, "方差应接近 1/6: {variance}");
    }

    #[test]
    fn test_int16_dithered_roundtrip() {
        let original = vec![0.0f32, 0.25f32, -0.25f32, 0.8f32, -0.8f32, 1.0f32, -1.0f32];
        let mut bytes = vec![0u8; original.len() * 2];
        let mut prng = DitherPrng::for_channel(0);

        convert_f32_to_asio_dithered(&original, &mut bytes, AsioSampleType::Int16, Some(&mut prng));

        let mut reconstructed = vec![0.0f32; original.len()];
        convert_asio_to_f32(&bytes, &mut reconstructed, AsioSampleType::Int16);

        // 包含抖动时误差在 1.5 LSB 范围内（1 LSB ≈ 1/32768 ≈ 3.05e-5）
        for (a, b) in original.iter().zip(reconstructed.iter()) {
            assert!((a - b).abs() < 2.0 / 32767.0, "left: {a}, right: {b}");
        }
    }

    #[test]
    fn test_int24_dithered_roundtrip() {
        let original = vec![0.0f32, 0.25f32, -0.25f32, 0.8f32, -0.8f32, 1.0f32, -1.0f32];
        let mut bytes = vec![0u8; original.len() * 3];
        let mut prng = DitherPrng::for_channel(1);

        convert_f32_to_asio_dithered(&original, &mut bytes, AsioSampleType::Int24, Some(&mut prng));

        let mut reconstructed = vec![0.0f32; original.len()];
        convert_asio_to_f32(&bytes, &mut reconstructed, AsioSampleType::Int24);

        for (a, b) in original.iter().zip(reconstructed.iter()) {
            assert!((a - b).abs() < 2.0 / 8388607.0, "left: {a}, right: {b}");
        }
    }
}
