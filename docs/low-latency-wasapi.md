# 低延迟 WASAPI 后端设计方案

> 目标读者:准备实现这件事的人。读之前请先看 [`architecture.md`](architecture.md)
> 了解引擎的整体结构。

## 一、要解决什么

当前延迟的大头有两个,而且互相牵制:

| 环节 | 数值(48 kHz) | 谁决定 |
|---|---|---|
| 环形缓冲水位 | 默认 30 ms | `engine.watermark_ms`(用户可调) |
| ASIO 缓冲区 | 默认 21.3 ms | `buffer_size_samples`(用户可调) |
| **设备回调块** | **约 10 ms** | **WASAPI 共享模式的 period** |

设备块是"根":水位至少要盖过两块设备回调(README 的实测结论),所以
**设备块 10 ms 意味着水位下限约 20 ms**。想真正压低延迟,必须先压设备块。

Windows 10 起,WASAPI 共享模式支持低延迟:通过 `IAudioClient3::InitializeSharedAudioStream`
可以把 period 降到设备允许的最小值(常见 2.67 ms,48 kHz 下 128 帧)。
这样设备块 10 → 3 ms,**水位也能跟着从 20 降到约 6 ms**,合计省 15–20 ms。

**障碍是 cpal。** cpal 0.15 只用旧接口 `IAudioClient::Initialize`:

```rust
// cpal-0.15.3/src/host/wasapi/device.rs:687 (以及 157、578 两处)
let share_mode = Audio::AUDCLNT_SHAREMODE_SHARED;
let hresult = audio_client.Initialize(share_mode, stream_flags, buffer_duration, 0, &format, None);
```

共享模式下 `hnsBufferDuration` 由 audio engine 决定,`Initialize` 拿不到低 period
的能力。而 cpal 的 `Device` 内部虽然持有 `IMMDevice`,却没有公开访问器
(见 README 的"关于独占模式"),所以**没法在 cpal 之上打补丁**,只能绕开它。

## 二、总体设计:分四个阶段

这个顺序不是随意的 —— 阶段 1 是**纯重构、零行为变化**,可以独立验证;
后面三个阶段逐步加新能力,每步都有回退。

| 阶段 | 内容 | 行为变化 | 可独立验证 |
|---|---|---|---|
| 1 | 抽出后端 trait,cpal 作为其中一个实现 | **无** | 全部现有测试 + `pigasio check` 结果不变 |
| 2 | 直写 WASAPI 后端(默认 period) | 换后端,行为对齐 | 与 cpal 后端的 check 报告对比 |
| 3 | 接入 `IAudioClient3` 低延迟 period | 设备块变小 | check 报告里 `device_frames` 从 480 降到 128 |
| 4 | 配置开关、回退策略、水位联动 | 可选特性 | 端到端 |

## 实现进度

**阶段 1、2、3 已完成;阶段 4 的"配置开关"部分也已就绪**(水位联动还没做)。

- `backend/mod.rs`:三个 trait 与公共类型,以及 [`BackendKind`] 的选择逻辑。
- `backend/cpal_backend.rs`:原有 cpal 实现(默认后端)。
- `backend/wasapi/`:`mod.rs` 是设备枚举、混音格式协商与 period 选择,
  `stream.rs` 是事件驱动的流。

### 低延迟 period 是怎么接的

`negotiate` 里先试 `IAudioClient3::GetSharedModeEnginePeriod` 问出设备支持的
范围,选定的值放进 `StreamFormat::period_frames`;`init_client` 再用
`InitializeSharedAudioStream` 按它初始化。任何一步失败(拿不到
`IAudioClient3`、查不到范围、指定周期被拒)都退回普通的 `Initialize`,
行为等同阶段 2。

不写 `period_frames` 就用设备允许的**最小值**;显式指定时会向上对齐到
`fundamental_period` 的整数倍 —— 不对齐的话 `InitializeSharedAudioStream`
直接返回 `E_INVALIDARG`,再夹进设备报的范围。

