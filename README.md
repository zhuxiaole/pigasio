# PigASIO —— 支持多设备的多路 ASIO 驱动

PigASIO 是一个用 Rust 写的通用 [ASIO] 驱动,把 Windows 的通用音频接口
(WASAPI)桥接给 ASIO 宿主软件。

它参考了 [FlexASIO] 的设计,但解除了 FlexASIO 最关键的一个限制:

> **FlexASIO 只能配置一个输入设备和一个输出设备。**
> **PigASIO 可以配置任意多个输入和输出设备。**

这个限制不是 FlexASIO 的作者偷懒,而是它依赖的底层库 PortAudio 的
`Pa_OpenStream()` 只接受「一个输入设备 + 一个输出设备」。PigASIO 放弃了
那层封装,改为**每块设备各开一条独立的流**,再自己解决由此带来的
时钟同步问题。

[ASIO]: https://www.steinberg.net/developers/
[FlexASIO]: https://github.com/dechamps/FlexASIO

---

## 它能做什么

举几个 FlexASIO 做不到而 PigASIO 可以的实际场景:

* **把多个声卡的输出合成一个多路 ASIO 设备。** 比如主板声卡接监听音箱、
  USB 声卡接耳机、HDMI 输出接功放 —— 在 DAW 里它们表现为一个 6 输出的
  设备,可以各自分配不同的混音总线。
* **同时录制多块声卡的输入。** 内置麦克风阵列和 USB 音频接口可以同时
  录成 4 个输入通道。
* **用虚拟声卡做路由。** 配合 VB-Cable / Virtual Audio Cable / Elgato
  Virtual Audio 之类的虚拟设备,可以把 ASIO 的输入输出接到系统里的
  任意位置。

## 它是怎么做到的

多设备 ASIO 的本质困难是:**ASIO 只能有一个采样时钟**。每块声卡都有自己的
晶振,标称 48 kHz 的声卡实际可能是 47999.7 Hz,另一块可能是 48000.4 Hz。
如果只是把数据按帧搬运,这个差值会持续累积 —— 100 ppm 的偏差意味着
每 10 秒差 48 帧,大约 3.5 分钟就攒够一个 1024 帧的缓冲区,然后爆音。

PigASIO 的处理方式:

```
   ASIO 宿主
      │  bufferSwitch(index)
      ▼
┌──────────────────────────────────────────────────────┐
│ AudioCore —— 由「时钟主设备」的回调驱动                │
│                                                      │
│  1. 从各输入流的环形缓冲取数据 → 填 ASIO 输入缓冲      │
│  2. 调用宿主的 bufferSwitch()                        │
│  3. 把 ASIO 输出缓冲分发到各输出流的环形缓冲           │
│  4. 根据各流的水位调整重采样比率                       │
└───┬──────────────┬──────────────┬────────────────────┘
    │ ring         │ ring         │ ring
    ▼              ▼              ▼
┌─────────┐   ┌─────────┐   ┌─────────┐
│ 声卡 A  │   │ 声卡 B  │   │ 声卡 C  │
│ (主时钟)│   │(重采样) │   │(重采样) │
└─────────┘   └─────────┘   └─────────┘
```

1. 从所有设备里选一个作为**时钟主设备**(默认是第一个输出设备),
   它的回调充当整个驱动的时间基准。
2. 其余每个设备的数据都经过一个**变速重采样器**(rubato 的窗化 sinc
   插值),把它们的时钟"软锁"到主设备上。
3. 控制器用环形缓冲区的**水位**作为反馈信号做 PI 调节:水位偏高说明
   这个设备生产过剩,就降低它的重采样比率;水位偏低就反过来。

这样即使两块声卡的晶振差几百 ppm,系统也能长期稳定运行。

## 与 FlexASIO 的对比

