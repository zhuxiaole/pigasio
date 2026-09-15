//! `pigasio` —— PigASIO 的命令行工具。
//!
//! 提供四个命令:
//!
//! * `devices` —— 列出系统上所有可用的输入/输出设备;
//! * `init`    —— 生成一份带注释的配置文件模板;
//! * `check`   —— 自检:按宿主的流程走一遍,跑若干秒并报告统计;
//! * `monitor` —— 持续打印各流的缓冲区水位与漂移补偿量。
//!
//! `check` 是最有用的一个。它模拟 ASIO 宿主的完整生命周期
//! (`init → createBuffers → start → 运行 → stop → dispose`),
//! 因此能在真正打开 DAW 之前就确认「多设备到底同不同步」。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pigasio_core::config::{self, Config};
use pigasio_core::{AsioBufferSet, Engine, StreamKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 命令行工具直接把日志送到 stderr,方便用户看到驱动的详细决策过程。
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    pigasio_core::log::init_stderr(if verbose {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });

    let result = match args.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("devices") => cmd_devices(),
        Some("init") => cmd_init(args.get(1).map(PathBuf::from)),
        Some("check") => cmd_check(&args[1..]),
        Some("monitor") => cmd_monitor(&args[1..]),
        Some("channels") => cmd_channels(&args[1..]),
        Some("install") => cmd_install(&args[1..]),
        Some("uninstall") => cmd_uninstall(),
        Some(other) => {
            eprintln!("未知命令:{other}\n");
            print_help();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("\n错误:{e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        r#"pigasio —— PigASIO 多设备 ASIO 驱动的配套工具

用法:
    pigasio devices              列出所有可用的音频设备
    pigasio init [路径]          生成配置文件模板(默认 ./PigASIO.toml)
    pigasio check [选项]         自检:打开设备并运行若干秒,报告同步状况
    pigasio channels [选项]      列出 ASIO 会暴露的通道及其显示名
    pigasio monitor [选项]       实时显示各流的缓冲水位与漂移补偿
    pigasio install [dll路径]    注册 ASIO 驱动(需要管理员权限)
    pigasio uninstall            注销 ASIO 驱动(需要管理员权限)

check / monitor 的选项:
    --config <路径>   使用指定的配置文件(默认按标准顺序查找)
    --seconds <秒数>  运行多久(check 默认 5 秒,monitor 默认 10 秒)
    --passthrough     把输入直接送到输出(默认输出静音,避免吵到人)
    --tone            输出 440 Hz 测试音(配合虚拟声卡回环可验证数据通路)
    -v, --verbose     输出调试日志

配置文件查找顺序:
    1. 环境变量 PIGASIO_CONFIG 指向的路径
    2. 当前目录下的 PigASIO.toml
    3. 用户目录下的 PigASIO.toml
    4. 都没有则使用内置默认值(默认输入/输出设备,各取前 2 个通道)
"#
    );
}

// ---------------------------------------------------------------------------
// devices
// ---------------------------------------------------------------------------

fn cmd_devices() -> Result<(), String> {
    println!("=== 系统音频设备 ===\n");
    let listing = pigasio_core::devices::describe_all().map_err(|e| format!("枚举设备失败:{e}"))?;
    print!("{listing}");

    println!("提示:在 PigASIO.toml 里用 device = \"名字的一部分\" 来指定设备,");
    println!("      也可以用 device_regex 做正则匹配。");
    Ok(())
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

fn cmd_init(path: Option<PathBuf>) -> Result<(), String> {
    let path = path.unwrap_or_else(|| PathBuf::from(config::CONFIG_FILE_NAME));
    if path.exists() {
        return Err(format!(
            "{} 已经存在;请先备份或删除它再运行 init",
            path.display()
        ));
    }

    std::fs::write(&path, CONFIG_TEMPLATE)
        .map_err(|e| format!("写入 {} 失败:{e}", path.display()))?;

    println!("已生成配置文件模板:{}", path.display());
    println!();
    println!("接下来:");
    println!("    pigasio devices              查看可用的设备名");
    println!("    pigasio check                验证配置能否正常打开");
    println!();
    println!("注意:[[input]] 和 [[output]] 都可以写多个 —— 这正是 PigASIO");
    println!("与 FlexASIO 最大的不同。所有设备的通道会按书写顺序拼成 ASIO");
    println!("的通道列表。");
    Ok(())
}

const CONFIG_TEMPLATE: &str = r#"# PigASIO 配置文件
#
# 与 FlexASIO 不同,这里的 [[input]] 和 [[output]] 都是数组,可以写任意多个。
# 每个条目对应一块独立的声卡,它们各自的通道会按书写顺序拼成 ASIO 的通道列表。
#
# 例:下面配置了两块输出设备(共 4 个输出通道)和一块输入设备(2 个输入通道),
# 那么 ASIO 宿主会看到:
#     OUT 1 (Speakers)  OUT 2 (Speakers)  OUT 1 (S/PDIF)  OUT 2 (S/PDIF)
#     IN 1 (Microphone) IN 2 (Microphone)

# ASIO 采样率。所有设备都会尽量以此速率打开;
# 设备不支持时会被重采样到该速率。
sample_rate = 48000

# ASIO 缓冲区大小(采样帧)。必须是 2 的幂,宿主几乎都这么要求。
# 48 kHz 下 512 帧 ≈ 10.7 ms 延迟,1024 帧 ≈ 21.3 ms。
buffer_size_samples = 1024

# 暴露给宿主的采样类型。目前只支持 float32 ——
# 它也是 Windows 音频引擎的内部格式,转换代价最低。
asio_sample_type = "float32"

[engine]
# 重采样质量:sinc(默认,音质最好) / fast(省 CPU) / none(不重采样)
# 只有在所有设备采样率完全一致、且确定不需要漂移补偿时才用 none。
resample_quality = "sinc"

# 是否补偿各设备之间的时钟漂移。
# 关掉它,只要两块声卡的晶振有哪怕 50 ppm 的差异,几十秒内就会爆音。
drift_correction = true

# 漂移补偿的最大修正量(ppm)。500 足以覆盖绝大多数消费级声卡;
# 如果日志里频繁出现“水位异常”,可以适当调大。
max_drift_ppm = 500.0

# 每个流的缓冲区目标水位,单位是「多少个 ASIO 缓冲区」。
# 调大更抗抖动但延迟更高。3.0 是实测出来的下限 —— 低于它开场容易欠载。
buffer_watermark = 3.0

# ---- 输出设备 ----
# 可以写多个 [[output]]。默认播放设备:

[[output]]
device = "default"
channel_count = 2

# 第二块输出设备(把下面几行的注释去掉即可启用)。
# 注意 device 写的是名字的一部分,不区分大小写。
#
# [[output]]
# device = "S/PDIF"
# channels = [0, 1]          # 只取该设备的前两个通道
# latency = 0.02             # 建议延迟(秒),传给 WASAPI
# gain_db = 0.0              # 该设备所有通道的增益

# ---- 输入设备 ----
# 可以写多个 [[input]]。默认录音设备:

[[input]]
device = "default"
channel_count = 2

# 第二块输入设备:
#
# [[input]]
# device = "USB Audio"
# device_regex = "^麦克风"    # 与 device 二选一,正则匹配设备名
# channels = [2, 3]          # 挑该声卡上特定的两个物理输入
# gain_db = 6.0              # 提升 6 dB

# ---- 时钟主设备 ----
# 多设备之间必须有一个时间基准。默认取第一个输出设备;
# 想指定别的设备,在对应的条目里加一行 clock_master = true 即可
# (整个配置里最多只能有一个)。
"#;

// ---------------------------------------------------------------------------
// check / monitor 共用的参数
// ---------------------------------------------------------------------------

struct RunOptions {
    config: Option<PathBuf>,
    seconds: f64,
    passthrough: bool,
    /// 输出 440 Hz 测试音。配合虚拟声卡的回环可以验证整条数据通路。
    tone: bool,
}

fn parse_run_options(args: &[String], default_seconds: f64) -> Result<RunOptions, String> {
    let mut opts = RunOptions {
        config: None,
        seconds: default_seconds,
        passthrough: false,
        tone: false,
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                let value = args.get(i).ok_or("--config 后面需要跟一个路径")?;
                opts.config = Some(PathBuf::from(value));
            }
            "--seconds" => {
                i += 1;
                let value = args.get(i).ok_or("--seconds 后面需要跟一个数字")?;
                opts.seconds = value
                    .parse()
                    .map_err(|_| format!("--seconds 的值 “{value}” 不是数字"))?;
                if opts.seconds <= 0.0 {
                    return Err("--seconds 必须是正数".into());
                }
            }
            "--passthrough" => opts.passthrough = true,
            "--tone" => opts.tone = true,
            "-v" | "--verbose" => {}
            other => return Err(format!("无法识别的选项:{other}")),
        }
        i += 1;
    }

    if opts.passthrough && opts.tone {
        return Err("--passthrough 与 --tone 不能同时使用".into());
    }

    Ok(opts)
}