### 实测:收益完全取决于驱动

在开发机上试过的所有端点(虚拟声卡、Realtek 板载、USB 声卡)**全部报告
`480..480`**,也就是 `min == max == fundamental == 480 帧`:

```
共享模式 period:可选 480..480 帧(默认 480、基本单位 480),选用 480 帧
```

系统是 Windows 11(26200),`IAudioClient3` 正常可用 —— 是**驱动**只支持
这一个 period。所以在这台机器上低延迟共享模式拿不到任何收益,设备块仍是
10 ms。

这不是实现问题,而是这个特性的固有限制:它要求驱动声明支持更小的 period,
而相当多的消费级音频驱动(尤其是带音效处理的板载方案)并不支持。**能不能
受益要看你自己的设备** —— 跑一次 `pigasio check`,看日志里那一行:

- `可选 480..480` → 这台设备没戏,阶段 3 对它没有意义;
- 比如 `可选 64..480` → 有戏,设备块会降到 64 帧(48 kHz 下 1.3 ms)。

顺带排除了一个猜想:试过用 `IAudioClient2::SetClientProperties` 把流声明成
"专业音频"类别来解锁更小的 period,但 `AUDIO_STREAM_CATEGORY` 枚举里根本
没有 ProAudio 这个值(Windows 的 "Pro Audio" 指的是 MMCSS 线程优先级,项目
已经在用了),所以这条路不成立。

### 切换后端

有三个入口,优先级从低到高:

```toml
# 1. 配置文件
[engine]
backend = "wasapi"   # cpal(默认) / wasapi / auto
period_frames = 0    # 0 或省略 = 用设备允许的最小周期
```

```text
2. 控制面板「引擎设置 → 音频后端 / 设备周期」
```

```cmd
:: 3. 环境变量(最高,方便临时覆盖排查)
set PIGASIO_BACKEND=wasapi
```

改动**配置文件或面板之后要重启宿主**才生效 —— 驱动跑在宿主进程里。控制面板
自己的试运行会立刻用上新的选择。

环境变量的值写错时会记一条警告并忽略,不会静默退回默认值 —— 否则用户会以为
自己切换成功了。

### 验收实测

阶段 2 的验收实测(同一份 3 进 3 出的配置,各跑 12 秒):

| | cpal | wasapi |
|---|---|---|
| 设备块 | 480 帧(10 ms) | 480 帧(10 ms) |
| 缓冲区交换偏差 | 0.00% | 0.01% |
| 欠载 / 溢出 | 0 / 0 | 0 / 0 |

顺带修掉一个**与后端无关**的启动期问题:调整了 `Engine::start()` 的顺序,
把"打开闸门"提到"启动输出设备"之前。原来设备 Start 后会立刻从环形缓冲
取数据,而闸门还没开、`drain()` 不执行,缓冲只出不进 —— WASAPI 后端下这会
造成启动期几百帧的欠载,cpal 因为内部时序不同而没有暴露出来。

### 还剩什么

- **水位联动**(阶段 4 的后半):设备块降到 3 ms 之后,`watermark_ms` 的默认
  30 ms 就成了纯粹的多余延迟。现在引擎已经实测出每条流的设备块
  (`StreamStatusSnapshot::device_frames`),可以据此提示或自动下调水位。
  在驱动支持低 period 的机器上,这两件事必须一起做才看得到效果。
- 阶段 3 的代码路径在开发机上只走通了"退回默认"那一支(因为驱动不支持),
  `InitializeSharedAudioStream` 真正成功的那条分支**还没有在真机上验证过**。

**阶段 1 单独就有价值**:它把 `engine.rs` 从 cpal 的具体类型上摘下来,
即使后面几阶段不做了,也让后端可替换。

## 三、阶段 1:后端抽象