| | FlexASIO | PigASIO |
|---|---|---|
| 输入设备数 | 1 | **任意多个** |
| 输出设备数 | 1 | **任意多个** |
| 后端 | PortAudio(DirectSound / MME / WASAPI / WDM-KS) | WASAPI(经 cpal) |
| WASAPI 独占模式 | 支持 | 暂未实现 |
| 多设备时钟同步 | 不适用 | 环形缓冲 + 变速重采样 + PI 控制 |
| 动态重载配置 | 支持(监听配置文件变化) | 需要重启宿主 |
| ASIO 采样类型 | float32 / int32 / int24 / int16 | float32 |
| 平台 | Windows x86 + x64 | Windows **x64**(见下方"已知限制") |
| 许可证 | GPL-3.0 | GPL-3.0 |

## 项目结构

```
crates/
├── pigasio-core/     引擎:设备枚举、环形缓冲、重采样、时钟同步
├── pigasio-asio/     ASIO 驱动 DLL(COM 服务器 + IASIO 实现 + 注册表)
├── pigasio-cli/      命令行工具 pigasio.exe
└── pigasio-gui/      控制面板 pigasio-gui.exe
```

| 模块 | 职责 |
|---|---|
| `config.rs` | TOML 配置解析与校验(多设备的核心表达) |
| `devices.rs` | 设备枚举与"名字片段/正则"匹配 |
| `ring.rs` | 每流一个的无锁 SPSC 环形缓冲 |
| `resample.rs` | rubato 封装:固定输出 / 固定输入两种变速重采样 |
| `drift.rs` | PI 控制器,把水位钉在目标值上 |
| `engine.rs` | 多流生命周期管理 + 时钟主设备驱动 |
| `abi.rs` | ASIO SDK 2.3 的 ABI 类型翻译(含虚表布局断言) |
| `driver.rs` | `IASIO` 接口实现 |
| `factory.rs` | `IClassFactory` 实现 |
| `registry.rs` | COM 与 ASIO 驱动注册表的读写 |

## 构建

需要 **Rust 1.77+** 和 **MSVC 工具链**。装好 Rust 后如果终端里找不到
`cargo`,重新开一个终端让 PATH 生效。

> 从零搭建开发环境的完整步骤(含国内镜像加速和常见坑)见
> [`docs/development.md`](docs/development.md)。架构与设计决策见
> [`docs/architecture.md`](docs/architecture.md)。

### 一键打包

```cmd
build.bat
```

或者在 PowerShell / Git Bash 里:

```bash
powershell -ExecutionPolicy Bypass -File build.ps1
```

脚本会编译 release 并把产物收集到 `dist\`:

```
dist/
├── pigasio_asio.dll    ASIO 驱动
├── pigasio.exe         命令行工具
├── pigasio-gui.exe     控制面板
└── examples/           示例配置
```

`dist\` 整个目录拷到哪里都行,但**三个文件不能拆开** —— 驱动 DLL 要靠
同目录的 `pigasio-gui.exe` 打开控制面板,`pigasio.exe` 也靠同目录的 DLL
来做安装/卸载。

### 手动构建

```bash
cargo build --release
```

产物在 `target/release/` 下,自己把上面那三个文件拷到一起即可。

## 安装

1. 把三个文件(`pigasio_asio.dll`、`pigasio.exe`、`pigasio-gui.exe`)
   放到同一个目录,例如 `C:\Program Files\PigASIO\`。
2. 以**管理员身份**注册驱动,两种方式任选:

   ```cmd
   pigasio install
   ```

   或者用系统自带的 `regsvr32`:

   ```cmd
   regsvr32 "C:\Program Files\PigASIO\pigasio_asio.dll"
   ```

   需要管理员权限是因为要写 `HKLM\SOFTWARE\ASIO` —— 那一项正是 ASIO
   宿主枚举驱动列表的地方。
3. 重启宿主软件,PigASIO 就会出现在它的 ASIO 驱动列表里。

卸载:

```cmd
pigasio uninstall
```

或者 `regsvr32 /u "C:\Program Files\PigASIO\pigasio_asio.dll"`。

## 配置

配置文件 `PigASIO.toml` 按以下顺序查找,用第一个找到的:

1. 环境变量 `PIGASIO_CONFIG` 指向的路径
2. 宿主可执行文件所在目录
3. 用户目录(`%USERPROFILE%`)

都没有就用内置默认值(默认输入/输出设备,各取前 2 个通道)。

生成一份带注释的模板:

```bash
pigasio init
```

### 最小配置

```toml
sample_rate = 48000
buffer_size_samples = 1024

