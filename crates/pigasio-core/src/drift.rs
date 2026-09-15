//! 时钟漂移补偿。
//!
//! # 问题
//!
//! 多设备 ASIO 的本质困难在于「只能有一个采样时钟」。ASIO 宿主按照一个
//! 固定的时间基准搬运数据,但每块声卡的晶振频率都略有不同 —— 标称
//! 48 kHz 的声卡实际可能是 47999.7 Hz 或 48000.4 Hz。两块声卡之间的
//! 相对偏差通常在 ±100 ppm 量级,差一点的能到 ±500 ppm。
//!
//! 如果不做补偿,偏差会在线性累加:100 ppm 意味着每 10 秒差 48 帧,
//! 大约 3.5 分钟就会差出一个 1024 帧的缓冲区。表现出来就是每隔几分钟
//! 一次爆音,或者干脆彻底失步。
//!
//! # 办法
//!
//! 让每个从设备的流经过一个可以微调比率的重采样器,再用环形缓冲区的
//! 水位作为反馈信号,把水位“钉”在目标值上。水位稳定就说明生产速率和
//! 消费速率匹配了,也就是从设备的时钟被软锁到了主设备上。
//!
//! 控制器是标准的 PI(比例 + 积分):
//!
//! * 比例项负责快速响应水位偏差;
//! * 积分项负责消除稳态误差 —— 这是必需的,因为持续存在的时钟偏差
//!   要求持续存在的非零修正量,只有比例项的话水位最终还是会漂走。
//!
//! # 符号约定
//!
//! 两个方向的控制律是同一个公式,这是刻意设计的:
//!
//! ```text
//! 修正量 = -(kp * e + integral)     其中 e = (当前水位 - 目标水位) / 目标水位
//! ```
//!
//! * **输出流**(ASIO 生产 → 设备消费):重采样比率是 `设备率 / ASIO率`。
//!   水位升高说明 ASIO 生产过剩,减小比率能让每块 ASIO 数据产出更少的
//!   设备帧,水位回落。
//! * **输入流**(设备生产 → ASIO 消费):重采样比率是 `ASIO率 / 设备率`。
//!   水位升高说明设备生产过剩,减小比率会让每次 ASIO 处理消耗更多的
//!   设备帧,水位同样回落。
//!
//! 两个方向都是「减小比率 → 水位下降」,所以可以直接共用一套控制器。

/// 比例增益。
///
/// 归一化误差达到 5%(例如目标水位 2048 帧、实际偏离 102 帧)时,
/// 比例项就会给出接近满量程的修正。这样既能在大偏差时快速收敛,
/// 又能在接近目标时平滑下来。
const KP: f64 = 0.01;

/// 积分增益,单位是「每秒钟」。
///
/// 太小则稳态修正建立得很慢(听感上是缓慢的音高偏移),太大则会在
/// 目标水位附近来回振荡。0.01 对应几秒量级的时间常数,对于 ppm 级别的
/// 漂移很合适。
const KI: f64 = 0.01;

/// 单次调整的最大步长(相对于满量程的比例),避免比率突跳产生咔哒声。
const MAX_STEP_FRACTION: f64 = 0.25;

/// 水位纠正允许比稳态漂移补偿激进多少倍。
///
/// 稳态漂移补偿只需要抵消晶振差异(几百 ppm 量级),而启动时水位可能
/// 离目标差着好几个缓冲区 —— 用几百 ppm 去补要几十秒,这段时间里系统
/// 一直贴着欠载的边缘跑。所以水位纠正允许更大的比率偏移:短暂的轻微
/// 音高变化,远好过持续爆音。
const WATERMARK_CORRECTION_GAIN: f64 = 10.0;

/// 无论如何都不超过的比率偏移上限,0.5%(约 8.6 音分)。
/// 这是「听不出来」和「收敛得够快」之间的折中。
const ABSOLUTE_ADJUST_LIMIT: f64 = 0.005;

/// 水位偏差超过这个比例时,认为发生了异常(设备重配置、暂停后恢复等),
/// 直接清零积分项,避免积分饱和把系统拖住。
const RESET_THRESHOLD: f64 = 0.75;

/// 时钟漂移补偿控制器。
#[derive(Debug, Clone)]
pub struct DriftController {
    /// 目标水位,单位帧。
    target_frames: f64,
    /// 积分累加器。
    integral: f64,
    /// 积分项的上限(相对值),也就是稳态漂移补偿能力。
    max_adjust: f64,
    /// 总修正量的上限(相对值),比 `max_adjust` 宽,用于快速拉回水位。
    total_limit: f64,
    /// 上一次输出的修正量,用于限制步长。
    last_adjust: f64,
    /// 是否启用。关闭时始终返回 0 修正。
    enabled: bool,
    /// 累计运行时间,用于诊断输出。
    elapsed_seconds: f64,
    /// 最近一次观察到的水位,用于诊断。
    last_queued: usize,
}