/// 按配置来源优先级载入配置。
fn load_config(explicit: Option<&PathBuf>) -> Result<Config, String> {
    let path = match explicit {
        Some(p) => {
            if !p.is_file() {
                return Err(format!("配置文件 {} 不存在", p.display()));
            }
            Some(p.clone())
        }
        None => {
            // 命令行场景下「宿主目录」用当前工作目录代替。
            let cwd = std::env::current_dir().ok();
            config::find_config_file(cwd.as_deref())
        }
    };

    match path {
        Some(p) => {
            println!("配置文件:{}", p.display());
            Config::from_file(&p).map_err(|e| e.to_string())
        }
        None => {
            println!("配置文件:未找到,使用内置默认设置");
            let config = Config::default();
            config.validate().map_err(|e| e.to_string())?;
            Ok(config)
        }
    }
}

fn describe_config(config: &Config) {
    let rate = config.sample_rate;
    let chunk = config.buffer_size_samples;
    let latency_ms = chunk as f64 / rate as f64 * 1000.0;
    println!(
        "采样率 {rate} Hz,缓冲区 {chunk} 帧(约 {latency_ms:.1} ms),\
         {} 路 ASIO 输入 / {} 路 ASIO 输出",
        config.total_input_channels(),
        config.total_output_channels()
    );
    println!(
        "重采样 {:?},漂移补偿 {}",
        config.engine.resample_quality,
        if config.engine.drift_correction {
            "开启"
        } else {
            "关闭"
        }
    );
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// 一次自检运行期间收集到的统计。
#[derive(Default)]
struct RunStats {
    /// 宿主回调被调用的次数。
    switches: AtomicU64,
    /// 输入缓冲里出现的非有限样本个数(NaN / Inf)。
    non_finite: AtomicU64,
    /// 输入信号的峰值(以 f32 位模式存放,避免浮点原子操作)。
    peak_bits: AtomicU32,
    /// 输入缓冲里非零样本的比例,用来判断设备是不是真的在送数据。
    non_zero: AtomicU64,
    total_samples: AtomicU64,
}

impl RunStats {
    fn observe_input(&self, buffers: &AsioBufferSet, index: usize, channels: usize) {
        let mut peak = f32::from_bits(self.peak_bits.load(Ordering::Relaxed));
        let mut non_zero = 0u64;
        let mut total = 0u64;
        let mut non_finite = 0u64;

        for ch in 0..channels {
            for &v in buffers.input_plane(ch, index) {
                total += 1;
                if !v.is_finite() {
                    non_finite += 1;
                    continue;
                }
                if v != 0.0 {
                    non_zero += 1;
                }
                let a = v.abs();
                if a > peak {
                    peak = a;
                }
            }
        }

        self.peak_bits.store(peak.to_bits(), Ordering::Relaxed);
        self.non_finite.fetch_add(non_finite, Ordering::Relaxed);
        self.non_zero.fetch_add(non_zero, Ordering::Relaxed);
        self.total_samples.fetch_add(total, Ordering::Relaxed);
    }
}

fn cmd_check(args: &[String]) -> Result<(), String> {
    let opts = parse_run_options(args, 5.0)?;
    let config = load_config(opts.config.as_ref())?;

    println!();
    describe_config(&config);
    println!();

    println!("[1/5] 解析设备并创建引擎…");
    let mut engine = Engine::new(config).map_err(|e| e.to_string())?;
    let in_ch = engine.input_channel_count();
    let out_ch = engine.output_channel_count();
    let chunk = engine.buffer_size();
    let rate = engine.sample_rate();
    println!("      ASIO 将暴露 {in_ch} 路输入 / {out_ch} 路输出");

    println!("[2/5] 打开设备流…");
    let stats = Arc::new(RunStats::default());
    let observe = Arc::clone(&stats);
    let passthrough = opts.passthrough;
    let tone = opts.tone;
    // 测试音的状态。放在闭包外,这样相位能跨缓冲区连续 ——
    // 每个缓冲区都从 0 开始的话,得到的会是一串咔哒声而不是正弦波。
    let mut phase = 0.0f64;
    let phase_step = 440.0 / rate.max(1) as f64;
    let callback = Box::new(move |buffers: &mut AsioBufferSet, index: usize| {
        observe.switches.fetch_add(1, Ordering::Relaxed);
        if in_ch > 0 {
            observe.observe_input(buffers, index, in_ch);
        }
        for ch in 0..out_ch {
            if tone {
                // 440 Hz 正弦,幅度 -12 dBFS。用虚拟声卡把它绕回来,
                // 就能验证「ASIO 输出 → 引擎 → 设备 → 采集 → ASIO 输入」
                // 这整条链路是通的。
                let plane = buffers.output_plane_mut(ch, index);
                for s in plane.iter_mut() {
                    *s = (phase * std::f64::consts::TAU).sin() as f32 * 0.25;
                    phase += phase_step;
                    if phase >= 1.0 {
                        phase -= 1.0;
                    }
                }
            } else if passthrough && in_ch > 0 {
                let src = ch % in_ch;
                let (input, output) = buffers.input_and_output_mut(src, ch, index);
                output.copy_from_slice(input);
            } else {
                buffers.output_plane_mut(ch, index).fill(0.0);
            }
        }
    });

    engine
        .prepare(chunk, callback)
        .map_err(|e| format!("打开设备失败:{e}"))?;

    println!("[3/5] 启动…");
    engine.start().map_err(|e| format!("启动失败:{e}"))?;

    println!("[4/5] 运行 {:.1} 秒…", opts.seconds);
    let started = Instant::now();
    let mut last_report = Instant::now();
    while started.elapsed() < Duration::from_secs_f64(opts.seconds) {
        std::thread::sleep(Duration::from_millis(500));
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            let elapsed = started.elapsed().as_secs_f64();
            let switches = stats.switches.load(Ordering::Relaxed);
            let expected = elapsed * rate as f64 / chunk as f64;
            print!(
                "\r      已运行 {elapsed:5.1}s,缓冲区交换 {switches} 次\
                 (期望约 {expected:.0} 次)"
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
    println!();

    println!("[5/5] 停止并收集统计…");
    engine.stop().map_err(|e| format!("停止失败:{e}"))?;
    let elapsed = started.elapsed().as_secs_f64();
    let status = engine.status();
    engine.dispose().map_err(|e| format!("释放资源失败:{e}"))?;

    // ---- 报告 ----
    let switches = stats.switches.load(Ordering::Relaxed);
    let total_samples = stats.total_samples.load(Ordering::Relaxed);
    let non_zero = stats.non_zero.load(Ordering::Relaxed);
    let non_finite = stats.non_finite.load(Ordering::Relaxed);
    let peak = f32::from_bits(stats.peak_bits.load(Ordering::Relaxed));
    let expected = elapsed * rate as f64 / chunk as f64;
    let missed = expected - switches as f64;
    let missed_ratio = if expected > 0.0 {
        missed / expected
    } else {
        0.0
    };

    println!();
    println!("================ 自检报告 ================");
    println!("运行时长      : {elapsed:.2} 秒");
    println!(
        "缓冲区交换    : {switches} 次(期望 {expected:.0} 次,{} {:.2}%)",
        if missed >= 0.0 { "少" } else { "多" },
        (missed_ratio * 100.0).abs()
    );
    if total_samples > 0 {
        println!(
            "输入信号      : 峰值 {peak:.4},有效样本 {:.1}%",
            non_zero as f64 / total_samples as f64 * 100.0
        );
    }
    if non_finite > 0 {
        println!("⚠ 异常样本    : {non_finite} 个非有限值(NaN/Inf)");
    }

    println!();
    println!("各流状态:");
    for s in &status.stream_stats {
        let d = &s.stats;
        let master = if s.is_clock_master {
            " [时钟主]"
        } else {
            ""
        };
        println!(
            "  {} “{}”{} × {} 通道",
            s.kind.as_str(),
            s.device_name,
            master,
            s.channel_count
        );
        println!(
            "      水位 {} 帧 | 漂移补偿 {:+.1} ppm | 欠载 {} / 溢出 {} / 丢弃 {} 帧",
            d.queued_frames, d.drift_ppm, d.underflow_frames, d.overflow_frames, d.dropped_frames
        );
    }

    // ---- 判定 ----
    let mut problems = Vec::new();
    let mut hints: Vec<String> = Vec::new();

    if missed_ratio > 0.02 {
        problems.push(format!(
            "缓冲区交换次数比期望少 {:.1}%,说明音频回调有中断",
            missed_ratio * 100.0
        ));
    }
    if non_finite > 0 {
        problems.push("输入数据里出现了 NaN/Inf,可能是某个设备的驱动有问题".into());
    }
    let glitchy: Vec<_> = status
        .stream_stats
        .iter()
        .filter(|s| s.had_glitch)
        .map(|s| format!("{} “{}”", s.kind.as_str(), s.device_name))
        .collect();
    if !glitchy.is_empty() {
        problems.push(format!(
            "以下流出现过欠载或溢出:{}。可以试着调大 engine.buffer_watermark 或 buffer_size_samples",
            glitchy.join("、")
        ));
    }

    // 输入静音只作为提示。虚拟声卡在没有程序往里送数据时本来就输出静音,
    // 真实的麦克风也可能因为隐私设置或没插好而安静 —— 这两种情况都
    // 不代表驱动有问题。
    if in_ch > 0 && total_samples > 0 && non_zero == 0 {
        hints.push(
            "输入全程静音。如果这个设备本来就该有声音,请检查信号源、\
             Windows 的麦克风隐私设置,或者先用 --passthrough 试着把输入送到输出"
                .into(),
        );
    }

    println!();
    if problems.is_empty() {
        println!("结论:通过。驱动可以正常工作。");
        for h in &hints {
            println!("  提示:{h}");
        }
        Ok(())
    } else {
        println!("结论:有问题需要处理。");
        for p in &problems {
            println!("  · {p}");
        }
        for h in &hints {
            println!("  提示:{h}");
        }
        Err("自检未通过".into())
    }
}

// ---------------------------------------------------------------------------
// monitor
// ---------------------------------------------------------------------------

fn cmd_monitor(args: &[String]) -> Result<(), String> {
    let opts = parse_run_options(args, 10.0)?;
    let config = load_config(opts.config.as_ref())?;

    println!();
    describe_config(&config);
    println!();

    let mut engine = Engine::new(config).map_err(|e| e.to_string())?;
    let in_ch = engine.input_channel_count();
    let out_ch = engine.output_channel_count();
    let chunk = engine.buffer_size();

    let callback = Box::new(move |buffers: &mut AsioBufferSet, index: usize| {
        // 输出静音:监控不该发出声音。
        for ch in 0..out_ch {
            buffers.output_plane_mut(ch, index).fill(0.0);
        }
        let _ = in_ch;
    });

    engine
        .prepare(chunk, callback)
        .map_err(|e| format!("打开设备失败:{e}"))?;
    engine.start().map_err(|e| format!("启动失败:{e}"))?;

    println!("实时监控中(按 Ctrl+C 结束)…\n");
    println!(
        "{:>8}  {:<28} {:>10} {:>12}",
        "方向", "设备", "水位(帧)", "漂移(ppm)"
    );
    println!("{}", "-".repeat(62));

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs_f64(opts.seconds) {
        std::thread::sleep(Duration::from_secs(1));
        let rows = engine.ring_stats();
        // 用 ANSI 光标上移来原地刷新,而不是刷屏。
        print!("\x1b[{}A", rows.len());
        for (kind, name, snap) in &rows {
            let short: String = name.chars().take(28).collect();
            let flag = if snap.is_healthy() { " " } else { "!" };
            println!(
                "{:>8}  {:<28} {:>10} {:>11.1} {flag}",
                match kind {
                    StreamKind::Input => "输入",
                    StreamKind::Output => "输出",
                },
                short,
                snap.queued_frames,
                snap.drift_ppm
            );
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    engine.stop().map_err(|e| format!("停止失败:{e}"))?;
    engine.dispose().ok();
    println!("\n监控结束。");
    Ok(())
}

// ---------------------------------------------------------------------------
// install / uninstall
// ---------------------------------------------------------------------------

/// 找到与当前可执行文件同目录的驱动 DLL。
fn default_driver_dll() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法确定自身路径:{e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "无法确定自身所在目录".to_string())?;
    let candidate = dir.join("pigasio_asio.dll");
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "在 {} 里找不到 pigasio_asio.dll。\n\
             请先构建,再把 pigasio.exe、pigasio-gui.exe 和 pigasio_asio.dll \
             放到同一个目录。",
            dir.display()
        ))
    }
}

fn cmd_install(args: &[String]) -> Result<(), String> {
    let dll = match args.first() {
        Some(p) if !p.starts_with('-') => PathBuf::from(p),
        _ => default_driver_dll()?,
    };
    if !dll.is_file() {
        return Err(format!("找不到 {}", dll.display()));
    }

    println!("正在注册 ASIO 驱动");
    println!("  DLL:{}", dll.display());
    println!();

    pigasio_asio::registry::register_server(&dll)?;

    println!("注册成功。接下来:");
    println!("  1. 重启宿主软件;");
    println!("  2. 在它的 ASIO 驱动列表里选择 “PigASIO”;");
    println!("  3. 在用户目录或宿主目录放一份 PigASIO.toml 配置多设备");
    println!("     (可以先用 `pigasio init` 生成模板)。");
    println!();
    println!("如果宿主列表里看不到 PigASIO,多半是这次命令没有以管理员身份运行 ——");
    println!(r"写 HKLM\SOFTWARE\ASIO 需要管理员权限。");
    Ok(())
}

fn cmd_uninstall() -> Result<(), String> {
    println!("正在注销 ASIO 驱动…");
    pigasio_asio::registry::unregister_server()?;
    println!("注销完成。重启宿主后它就不再出现在驱动列表里。");
    println!("(DLL 文件本身没有删除,可以手动清理。)");
    Ok(())
}

// ---------------------------------------------------------------------------
// channels
// ---------------------------------------------------------------------------

/// 打印 ASIO 会暴露的通道列表,以及每个通道在宿主里显示的名字。
///
/// 这个命令的价值在于:通道名是**驱动算出来的**,而机架里显示的是**宿主
/// 解释后的结果**。两者不一致时(乱码、重名、被截断),跑一下就能判断
/// 问题出在驱动侧还是宿主侧。
fn cmd_channels(args: &[String]) -> Result<(), String> {
    let opts = parse_run_options(args, 1.0)?;
    let config = load_config(opts.config.as_ref())?;

    println!();
    describe_config(&config);
    println!();

    let engine = Engine::new(config).map_err(|e| e.to_string())?;
    let names = engine.channel_names();

    println!("ASIO 会暴露的通道(方括号里是通道号):");
    println!();

    println!("输入({} 路):", names.inputs().len());
    if names.inputs().is_empty() {
        println!("  (无)");
    }
    for (i, name) in names.inputs().iter().enumerate() {
        println!("  [{i}] {name}");
    }

    println!();
    println!("输出({} 路):", names.outputs().len());
    if names.outputs().is_empty() {
        println!("  (无)");
    }
    for (i, name) in names.outputs().iter().enumerate() {
        println!("  [{i}] {name}");
    }

    // 重名在机架里是致命的 —— 用户没法把两路分开。主动检查并报告。
    let duplicated = [StreamKind::Input, StreamKind::Output]
        .into_iter()
        .filter(|kind| !names.all_unique(*kind))
        .collect::<Vec<_>>();

    if !duplicated.is_empty() {
        for kind in duplicated {
            println!();
            println!("⚠ {}通道名有重复:", kind.as_str());
            let list = match kind {
                StreamKind::Input => names.inputs(),
                StreamKind::Output => names.outputs(),
            };
            let mut seen = std::collections::HashMap::new();
            for name in list {
                *seen.entry(name.as_str()).or_insert(0usize) += 1;
            }
            let mut pairs: Vec<_> = seen.into_iter().filter(|(_, c)| *c > 1).collect();
            pairs.sort();
            for (name, count) in pairs {
                println!("    “{name}” 出现了 {count} 次");
            }
        }
        println!();
        println!("这是 bug,请报告 —— 正常情况下每个通道名都应当是唯一的。");
        return Err("通道名有重复".into());
    }

    println!();
    println!("提示:如果机架里显示的名字和上面不一致(乱码、被截断),");
    println!("      说明问题在宿主的编码处理上。可以试试在配置里设:");
    println!("          [engine]");
    println!("          use_non_ascii_channel_names = false");
    Ok(())
}
