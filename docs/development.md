# 开发环境配置

在一台新的 Windows 机器上从零搭建 PigASIO 的开发环境。

整个过程大约 20 分钟，其中大部分时间花在下载上。

## 前置要求

| | 要求 | 说明 |
|---|---|---|
| 系统 | Windows 10 1809+ / 11 | 需要 WASAPI 的现代版本 |
| 架构 | **x64** | 只支持 64 位,原因见 `docs/architecture.md` |
| C++ 工具链 | MSVC + Windows SDK | Rust 在 Windows 上默认用它做链接器 |
| Rust | 1.77+ | 项目用了 `core::mem::offset_of!`(1.77 稳定) |

本文档在以下环境验证过:

```
rustc 1.98.1 (x86_64-pc-windows-msvc)
Visual Studio Build Tools 18,MSVC 14.50.35717
Windows SDK 10.0.26100.0
```

---

## 第一步:MSVC 构建工具

Rust 的 `x86_64-pc-windows-msvc` 目标需要一个 C++ 链接器和 Windows SDK。
最省事的装法是 **Build Tools**(不需要完整的 Visual Studio IDE):

1. 下载 [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/)
   (页面底部 "Tools for Visual Studio" → "Build Tools for Visual Studio")
2. 安装时勾选**「使用 C++ 的桌面开发」**工作负载
3. 确认右侧的安装详细信息里包含:
   - **MSVC v143 生成工具**(或更新版本)
   - **Windows 11 SDK**(或 Windows 10 SDK)

装完之后**不用**配置任何环境变量 —— Rust 会自己去调 `vswhere.exe`
定位工具链。

验证:

```cmd
"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe" ^
  -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 ^
  -property installationPath
```

有输出路径就对了。

## 第二步:Rust