impl DriftController {
    /// 创建控制器。
    ///
    /// * `target_frames` —— 目标水位。通常取「若干个 ASIO 缓冲区」的大小。
    /// * `max_drift_ppm` —— 稳态漂移补偿的最大量,单位 ppm。
    /// * `enabled` —— 是否启用。
    pub fn new(target_frames: usize, max_drift_ppm: f64, enabled: bool) -> Self {
        let max_adjust = (max_drift_ppm / 1_000_000.0).abs().max(1e-6);
        let total_limit = (max_adjust * WATERMARK_CORRECTION_GAIN).min(ABSOLUTE_ADJUST_LIMIT);
        DriftController {
            target_frames: target_frames.max(1) as f64,
            integral: 0.0,
            max_adjust,
            total_limit,
            last_adjust: 0.0,
            enabled,
            elapsed_seconds: 0.0,
            last_queued: target_frames,
        }
    }

    /// 目标水位。
    pub fn target_frames(&self) -> usize {
        self.target_frames as usize
    }

    /// 是否启用漂移补偿。
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 稳态漂移补偿的上限(相对值)。
    pub fn max_adjust(&self) -> f64 {
        self.max_adjust
    }

    /// 总修正量的上限(相对值)。
    pub fn total_limit(&self) -> f64 {
        self.total_limit
    }

    /// 当前修正量。空转时为 0。
    pub fn current_adjust(&self) -> f64 {
        self.last_adjust
    }

    /// 丢弃积分状态。设备重启、流重建、配置变更后应当调用。
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.last_adjust = 0.0;
    }

    /// 根据当前水位推进一次控制,返回应当施加的相对比率修正量。
    ///
    /// * `queued_frames` —— 环形缓冲区当前积压的帧数。
    /// * `dt_seconds` —— 距离上次调用经过的时间。用 ASIO 缓冲区大小除以
    ///   采样率即可得到,不需要真的读时钟,这样即使系统时间被调整也
    ///   不会影响控制律。
    ///
    /// 返回值直接喂给 `set_relative_ratio(1.0 + 返回值)`。
    pub fn update(&mut self, queued_frames: usize, dt_seconds: f64) -> f64 {
        self.last_queued = queued_frames;
        if !self.enabled || dt_seconds <= 0.0 {
            return 0.0;
        }
        self.elapsed_seconds += dt_seconds;

        let error = (queued_frames as f64 - self.target_frames) / self.target_frames;

        // 偏差过大通常意味着发生了不连续事件,而不是真实的时钟漂移。
        // 这时候继续用历史积分值只会帮倒忙。
        if error.abs() > RESET_THRESHOLD {
            log::debug!("水位偏差 {:.1}% 超过阈值,重置漂移积分", error * 100.0);
            self.integral = 0.0;
        }

        // 误差为正(积压)时积分朝「减小比率」的方向累积。
        // 最终修正量要取负号,所以这里累计误差本身的符号。
        self.integral =
            (self.integral + error * dt_seconds * KI).clamp(-self.max_adjust, self.max_adjust);

        // 比例项负责快速拉回水位,积分项负责消除稳态误差。
        let raw = -(KP * error + self.integral);

        // 限制单步变化量,避免比率突跳产生咔哒声。
        let step_limit = self.total_limit * MAX_STEP_FRACTION;
        let delta = (raw - self.last_adjust).clamp(-step_limit, step_limit);
        self.last_adjust = (self.last_adjust + delta).clamp(-self.total_limit, self.total_limit);

        self.last_adjust
    }

    /// 一次诊断快照。
    pub fn snapshot(&self) -> DriftSnapshot {
        DriftSnapshot {
            target_frames: self.target_frames as usize,
            queued_frames: self.last_queued,
            adjust_ppm: self.last_adjust * 1e6,
            integral_ppm: self.integral * 1e6,
            enabled: self.enabled,
            elapsed_seconds: self.elapsed_seconds,
        }
    }
}

/// 控制器状态快照,用于日志和控制面板。
#[derive(Debug, Clone, Copy)]
pub struct DriftSnapshot {
    pub target_frames: usize,
    pub queued_frames: usize,
    /// 当前修正量,单位 ppm。
    pub adjust_ppm: f64,
    /// 积分项贡献,单位 ppm。
    pub integral_ppm: f64,
    pub enabled: bool,
    pub elapsed_seconds: f64,
}