### 3.1 为什么要抽象到这个粒度

看当前的依赖面(`grep` 结果):

- `devices.rs`:`cpal::default_host()`、`DeviceTrait::{default_input_config, default_output_config}`、设备枚举
- `engine.rs`:`DeviceTrait::{build_input_stream, build_output_stream, supported_*_configs}`、`StreamTrait::{play, pause}`、`cpal::SampleFormat`、`cpal::FromSample`
- `error.rs`:cpal 各类错误 → `Error::Backend`

所以抽象要覆盖三件事:**枚举**、**协商格式**、**创建/控制流**。

### 3.2 接口

新建 `crates/pigasio-core/src/backend/`:

```rust
// backend/mod.rs

/// 一个音频后端。
pub trait Backend: Send + Sync {
    fn enumerate(&self, kind: StreamKind) -> Result<Vec<DeviceInfo>>;
    fn default_device(&self, kind: StreamKind) -> Result<DeviceInfo>;
}

/// 后端持有的设备句柄。`DeviceInfo` 里放 `Arc<dyn DeviceHandle>`。
pub trait DeviceHandle: Send + Sync {
    /// 协商出一个可以打开的流格式。
    fn negotiate(&self, kind: StreamKind, target_rate: u32) -> Result<StreamFormat>;

    fn open_input(
        &self,
        fmt: &StreamFormat,
        on_data: InputCallback,
        on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>>;

    fn open_output(
        &self,
        fmt: &StreamFormat,
        on_data: OutputCallback,
        on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>>;
}

/// 一个已打开的流。`Send` 是因为它必须在专用线程里创建、启停、销毁
/// (见 `engine.rs` 的 `StreamHost`)。
pub trait StreamHandle: Send {
    fn play(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
}
```

配套类型:

```rust
/// 协商好的流格式。
#[derive(Debug, Clone)]
pub struct StreamFormat {
    pub sample_rate: u32,
    /// 设备原生通道数 —— 回调收到的交错数据的步长。
    pub channels: usize,
    /// 请求的设备周期(帧)。`None` 表示"让后端决定"(当前 cpal 就是这样)。
    pub period_frames: Option<usize>,
}

/// 输入回调:后端把设备数据**转成 f32 交错**后交过来。
pub type InputCallback = Box<dyn FnMut(&[f32]) + Send>;
/// 输出回调:后端从这块缓冲取数据送去设备(同样是交错 f32)。
pub type OutputCallback = Box<dyn FnMut(&mut [f32]) + Send>;
pub type ErrorCallback = Box<dyn FnMut(String) + Send>;
```

**回调统一用 f32 交错**。现在 `engine.rs` 里 `build_input::<T>` 的泛型
(`T: cpal::SizedSample + cpal::FromSample<f32>`)整个消失 —— 格式转换下沉到
后端。`engine.rs` 的闭包本来就要算 `frames = data.len() / device_channels`,
换成 f32 之后这段逻辑不变。

### 3.3 改动清单

| 文件 | 改动 |
|---|---|
| `devices.rs` | `DeviceInfo.device: cpal::Device` → `handle: Arc<dyn DeviceHandle>`;`enumerate`/`resolve` 走 `Backend` |
| `engine.rs` | `StreamSpec` 持 `Arc<dyn DeviceHandle>` + `StreamFormat`;删掉 `build_input::<T>`/`build_output::<T>` 的泛型分派,直接调 `open_input`/`open_output`;`StreamHost` 里存 `Box<dyn StreamHandle>` |
| `error.rs` | cpal 错误到 `Error::Backend` 的 `From` 实现挪到 cpal 后端模块内 |
| `backend/cpal_backend.rs`(新) | 现有 cpal 逻辑整体搬过来,包一层 trait impl |
| `Cargo.toml` | 无新依赖 |

