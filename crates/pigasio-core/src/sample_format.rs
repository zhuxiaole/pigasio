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
/// 2^23 - 1
const INT24_MAX_F32: f32 = 8388607.0;
/// 2^31 - 1
const INT32_MAX_F64: f64 = 2147483647.0;

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

/// 将内部 `f32` 切片转换为目标 ASIO 采样格式并写入原生字节切片。
///
/// # Panics
/// 若 `dst` 字节长度小于 `src.len() * sample_type.size_of()` 则 panic。
pub fn convert_f32_to_asio(src: &[f32], dst: &mut [u8], sample_type: AsioSampleType) {
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
        AsioSampleType::Int24 => {
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
}