[[output]]
device = "default"

[[input]]
device = "default"
```

### 多设备配置

```toml
sample_rate = 48000
buffer_size_samples = 512

[engine]
resample_quality = "sinc"      # 默认;音质最好
drift_correction = true        # 关掉它,几十秒内必然爆音
max_drift_ppm = 500.0          # 稳态漂移补偿能力
watermark_ms = 30.0            # 每个流的缓冲目标水位,单位毫秒,直接加在延迟上
use_non_ascii_channel_names = true   # 通道名带中文设备名(乱码时设为 false)

# 第一块输出设备:主板声卡,接监听音箱
[[output]]
device = "扬声器 (Realtek"
channel_count = 2

# 第二块输出设备:USB 声卡,接耳机
[[output]]
device = "USB Audio"
channels = [0, 1]              # 也可以只挑特定通道
gain_db = -6.0                 # 这块声卡单独衰减 6 dB

# 第三块输出设备:HDMI,接功放。指定它做时钟主设备
[[output]]
device = "NVIDIA High Definition Audio"
clock_master = true

# 两块输入设备
[[input]]
device = "麦克风 (AB13X"
channel_count = 1              # 单声道麦克风

[[input]]
device = "Line 1"              # 子串匹配,忽略大小写
channels = [0, 1]
```

上面这份配置会让 ASIO 宿主看到一个 **3 路输入 / 6 路输出**的设备,
在宿主的通道列表里显示成:

```
OUT 1 (扬声器)   OUT 2 (扬声器)
OUT 1 (USB Audio) OUT 2 (USB Audio)
OUT 1 (NVIDIA High Definition) OUT 2 (NVIDIA High Definition)
IN 1 (麦克风)
IN 1 (Line 1)    IN 2 (Line 1)
```

### 配置项速查

| 键 | 默认值 | 说明 |
|---|---|---|
| `sample_rate` | `48000` | ASIO 采样率。设备不支持时会被重采样 |
| `buffer_size_samples` | `1024` | ASIO 缓冲区帧数;2 的幂,界面可选 16–2048 |
| `asio_sample_type` | `"float32"` | 暴露给宿主的采样类型,目前只支持 float32 |
| `engine.resample_quality` | `"sinc"` | `sinc` / `fast` / `none` |
| `engine.drift_correction` | `true` | 是否补偿设备间时钟漂移 |
| `engine.max_drift_ppm` | `500.0` | 稳态漂移补偿上限 |
| `engine.watermark_ms` | `30.0` | 缓冲目标水位,**单位毫秒**;直接加在延迟上 |
| `engine.use_non_ascii_channel_names` | `true` | 通道名是否允许中文;显示乱码时设为 `false` |
| `[[input]]` / `[[output]].device` | `"default"` | 设备名片段;`"default"` 用系统默认设备,`"none"` 禁用 |
| `...channels` | — | 指定通道,如 `[0, 3]` |
| `...channel_count` | `2` | 取前 N 个通道(上限 256);与 `channels` 二选一 |
| `...gain_db` | `0.0` | 该设备所有通道的增益,范围 ±120 dB |
| `...latency` | — | 建议延迟(秒) |
| `...clock_master` | `false` | 是否作为时钟主设备,全配置最多一个 |

> **`device_regex` 已移除。** 设备只能用 `device` 做名字子串匹配(忽略
> 大小写)。早先支持正则,但控制面板在配置往返时会把正则渲染成普通字符串
> 再写回文件,匹配语义被悄悄换掉 —— 用户直到设备打不开才发现。与其维护
> 一个两边对不齐的功能,不如只留行为可预期的子串匹配。含 `device_regex`
> 的旧配置现在会因未知字段而报错,删掉该行、改用 `device` 即可。

## 命令行工具

```bash
pigasio devices              # 列出所有可用设备及其通道数
pigasio init                 # 生成配置模板
pigasio check                # 自检:打开设备跑几秒,报告同步状况
pigasio monitor              # 实时显示各流的缓冲水位与漂移
pigasio channels             # 列出 ASIO 会暴露的通道及其显示名
pigasio install              # 注册驱动(需要管理员权限)
pigasio uninstall            # 注销驱动
```

`check` 是最有用的一个。它完整走一遍宿主的生命周期
(`init → createBuffers → start → 运行 → stop → dispose`),
因此能在打开 DAW 之前就确认多设备是否真的同步:

```
$ pigasio check --config multi.toml --seconds 10