`promote_audio_thread`(实时优先级提升)保持在 `engine.rs` 的回调闭包里 ——
它提的是**当前线程**,而后端正是在回调线程上调用闭包的,所以位置不用变。

### 3.4 验收

阶段 1 的唯一验收标准是**行为完全不变**:

- `cargo test --workspace` 全绿(现有 105 个测试)
- `pigasio check` 的报告与改动前逐字节对比(运行时长、缓冲交换次数、各流水位、漂移、欠载/溢出)
- `pigasio devices` 输出一致

## 四、阶段 2:直写 WASAPI 后端

新建 `backend/wasapi/`。用 `windows` crate(和 cpal 同一个,`IAudioClient3`
等接口定义齐全),不是项目其他地方用的 `windows-sys` —— 后者没有 COM
包装,手写虚表调用不值得。

### 4.1 设备枚举

```
CoInitializeEx(COINIT_MULTITHREADED)
CoCreateInstance(MMDeviceEnumerator)
  → EnumAudioEndpoints(dataflow, DEVICE_STATE_ACTIVE)
  → 每个 IMMDevice:
      OpenPropertyStore → PKEY_Device_FriendlyName   (显示名)
      Activate(IID_IAudioClient) → GetMixFormat      (通道数、采样率)
```

`IMMDevice` 用引用计数持有(和 cpal 一样),作为 `DeviceHandle` 的实现。

### 4.2 流

```
IAudioClient:
  GetMixFormat                     → WAVEFORMATEX(共享模式通常是 f32)
  IsFormatSupported                → 确认目标格式
  SetEventHandle(事件)             → 事件驱动
  GetService(IAudioRenderClient / IAudioCaptureClient)
  GetBufferSize                    → 设备缓冲帧数
  专用线程:
    循环 { WaitForSingleObject(event)
           渲染:GetCurrentPadding → GetBuffer → 回调填充 → ReleaseBuffer
           采集:GetNextPacketSize → GetBuffer → 回调读取 → ReleaseBuffer }
  Start / Stop
```

采集侧的格式转换:共享模式的混音格式绝大多数是 `WAVE_FORMAT_EXTENSIBLE`
配 IEEE float,直接当 f32 用;非 f32 的情况(少见)在这个阶段返回
`Error::DeviceOpen`,不勉强支持 —— 低延迟场景本来也不该用这种设备。

### 4.3 与 cpal 后端的取舍

两个后端在阶段 2 结束后并存,由配置选择(阶段 4 加开关),这样可以
在真机上对比。**如果 WASAPI 后端在 `pigasio check` 下各项指标都不差,
就应该把 cpal 后端删掉** —— 留两套后端意味着双份维护成本,而且 cpal
那份还背着"period 不可控"的原罪。

## 五、阶段 3:低延迟共享模式

这是整个方案的目的所在,实现量却最小。

### 5.1 流程

```
1. 尝试 QueryInterface<IAudioClient3>
   拿不到        → 回退 IAudioClient::Initialize(与阶段 2 相同)
2. GetSharedModeEnginePeriod(format, &default, &fundamental, &min, &max)
3. 选 period:
     用户指定了 period_frames → 向上对齐到 fundamental 的整数倍
     否则                     → min_period(最低延迟)
4. InitializeSharedAudioStream(flags, period_frames, format, session_guid)
   失败          → 回退 Initialize
```

### 5.2 硬要求

- `period_frames` **必须是 `fundamental_period` 的整数倍**,否则
  `InitializeSharedAudioStream` 返回 `E_INVALIDARG`。所以第 3 步的对齐不能省。
- `IAudioClient3` 需要 Windows 10 以上。低版本上 `QueryInterface` 直接失败,
  走回退路径,行为等同阶段 2。
- period 是**每个 client 独立**的,降低它不会影响系统里其他程序 ——
  这也是共享模式相对独占模式的优势。

### 5.3 水位必须跟着降