用 [rustup](https://rustup.rs) 安装(不要用独立安装包,后面加组件会麻烦)。

### 国内网络加速

`rustup-init.exe` 默认从 `static.rust-lang.org` 下载,国内可能很慢。
先设两个环境变量指向清华镜像:

```cmd
set RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup
set RUSTUP_UPDATE_ROOT=https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup
```

然后下载并运行安装器:

```cmd
curl -L -o rustup-init.exe https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe
rustup-init.exe -y --profile minimal --default-toolchain stable-x86_64-pc-windows-msvc
```

`--profile minimal` 是刻意的:默认的 `default` profile 会多装文档和
`rust-docs`,对这个项目没用。

### 补上 clippy

`minimal` profile 不含 clippy,而项目的代码质量门禁要用它:

```cmd
rustup component add clippy
```

### ⚠️ 装完必须重开终端

rustup 会把 `%USERPROFILE%\.cargo\bin` 写进用户 PATH,但**已经打开的
终端看不到**。重开一个终端再验证:

```cmd
cargo --version
rustc --version
```

如果重开后仍然找不到,手动检查用户 PATH 里有没有 `%USERPROFILE%\.cargo\bin`。

> 这一步是本项目最容易卡住的地方。如果你在某种受限环境里(比如自动化
> 脚本、非交互式会话)拿不到更新后的 PATH,可以直接用绝对路径:
> `"%USERPROFILE%\.cargo\bin\cargo.exe"`。

## 第三步:验证

```cmd
cd <项目目录>
cargo test --workspace
```

应当看到 4 个测试组全部通过,总计 **72 项**:

```
test result: ok. 19 passed   (pigasio-asio 的 ABI 布局与字符串编码)
test result: ok. 41 passed   (pigasio-core 的引擎、通道名、漂移控制)
test result: ok.  4 passed   (send_probe 的线程安全断言)
test result: ok.  8 passed   (pigasio-gui 的配置往返)
```

（另外还有几个 `0 passed` 的条目,那是空文档测试,正常。）

再跑一次 clippy,确认零警告:

```cmd
cargo clippy --workspace --all-targets
```

## 第四步:构建

```cmd
build.bat
```

产物会收集到 `dist\`:

```
dist/
├── pigasio_asio.dll    ASIO 驱动
├── pigasio.exe         命令行工具
├── pigasio-gui.exe     控制面板
└── examples/           示例配置
```

首次构建要编译 eframe/egui(控制面板的 GUI 框架),大约 2–4 分钟。
之后的增量构建在 20 秒以内。

只想快速验证引擎侧的话,可以跳过 GUI:

```cmd
cargo build --release -p pigasio-cli
```

---

## 项目结构

```
crates/
├── pigasio-core/     引擎:设备枚举、环形缓冲、重采样、时钟同步
├── pigasio-asio/     ASIO 驱动 DLL(COM 服务器 + IASIO 实现)
├── pigasio-cli/      命令行工具 pigasio.exe
└── pigasio-gui/      控制面板 pigasio-gui.exe
```

各模块的职责划分见 [`docs/architecture.md`](architecture.md)。

### 在哪里改什么

| 想改的东西 | 去的文件 |
|---|---|
| 配置文件支持的字段 | `pigasio-core/src/config.rs` |
| 设备匹配逻辑 | `pigasio-core/src/devices.rs` |
| 时钟同步 / 漂移补偿 | `pigasio-core/src/drift.rs` |
| 多流调度、缓冲区管理 | `pigasio-core/src/engine.rs` |
| 重采样封装 | `pigasio-core/src/resample.rs` |
| 通道名生成 | `pigasio-core/src/channel_name.rs` |
| ASIO 接口行为 | `pigasio-asio/src/driver.rs` |
| ASIO 二进制布局 | `pigasio-asio/src/abi.rs` |
| 注册表 | `pigasio-asio/src/registry.rs` |

**改动 ASIO ABI 要格外小心**:`abi.rs` 里的虚表顺序和结构体布局有
编译期断言锁着,改错了测试会失败;但如果是绕过断言改了语义,宿主那边
会直接崩溃而不是报错。

## 开发工作流

改完代码后建议依次跑:

```cmd
cargo test --workspace           # 单元测试
cargo clippy --workspace --all-targets   # 静态检查
pigasio check --seconds 10       # 真机验证引擎(需要音频设备)
```

`pigasio check` 是最有价值的一步 —— 它完整走一遍宿主的生命周期
(`init → createBuffers → start → 运行 → stop → dispose`),
能在不打开 DAW 的情况下验证多设备同步。看两个数字就够:

- **缓冲区交换次数**应当与期望值误差 < 1%
- **欠载 / 溢出**应当都是 0

## 关于依赖下载

本项目直接从 crates.io 拉依赖,不需要额外配置。实测国内直连可用
(`index.crates.io` 正常响应)。

如果所在网络很慢,可以在 `%USERPROFILE%\.cargo\config.toml` 里配镜像:

```toml
[source.crates-io]
replace-with = 'tuna'

[source.tuna]
registry = "sparse+https://mirrors.tuna.tsinghua.edu.cn/crates.io-index/"
```

注意用 `sparse+` 前缀 —— 老的 `git` 协议索引在国内同样很慢。

## 常见问题

**`cargo` 不是内部或外部命令**
rustup 装完要重开终端。见上面"装完必须重开终端"。

**链接失败,提示找不到 `link.exe` 或 `kernel32.lib`**
MSVC 构建工具没装全。回到第一步,确认勾了「使用 C++ 的桌面开发」,
并且安装详细信息里有 MSVC 生成工具和 Windows SDK 两项。

**`build.bat` 报"不是内部或外部命令",命令名是乱码**
`.bat` 文件不能存 UTF-8 中文 —— cmd.exe 按系统 ANSI 代码页(GBK)解析
批处理文件,中文注释会被错误解码,拼出假命令。项目的 `build.bat` 因此
是**纯 ASCII** 的;中文提示都放在 `build.ps1` 里(PowerShell 靠 BOM
正确识别 UTF-8)。往 `build.bat` 里加中文会重新踩这个坑。

**打包时报"文件正由另一进程使用"**
`dist\pigasio_asio.dll` 被正在运行的机架/DAW 加载着,Windows 不允许
替换。脚本会点名是哪个进程占用的:

```
这个文件正被以下程序加载:
    VBAudioMatrixCoconut_x64.exe

请关闭它们...再重新运行本脚本。
```

完全退出那个程序(不是最小化)再跑 `build.bat` 即可。

**`pigasio check` 报设备打开失败**
错误信息里会带 WASAPI 的 HRESULT。几个常见的:

| 错误码 | 含义 |
|---|---|
| `0x8889000A` | 设备被其他程序独占占用 |
| `0x88890008` | 设备不支持请求的音频格式 |
| `0x88890004` | 设备已被移除或禁用 |
| `0x88890010` | Windows 音频服务没有运行 |

注意 `DEVICE_IN_USE` 在多设备场景下是**常见现象**:Elgato、VB-Cable
这类虚拟音频端点通常不支持多客户端,被一个程序打开后另一个就打不开了。