采样率 48000 Hz,缓冲区 1024 帧(约 21.3 ms),2 路 ASIO 输入 / 4 路 ASIO 输出
重采样 Sinc,漂移补偿 开启

[1/5] 解析设备并创建引擎…
[2/5] 打开设备流…
[3/5] 启动…
[4/5] 运行 10.0 秒…
[5/5] 停止并收集统计…

================ 自检报告 ================
运行时长      : 10.01 秒
缓冲区交换    : 469 次(期望 469 次,少 0.00%)

各流状态:
  输入 "Line 1 Apex Legends (Virtual Audio Cable)" × 2 通道
      水位 2279 帧 | 漂移补偿 -441.0 ppm | 欠载 0 / 溢出 0 / 丢弃 0 帧
  输出 "扬声器 (Realtek(R) Audio)" × 2 通道
      水位 1827 帧 | 漂移补偿 +436.0 ppm | 欠载 0 / 溢出 0 / 丢弃 0 帧
  输出 "Line 1 Apex Legends (Virtual Audio Cable)" × 2 通道
      水位 2019 帧 | 漂移补偿 -552.0 ppm | 欠载 0 / 溢出 0 / 丢弃 0 帧

结论:通过。驱动可以正常工作。
```

## 控制面板

`pigasio-gui.exe` 提供一个图形界面:增删设备、选择通道、调增益、指定
时钟主设备,还能**在面板里直接试运行**引擎,实时看到各流的缓冲水位和
漂移补偿量 —— 不用打开 DAW 就能确认配置是否正确。试运行的输出是静音的。

ASIO 宿主调 `controlPanel()` 时,驱动会自动拉起它。

### 界面字体

egui 自带的字体不含汉字字形,所以控制面板会自动从系统字体目录加载一个
中文字体(优先微软雅黑),作为 fallback 追加到字体列表末尾 —— 拉丁字母
仍然用 egui 原生的 UI 字体,只有汉字才落到系统字体上。

如果界面上中文显示成方框,说明系统里没找到可用的字体。这时用环境变量
指定一个:

```cmd
set PIGASIO_FONT=C:\Windows\Fonts\msyh.ttc
pigasio-gui.exe
```

路径后面可以跟 `#序号` 指定 TTC 文件里的第几个字体面(例如
`msyh.ttc#1`),不写则用第 0 个。

## 排错

### 开启日志

在用户目录下创建一个**空文件** `PigASIO.log`:

```cmd
type nul > "%USERPROFILE%\PigASIO.log"
```

驱动会检测到它并把所有决策过程写进去。日志写得非常详细 —— 包括每个设备
用什么格式打开、时钟主设备是谁、每个 ASIO 接口被调用的顺序。

排查完记得**删掉它**:写日志会拖慢音频回调,而且文件会一直变大
(超过 1 GB 会自动停止,以免写满磁盘)。

也可以用命令行工具直接看日志(它们输出到 stderr)。

#### 日志级别与过滤

默认只记录**本项目自己的**日志,第三方库(eframe、winit、cpal……)只保留
警告及以上。这是刻意的:那些库在 TRACE 级别会刷出海量内容 —— 实测控制
面板启动 8 秒就能写出 117 KB,足以把真正有用的驱动日志淹掉。

两个环境变量可以调整:

| 变量 | 作用 |
|---|---|
| `PIGASIO_LOG_LEVEL` | 日志级别:`off` / `error` / `warn` / `info` / `debug` / `trace`,默认 `trace` |
| `PIGASIO_LOG_ALL` | 设为 `1` 时关掉来源过滤,把第三方库的日志也全部记下来 |

排查渲染、窗口创建这类问题时才需要 `PIGASIO_LOG_ALL=1`:

```cmd
set PIGASIO_LOG_ALL=1
pigasio-gui.exe
```

### 常见问题

