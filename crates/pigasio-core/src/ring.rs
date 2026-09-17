//! 帧环形缓冲区。
//!
//! 每个音频设备都有一个独立的生产者/消费者对:
//!
//! * **输入设备**:设备回调是生产者(把录音数据塞进来),ASIO 回调是消费者。
//! * **输出设备**:ASIO 回调是生产者,设备回调是消费者。
//!
//! 缓冲区里存的是**交错**的 `f32` 样本,因为设备回调拿到的就是交错数据,
//! 可以直接整块搬运,把通道重排的代价推迟到 ASIO 侧(那里本来就要做
//! deinterleave 才能喂给重采样器)。
//!
//! 这层解耦是 PigASIO 能支持多设备的前提:ASIO 的缓冲区和设备的缓冲区
//! 大小、触发时刻都可以不同,中间的环形缓冲区吸收了这种不匹配。

use std::sync::Arc;

/// 一帧占用多少字节 —— 只用于容量换算的可读性。
const BYTES_PER_SAMPLE: usize = std::mem::size_of::<f32>();
const _: () = assert!(BYTES_PER_SAMPLE == 4);

/// 多通道帧环形缓冲的生产者半端。只能有一个线程持有。
pub struct FrameWriter {
    inner: rtrb::Producer<f32>,
    channels: usize,
    /// 记录因为缓冲区满而丢弃的帧数,用于诊断欠载/溢出。
    overflow_frames: u64,
}

/// 消费者半端。同样只允许一个线程持有。
pub struct FrameReader {
    inner: rtrb::Consumer<f32>,
    channels: usize,
    /// 记录因为缓冲区空而不足的帧数。
    underflow_frames: u64,
    /// 记录因为跟不上生产者而被丢弃的帧数(仅在纠正性跳帧时增加)。
    dropped_frames: u64,
}

/// 一次性创建配对的读写端。
///
/// `capacity_frames` 是期望的帧容量。由于底层要求容量是 2 的幂,
/// 实际容量会向上取整,并在返回值里通过 `FrameReader::capacity_frames()` 可见。
pub fn ring_buffer(channels: usize, capacity_frames: usize) -> (FrameWriter, FrameReader) {
    assert!(channels > 0, "环形缓冲至少需要一个通道");
    let samples = channels.saturating_mul(capacity_frames).max(channels);
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(samples);
    (
        FrameWriter {
            inner: producer,
            channels,
            overflow_frames: 0,
        },
        FrameReader {
            inner: consumer,
            channels,
            underflow_frames: 0,
            dropped_frames: 0,
        },
    )
}

impl FrameWriter {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// 缓冲区的总帧容量。
    pub fn capacity_frames(&self) -> usize {
        self.inner.buffer().capacity() / self.channels
    }

    /// 当前还能写入多少帧(不阻塞)。
    ///
    /// 注意这是调用瞬间的快照 —— 只有生产者会改变它,所以对生产者而言
    /// 在写入前查询是准确的。
    pub fn writable_frames(&self) -> usize {
        self.inner.slots() / self.channels
    }

    /// 已经积压了多少帧尚未被消费。
    pub fn queued_frames(&self) -> usize {
        let total = self.inner.buffer().capacity();
        let free = self.inner.slots();
        (total - free) / self.channels
    }

    pub fn overflow_frames(&self) -> u64 {
        self.overflow_frames
    }

    /// 把一段交错数据整块写入。
    ///
    /// 返回实际写入的帧数。空间不足时只写入能容纳的部分,并把丢弃的
    /// 帧数记入 `overflow_frames` —— 调用方据此判断是否需要重置。
    pub fn write_interleaved(&mut self, src: &[f32], frames: usize) -> usize {
        let want = frames.min(src.len() / self.channels);
        if want == 0 {
            return 0;
        }
        let (pushed, _remainder) = self.inner.push_partial_slice(&src[..want * self.channels]);
        let written = pushed.len() / self.channels;
        if written < want {
            self.overflow_frames += (want - written) as u64;
            log::debug!(
                "环形缓冲写满:请求 {want} 帧,实际写入 {written} 帧(累计溢出 {} 帧)",
                self.overflow_frames
            );
        }
        written
    }