impl DriftSnapshot {
    /// 水位偏离目标的比例。
    pub fn error_ratio(&self) -> f64 {
        if self.target_frames == 0 {
            0.0
        } else {
            (self.queued_frames as f64 - self.target_frames as f64) / self.target_frames as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 水位处于目标时不产生修正() {
        let mut c = DriftController::new(2048, 500.0, true);
        let adjust = c.update(2048, 1024.0 / 48000.0);
        assert_eq!(adjust, 0.0);
    }

    #[test]
    fn 水位偏高时修正为负() {
        let mut c = DriftController::new(2048, 500.0, true);
        // 让积分项有机会建立起来。
        let mut last = 0.0;
        for _ in 0..50 {
            last = c.update(2048 + 512, 1024.0 / 48000.0);
        }
        assert!(last < 0.0, "水位偏高应产生负修正,实际 {last}");
    }

    #[test]
    fn 水位偏低时修正为正() {
        let mut c = DriftController::new(2048, 500.0, true);
        let mut last = 0.0;
        for _ in 0..50 {
            last = c.update(2048 - 512, 1024.0 / 48000.0);
        }
        assert!(last > 0.0, "水位偏低应产生正修正,实际 {last}");
    }

    #[test]
    fn 修正量不会超过上限() {
        let mut c = DriftController::new(2048, 500.0, true);
        let limit = c.total_limit();
        for _ in 0..10_000 {
            let a = c.update(0, 1024.0 / 48000.0);
            assert!(a.abs() <= limit + 1e-12, "修正量 {a} 超出上限 {limit}");
        }
    }

    #[test]
    fn 稳态修正不会动用启动阶段的额外额度() {
        // 水位只偏差一点点时,修正量应当紧贴稳态补偿能力,
        // 不该动用到为启动阶段准备的那份额外余量。
        let mut c = DriftController::new(2048, 500.0, true);
        let mut last = 0.0;
        for _ in 0..2000 {
            last = c.update(2048 + 20, 1024.0 / 48000.0);
        }
        // 积分项最多到 max_adjust,比例项在小偏差下只贡献一点点,
        // 所以总和的合理上界是 max_adjust 的 1.5 倍。
        let bound = c.max_adjust() * 1.5;
        assert!(
            last.abs() <= bound,
            "小偏差下的修正量 {last} 超出预期上界 {bound}"
        );
        assert!(
            last.abs() < c.total_limit(),
            "稳态修正不应该用到启动阶段的额度"
        );
    }

    #[test]
    fn 水位偏低能在十秒内收敛() {
        // 模拟启动场景:水位只有目标的一半,且没有外部漂移。
        // 修正量为正表示比率提高,ring 会以更快的速度被填满。
        let mut c = DriftController::new(2048, 500.0, true);
        let dt = 1024.0 / 48000.0;
        let mut queued = 1024.0f64;
        let mut seconds = 0.0;

        for _ in 0..20_000 {
            let adjust = c.update(queued as usize, dt);
            queued += adjust * 48_000.0 * dt;
            queued = queued.clamp(0.0, 8192.0);
            seconds += dt;
            if (queued - 2048.0).abs() < 100.0 {
                break;
            }
        }

        assert!(
            seconds < 10.0,
            "水位从 1024 收敛到目标 2048 用了 {seconds:.1} 秒,过慢"
        );
    }

    #[test]
    fn 关闭时不产生修正() {
        let mut c = DriftController::new(2048, 500.0, false);
        for _ in 0..100 {
            assert_eq!(c.update(9999, 1.0), 0.0);
        }
    }

    #[test]
    fn 单步变化受限制() {
        let mut c = DriftController::new(2048, 500.0, true);
        c.update(2048, 1024.0 / 48000.0);
        // 突然来一个巨大偏差,积分会被重置,修正量也不该一步跳到满量程。
        let step = c.update(100_000, 1024.0 / 48000.0);
        let limit = c.total_limit() * MAX_STEP_FRACTION;
        assert!(step.abs() <= limit + 1e-12, "单步 {step} 超过限制 {limit}");
    }

    #[test]
    fn 积分在持续偏差下收敛到抵消漂移() {
        // 模拟真实场景:设备比 ASIO 时钟快 200 ppm,于是水位每秒钟
        // 上涨 200ppm * 48000 = 9.6 帧。控制器必须把水位拉回目标附近。
        let mut c = DriftController::new(2048, 500.0, true);
        let dt = 1024.0 / 48000.0;
        let mut queued = 2048.0f64;
        let drift_per_second = 200e-6 * 48000.0;

        for _ in 0..20_000 {
            let adjust = c.update(queued as usize, dt);
            // 修正量直接作用在水位的增长速度上。
            // adjust 为负表示“减小比率”,也就是减少积压。
            let correction_frames_per_second = -(adjust * 48000.0);
            queued += (drift_per_second - correction_frames_per_second) * dt;
            queued = queued.clamp(0.0, 8192.0);
        }

        let error = (queued - 2048.0).abs();
        assert!(
            error < 200.0,
            "控制器未能把水位拉回目标附近:水位 {queued},误差 {error}"
        );
    }
}