**宿主列表里看不到 PigASIO**
`regsvr32` 必须以管理员身份运行,否则写 `HKLM` 会失败。成功与否会有弹窗提示。

**通道名是乱码**
ASIO 的通道名是 `char[32]`,协议从没规定过编码。PigASIO 默认按**系统
ANSI 代码页**写入(中文 Windows 上是 GBK),这与绝大多数原生宿主的
预期一致。但少数宿主会按 UTF-8 或别的编码解释这些字节。遇到这种情况,
在配置里关掉中文通道名:

```toml
[engine]
use_non_ascii_channel_names = false
```

通道名会退化成 `OUT 1 (dev2)` 这样的纯 ASCII 形式,用设备序号代替
设备名 —— 信息少一点,但任何编码下都不会出错。控制面板里也有对应的
复选框(「通道名带设备名 / 允许中文」)。

**通道名分不清是哪一路**
先跑 `pigasio channels` 看驱动实际给出的名字:

```cmd
pigasio channels
```

它会列出每个通道在宿主里应当显示的名字,并主动检查有没有重名。
如果这里显示的名字是唯一的、而机架里看着一样,那问题在宿主的显示
(比如列宽不够把尾部截掉了)。

通道名的生成规则是分级降级的,目的是在 32 字节里尽量保住区分度:

| 情况 | 名字长什么样 |
|---|---|
| 设备名本来就不同 | `OUT 1 (Speakers)` —— 最简形式 |
| 多块设备名字前缀相同 | `OUT 1 (扬声器 Realtek)` / `OUT 1 (扬声器 KO)` —— 补上括号里的厂商/型号 |
| 连型号都一样 | `OUT 1 (USB Audio #1)` / `OUT 1 (USB Audio #2)` —— 加序号 |

比如你这台机器上有四块名字都以"扬声器"开头的设备,只靠前缀是分不开的,
现在会自动带上括号里的关键词。整串数字(`OUT 1` / `OUT 2`)始终表示
它在**整块 ASIO 设备**里的通道序号。

**能加载但打开设备失败**
先跑 `pigasio check`。它会明确指出是哪块设备、什么原因。
最常见的是设备被其他程序以独占方式占用,或者通道号超出了设备能力。

**周期性的爆音**
两块声卡的时钟差超过了补偿能力。看日志里的漂移补偿值:如果它一直贴在
`max_drift_ppm` 上,就把 `max_drift_ppm` 调大(比如 2000)。

**一启动就爆一下,之后正常**
启动瞬间的缓冲欠载,属于正常现象。ASIO 规范本身就规定第一个输入缓冲区
是无效的。PigASIO 已经做了输入预热来减轻它。

**延迟太高**
延迟基本就是 `watermark_ms` 加上一个 `buffer_size_samples`。要压延迟,先降
`watermark_ms` —— 它占大头,而且填的是绝对时间,不会随着缓冲区缩水。降到
实时状态里开始出现「欠载」就说明到底了,再往回调一点即可。

`buffer_size_samples` 同样直接加在延迟上,降它是纯收益(不会让水位变薄),
但省下的量很小:16 / 64 / 128 / 256 帧分别是 0.3 / 1.3 / 2.7 / 5.3 ms,而水位
动辄二三十毫秒。所以它该按**宿主的承受能力**来选 —— 宿主里挂的插件重,就往上
调一点,别为了几毫秒让宿主回调超时。

注意这两种毛病的方向是相反的:**调小 `buffer_size_samples` 只降延迟,不会
让水位变薄**;**调小 `watermark_ms` 才是在拿抗抖动能力换延迟。**

**欠载**
`watermark_ms` 不够厚。它至少要盖过一块设备的回调周期 —— WASAPI 共享模式下
设备普遍 10 ms 一块。控制面板会在试运行之后把实测的设备块大小和水位摆在一起
比,不够厚会直接标黄。

实测下来,水位需要大约**两块设备回调期**才稳:水位是按平均值维持的,波谷会
比均值低将近一整块设备回调。所以设备块 10 ms 的机器上,20 ms 会持续欠载、
22 ms 才开始干净。按 `2 × 设备块 × 1.15` 估个保守值,再用试运行验证。