    /// 从设备的交错缓冲里挑选指定通道写入。
    ///
    /// `ch_map[i]` 表示“第 i 个输出通道取自源数据的第几个通道”。
    /// 当 `ch_map` 是恒等映射时会走整块拷贝的快路径。
    pub fn write_selected(
        &mut self,
        src: &[f32],
        src_channels: usize,
        ch_map: &[usize],
        frames: usize,
    ) -> usize {
        debug_assert!(src_channels > 0);
        let available_src_frames = src.len() / src_channels;
        let want = frames.min(available_src_frames);
        if want == 0 {
            return 0;
        }

        // 快路径:通道选取恰好是 0..n,整块拷贝即可。
        let identity =
            ch_map.len() == src_channels && ch_map.iter().enumerate().all(|(i, &c)| i == c);
        if identity {
            let end = want * src_channels;
            return self.write_interleaved(&src[..end], want);
        }

        let want_samples = want * self.channels;
        let Ok(mut chunk) = self.inner.write_chunk_uninit(want_samples) else {
            self.overflow_frames += want as u64;
            return 0;
        };

        let mut written = 0usize;
        {
            let (first, second) = chunk.as_mut_slices();
            let first_len = first.len();
            'outer: for f in 0..want {
                let base = f * src_channels;
                for &c in ch_map {
                    let slot = if written < first_len {
                        &mut first[written]
                    } else if written - first_len < second.len() {
                        &mut second[written - first_len]
                    } else {
                        break 'outer;
                    };
                    slot.write(src[base + c]);
                    written += 1;
                }
            }
        }
        // SAFETY: 上面恰好写满了 `written` 个元素,且 f32 是 Copy 类型,
        // 不存在需要析构的残留。
        unsafe { chunk.commit(written) };

        let written_frames = written / self.channels;
        if written_frames < want {
            self.overflow_frames += (want - written_frames) as u64;
        }
        written_frames
    }
}

impl FrameReader {
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity_frames(&self) -> usize {
        self.inner.buffer().capacity() / self.channels
    }

    /// 当前可以读出的完整帧数。
    pub fn available_frames(&self) -> usize {
        self.inner.slots() / self.channels
    }