**这一步单独做没有收益。** 设备块降到 3 ms 之后,`watermark_ms` 的默认值
30 ms 就变成纯粹的多余延迟了 —— 按 README 的"两块设备回调"经验,3 ms 的
设备块只要 6 ms 水位。

所以阶段 3 必须和阶段 4 的水位联动一起做,否则会得到"设备块更小、
总延迟没变、CPU 反而更高"的结果。

## 六、阶段 4:配置与联动

### 6.1 配置项

```toml
[engine]
# 后端:auto(默认,自动挑) / cpal / wasapi
backend = "auto"

# 请求的设备周期(帧)。0 或省略 = 设备允许的最小值。
# 只有 wasapi 后端认这个键。
period_frames = 0
```

### 6.2 水位联动

两种做法,建议先做前者:

**A. 显示 + 建议(低风险)**。引擎已经实测每条流的设备块
(`StreamStatusSnapshot::device_frames`),控制面板试运行面板也已经在把
设备块和水位摆在一起对比。加一条:当水位远大于 `2 × 设备块` 时提示
"当前设备块只有 X 帧,水位可以降到 Y ms"。

**B. 自动水位(中等风险)**。从保守值起步,按实测欠载逐步下调到本机最小值。
收益是用户不用手调,代价是要在实时路径上引入调节逻辑,而且调过头会爆音。
建议等 A 跑一段时间、积累真实数据之后再考虑。

### 6.3 回退

`backend` 的解析优先级:显式配置 > 自动。自动时的顺序是
`wasapi(低延迟)` → `wasapi(默认 period)` → `cpal`。任一步失败都记日志
并降级,不阻止驱动加载。(这正是 `negotiate_config` 现在的做法。)

## 七、风险

| 风险 | 影响 | 应对 |
|---|---|---|
| 低 period 增加 CPU 与爆音风险 | 欠载 | 项目已有 MMCSS 实时优先级;`pigasio check` 能直接看出欠载 |
| 多设备 × 低 period 的 CPU 压力 | 引擎跟不上 | `MAX_BUFFERS_PER_ADVANCE` 已有安全阀并计入 `dropped_frames`;文档说明取舍 |
| 设备/驱动不支持 `IAudioClient3` | 无收益 | `QueryInterface` 失败即回退,降级为阶段 2 行为 |
| 重写后端引入回归 | 音频异常 | 阶段 2 保留 cpal 后端可供对比;阶段 1 的"行为不变"验收是前提 |
| COM 生命周期/线程模型出错 | 崩溃 | 设备句柄用引用计数持有;所有 COM 调用集中在后端模块内,不越过 trait 边界 |

## 八、验证方法

每个阶段都要跑:

```bash
pigasio check --seconds 30        # 关键指标:缓冲交换次数、各流水位、漂移、欠载/溢出
pigasio monitor                   # 实时看水位与设备块
```

阶段 3 的**预期结果**:

- `check` 报告里各流的设备块从约 480 帧(10 ms)降到约 128 帧(2.67 ms)
- 欠载 / 溢出仍为 0
- 把 `watermark_ms` 调到 6–8 ms 之后依然稳定

最后再用真实 ASIO 宿主做一次录音回放验证 —— 引擎侧指标好看不等于宿主
体验没问题。

## 九、工作量估算

| 阶段 | 规模 | 说明 |
|---|---|---|
| 1 | 中 | 重构,约 400–600 行改动,难点在保持行为完全不变 |
| 2 | 大 | 新代码约 800–1000 行,含 COM 样板与事件循环 |
| 3 | 小 | 约 150 行,但依赖阶段 2 |
| 4 | 小 | 配置解析 + 文档,约 200 行 |

阶段 2 是主要成本,而且它是**纯体力活**:WASAPI 共享模式的标准用法,
没有设计上的未知数。真正的风险集中在 COM 生命周期和线程模型上,
这也是建议先做阶段 1(把边界划清楚)的原因。