## 已知限制

* **只支持 64 位宿主。** ASIO 在 32 位下用 MSVC 的 `__thiscall` 调用接口
  方法,而 Rust 稳定版无法表达这个调用约定。现代 ASIO 宿主基本都是 64 位。
* **只支持 WASAPI 共享模式。** 独占模式(bit-perfect、更低延迟)尚未实现。
  共享模式的好处是多个程序可以同时使用同一块设备。
  详见下方"关于独占模式"。
* **ASIO 侧只暴露 float32。** 它也是 Windows 音频引擎的内部格式,转换
  代价最低。int32/int24/int16 尚未实现 —— 配置里写这几个值会被直接拒绝
  (而不是报给宿主再静默出错),请保持 `float32`。
* **不支持动态重载配置。** 改完 `PigASIO.toml` 要重启宿主。
  (FlexASIO 支持热重载,这是它的优势。)
* **不支持 ASIO 的时间码/时间信息回调。** PigASIO 只调用 `bufferSwitch`,
  不调用 `bufferSwitchTimeInfo`。原因见 `crates/pigasio-asio/src/abi.rs`
  里的说明 —— 那个结构体在 `#pragma pack(4)` 下的内存布局与 Rust 的
  自然对齐规则不同,很容易写出内存错位。

### 关于独占模式

如果你将来想补上 WASAPI 独占模式,这里有调研过的结论,省得再查一遍。

**cpal 没有留扩展点。** 它的 WASAPI 后端把共享模式写死了:

```rust
// cpal/src/host/wasapi/device.rs 里三处
let share_mode = Audio::AUDCLNT_SHAREMODE_SHARED;
```

`Device` 虽然内部持有 `IMMDevice`,但没有公开的访问器,也没有
`DeviceExt` 之类的扩展 trait。所以**没法在 cpal 之上打补丁**,只能绕过
它,用 `windows` crate 直接操作 WASAPI。

**要写的东西**大致是:

| 部分 | 内容 |
|---|---|
| 后端抽象 | 把 `engine.rs` 对 `cpal::Device`/`cpal::Stream` 的直接依赖抽成 trait |
| WASAPI 后端 | `IAudioClient` 初始化、格式协商、`IAudioRenderClient`/`IAudioCaptureClient`、事件驱动循环 |
| 配置与回退 | `exclusive` 开关、打不开时自动退回共享模式 |

**收益在多设备场景下有限。** 这是最值得先想清楚的一点:为了把多块声卡的
时钟锁在一起,**从设备必须经过重采样**,而重采样意味着数据被改写过,
bit-perfect 从定义上就不成立。所以独占模式最多只能让**时钟主设备**
受益:

```text
时钟主设备        → 独占后可以 bit-perfect ✓
从设备 1、2、...  → 必须重采样跟随主时钟,bit-perfect 无意义 ✗
```

独占模式的另一个收益(更低的延迟)对所有设备都有效,但代价是设备被
独占后其他程序就用不了了 —— 在多程序共用声卡的场景里反而是倒退。

## 测试情况

引擎层已经在真机上验证过(2 块输出设备 + 1 块输入设备,跨 2 块不同的
声卡,25 秒连续运行):

* 缓冲区交换次数与期望值误差 0.05%
* 所有流欠载 0 帧、溢出 0 帧
* 输入缓冲水位收敛到目标值
* 漂移补偿稳定在 ±500 ppm 以内(约 0.9 音分,不可闻)

ASIO 驱动 DLL 的加载路径需要真实的 ASIO 宿主才能验证,本项目未包含这类
测试。如果你要用在关键场合,请先用 `pigasio check` 确认引擎侧没问题,
再在宿主里用简单的工程做一次录音回放验证。

## 许可证

GPL-3.0-or-later,与 FlexASIO 一致。

ASIO 是 Steinberg Media Technologies GmbH 的商标和软件。本项目的 ASIO
ABI 类型定义(`crates/pigasio-asio/src/abi.rs`)是依据公开的 ASIO SDK 2.3
接口规范独立翻译的,项目本身不再分发 SDK 源码;如果你要重新分发驱动,
请自行确认对 ASIO SDK 许可协议的遵守情况。