    pub fn underflow_frames(&self) -> u64 {
        self.underflow_frames
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// 读出 `frames` 帧交错数据写入 `dst`(长度必须 >= frames * channels)。
    ///
    /// 返回实际读出的帧数。数据不足时返回已有的部分,不足的帧数
    /// 记入 `underflow_frames`,由调用方决定是补静音还是等待。
    pub fn read_interleaved(&mut self, dst: &mut [f32], frames: usize) -> usize {
        let want_samples = (frames * self.channels).min(dst.len());
        if want_samples == 0 {
            return 0;
        }
        // pop_partial_slice 会立刻提交已读走的部分,不需要额外 commit。
        let got_frames = {
            let (filled, _remainder) = self.inner.pop_partial_slice(&mut dst[..want_samples]);
            filled.len() / self.channels
        };
        if got_frames < frames {
            self.underflow_frames += (frames - got_frames) as u64;
        }
        got_frames
    }

    /// 读出 `frames` 帧并 deinterleave 到分离的通道平面。
    ///
    /// `planes` 的每个元素对应一个通道,不足的帧数保持原样(调用方负责补静音)。
    /// 返回实际读出的帧数。
    ///
    /// 数据不够时**有多少读多少**,而不是一帧不读:ring 里已经攒下的数据
    /// 不该白白作废 —— 否则设备侧稍微抖一下,输出就整块变成静音,欠载计数
    /// 还会按整块记,比实际缺口大得多。
    pub fn read_into_planar(&mut self, planes: &mut [Vec<f32>], frames: usize) -> usize {
        let ch = self.channels.min(planes.len());
        if ch == 0 || frames == 0 {
            return 0;
        }
        // 先按实际可读量申请 chunk:`read_chunk` 要求整块可用,直接按
        // `frames` 申请的话差一帧就整个失败。
        let available = self.available_frames().min(frames);
        if available == 0 {
            self.underflow_frames += frames as u64;
            return 0;
        }
        let want_samples = available * self.channels;
        let Ok(chunk) = self.inner.read_chunk(want_samples) else {
            // 理论上走不到:available 就是按 slots 算出来的。
            self.underflow_frames += frames as u64;
            return 0;
        };

        let mut produced = 0usize;
        {
            let (first, second) = chunk.as_slices();
            for seg in [first, second] {
                let seg_frames = seg.len() / self.channels;
                for f in 0..seg_frames {
                    let base = f * self.channels;
                    for (c, plane) in planes.iter_mut().enumerate().take(ch) {
                        if produced + f < plane.len() {
                            plane[produced + f] = seg[base + c];
                        }
                    }
                }
                produced += seg_frames;
            }
        }
        // 借用结束,提交读取进度。忘记这一步会让下次读到同样的数据。
        chunk.commit_all();

        let got_frames = produced.min(frames);
        if got_frames < frames {
            self.underflow_frames += (frames - got_frames) as u64;
        }
        got_frames
    }

    /// 丢弃 `frames` 帧。用于漂移修正时把积压的水位拉回目标值。
    pub fn skip_frames(&mut self, frames: usize) {
        let samples = frames * self.channels;
        if let Ok(chunk) = self.inner.read_chunk(samples) {
            let got = chunk.len() / self.channels;
            chunk.commit_all();
            self.dropped_frames += got as u64;
        }
    }

    /// 一次性读出所有可用数据(最多 `max_frames` 帧),并 deinterleave。
    ///
    /// 用于设备回调这类“有多少取多少”的场景。
    pub fn drain_into_planar(&mut self, planes: &mut [Vec<f32>], max_frames: usize) -> usize {
        let available = self.available_frames();
        let frames = available.min(max_frames);
        if frames == 0 {
            return 0;
        }
        self.read_into_planar(planes, frames)
    }
}

/// 通过 `Arc` 共享的统计信息,供控制面板读取。
///
/// 音频回调不能加锁,所以统计量全部走原子操作。
///
/// # 谁负责写哪一项
///
/// 环形缓冲的两个半端分散在不同线程上,而 `underflow_frames` /
/// `overflow_frames` 的计数分别记在**半端自己**的普通字段里(见
/// [`FrameReader`] / [`FrameWriter`])。所以这两个数必须由**持有那一半端的人**
/// 上报 —— 换个说法:欠载只有持有 reader 的一侧看得见,溢出只有持有 writer
/// 的一侧看得见。
///
/// 这里踩过坑:两条流各自都只上报了「恰好落在 ASIO 侧」的那一半。输入流的
/// ASIO 侧是 reader(上报欠载),它的 writer 在设备回调里,于是**输入溢出
/// 永远不显示**;输出流反过来,**输出欠载永远不显示** —— 水位掉到不足一块
/// 设备缓冲、设备每次都读空,界面上仍是「正常」。修法就是在设备回调里补上
/// 对应那一半的上报(见 `build_output_stream` / `build_input_stream`)。
///
/// 往这个结构里加新计数器时,先想清楚它归哪一侧,再由那一侧 `store`。
/// **两个人都写同一个字段**(一个 `store` 一个 `fetch_add`)会互相覆盖,
/// 结果比不写还难解释。
#[derive(Debug, Default)]
pub struct RingStats {
    /// 读走时数据不够的帧数。由持有 [`FrameReader`] 的一侧上报。
    pub underflow_frames: std::sync::atomic::AtomicU64,
    /// 写入时缓冲已满、被丢掉的帧数。由持有 [`FrameWriter`] 的一侧上报。
    pub overflow_frames: std::sync::atomic::AtomicU64,
    pub dropped_frames: std::sync::atomic::AtomicU64,
    /// 当前积压帧数,由 ASIO 回调定期更新。
    pub queued_frames: std::sync::atomic::AtomicU64,
    /// 漂移修正的瞬时比率,单位是 ppm(百万分之一)。
    ///
    /// 专门存成整数,因为原子浮点操作在多数平台上要绕道 CAS 循环,
    /// 而音频回调里不该有那种东西。
    pub drift_ppm: std::sync::atomic::AtomicI64,
    /// 累计写进 ring / 从 ring 读走的帧数。
    ///
    /// `queued_frames` 看的是**存量**(某一刻积压多少),这两个看的是**流量**。
    /// 定位积压问题时存量只能说明"满了",流量才能指出是生产太快还是消费太慢。
    pub written_frames: std::sync::atomic::AtomicU64,
    pub read_frames: std::sync::atomic::AtomicU64,
    /// 重采样失败的次数。
    ///
    /// 同样是"音频线程只记数、不打印" —— 失败发生在 `fill`/`drain` 里,而那
    /// 两条路径跑在实时线程上,`log::warn!` 会阻塞。由 `Engine::status()` 取走。
    pub resample_failures: std::sync::atomic::AtomicU64,
}

impl RingStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> RingStatsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        RingStatsSnapshot {
            underflow_frames: self.underflow_frames.load(Relaxed),
            overflow_frames: self.overflow_frames.load(Relaxed),
            dropped_frames: self.dropped_frames.load(Relaxed),
            queued_frames: self.queued_frames.load(Relaxed),
            // `drift_ppm` 里存的就是 ppm,不需要再乘系数 ——
            // 早先这里错误地又除了一次 1e6,结果所有非零修正都显示成 0.0。
            drift_ppm: self.drift_ppm.load(Relaxed) as f64,
            written_frames: self.written_frames.load(Relaxed),
            read_frames: self.read_frames.load(Relaxed),
        }
    }
}

/// `RingStats` 的一次性快照,便于界面/日志使用。
#[derive(Debug, Clone, Copy, Default)]
pub struct RingStatsSnapshot {
    pub underflow_frames: u64,
    pub overflow_frames: u64,
    pub dropped_frames: u64,
    pub queued_frames: u64,
    /// 漂移比率的偏差量,例如 0.0001 表示正在以 +100 ppm 补偿。
    pub drift_ppm: f64,
    /// 累计写入 / 读出的帧数。见 [`RingStats::written_frames`]。
    pub written_frames: u64,
    pub read_frames: u64,
}

impl RingStatsSnapshot {
    /// 是否一切正常(没有任何欠载/溢出)。
    pub fn is_healthy(&self) -> bool {
        self.underflow_frames == 0 && self.overflow_frames == 0 && self.dropped_frames == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 交错读写往返() {
        let (mut w, mut r) = ring_buffer(2, 8);
        let src: Vec<f32> = (0..8).map(|i| i as f32).collect();
        assert_eq!(w.write_interleaved(&src, 4), 4);
        assert_eq!(r.available_frames(), 4);

        let mut dst = vec![0.0f32; 8];
        assert_eq!(r.read_interleaved(&mut dst, 4), 4);
        assert_eq!(dst, src);
    }

    #[test]
    fn 空间不足时只写一部分() {
        let (mut w, mut r) = ring_buffer(1, 4);
        let src = vec![1.0f32; 10];
        let written = w.write_interleaved(&src, 10);
        assert!(written <= w.capacity_frames());
        assert!(written > 0);

        let mut dst = vec![0.0f32; written];
        assert_eq!(r.read_interleaved(&mut dst, written), written);
    }

    #[test]
    fn 选择通道写入() {
        let (mut w, mut r) = ring_buffer(2, 8);
        // 源数据是 4 通道,取第 1、3 通道。
        let src: Vec<f32> = vec![
            0.0, 1.0, 2.0, 3.0, // 第 0 帧
            4.0, 5.0, 6.0, 7.0, // 第 1 帧
        ];
        assert_eq!(w.write_selected(&src, 4, &[1, 3], 2), 2);

        let mut dst = vec![0.0f32; 4];
        assert_eq!(r.read_interleaved(&mut dst, 2), 2);
        assert_eq!(dst, vec![1.0, 3.0, 5.0, 7.0]);
    }

    #[test]
    fn 读取到分离平面() {
        let (mut w, mut r) = ring_buffer(2, 8);
        let src: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        w.write_interleaved(&src, 2);

        let mut planes = vec![vec![0.0f32; 2], vec![0.0f32; 2]];
        assert_eq!(r.read_into_planar(&mut planes, 2), 2);
        assert_eq!(planes[0], vec![1.0, 3.0]);
        assert_eq!(planes[1], vec![2.0, 4.0]);
    }

    #[test]
    fn 欠载会被记录() {
        let (mut w, mut r) = ring_buffer(1, 8);
        w.write_interleaved(&[1.0, 2.0], 2);
        let mut dst = vec![0.0f32; 4];
        assert_eq!(r.read_interleaved(&mut dst, 4), 2);
        assert_eq!(r.underflow_frames(), 2);
    }

    #[test]
    fn 跳帧会推进读指针() {
        let (mut w, mut r) = ring_buffer(1, 16);
        let src: Vec<f32> = (0..8).map(|i| i as f32).collect();
        w.write_interleaved(&src, 8);
        r.skip_frames(3);
        let mut dst = vec![0.0f32; 2];
        assert_eq!(r.read_interleaved(&mut dst, 2), 2);
        assert_eq!(dst, vec![3.0, 4.0]);
        assert_eq!(r.dropped_frames(), 3);
    }

    #[test]
    fn 数据不足时读到多少算多少() {
        let (mut w, mut r) = ring_buffer(2, 8);
        // 只写 2 帧,却要读 4 帧。
        w.write_interleaved(&[1.0, 2.0, 3.0, 4.0], 2);

        let mut planes = vec![vec![0.0f32; 4], vec![0.0f32; 4]];
        assert_eq!(
            r.read_into_planar(&mut planes, 4),
            2,
            "ring 里已有的 2 帧该被读走,不该整块作废"
        );
        assert_eq!(planes[0][..2], [1.0, 3.0]);
        assert_eq!(planes[1][..2], [2.0, 4.0]);
        assert_eq!(r.underflow_frames(), 2, "只该记缺的那 2 帧");
    }

    #[test]
    fn 完全读空时记满欠载() {
        let (_w, mut r) = ring_buffer(1, 8);
        let mut planes = vec![vec![0.0f32; 4]];
        assert_eq!(r.read_into_planar(&mut planes, 4), 0);
        assert_eq!(r.underflow_frames(), 4);
    }
}
