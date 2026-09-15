//! 把图标编进 exe 的 Windows 资源,并给安装包脚本准备版本号。
//!
//! 图标这一步管的是**文件在资源管理器里长什么样**。运行时那个窗口图标(任务
//! 栏、Alt+Tab 上显示的)是另一回事,由 `ViewportBuilder::with_icon` 现设 ——
//! 两处都得有,否则会出现"文件有图标、任务栏却是空白"的怪状。

/// 把 `CARGO_PKG_VERSION` 写成一段 Inno 预处理指令。
///
/// 版本号本来就只有 `Cargo.toml` 一个出处(Inno 的预处理器读不了 TOML),
/// 与其让人在安装脚本里再抄一遍、抄漏了还没人发现,不如构建时顺手生成一份
/// 给 `installer/PigASIO.iss` 包含。
fn write_version_for_installer() {
    const VERSION: &str = env!("CARGO_PKG_VERSION");

    let path = std::path::Path::new("../../target/version.iss");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let body = format!(
        "// 由 crates/pigasio-gui/build.rs 生成,不要手改。\n\
         // 版本号取自工作区 Cargo.toml 的 version 字段。\n\
         #define AppVer \"{VERSION}\"\n"
    );

    if let Err(e) = std::fs::write(path, body) {
        println!("cargo:warning=写 target/version.iss 失败:{e}");
    }
}

fn main() {
    write_version_for_installer();

    // `Cargo.toml` 里的版本号变了就得重新生成 —— Cargo 默认会因为清单变化
    // 重跑构建脚本,这里再显式声明一次,免得依赖那个隐式规则。
    println!("cargo:rerun-if-changed=../../Cargo.toml");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/pigasio.ico");

        // 图标改了要重新编译资源,不然只有改代码才会触发。
        println!("cargo:rerun-if-changed=../../assets/pigasio.ico");

        if let Err(e) = res.compile() {
            // 编不出图标不该让整个构建失败 —— 没图标程序照样跑。
            println!("cargo:warning=编译 Windows 图标资源失败:{e}");
        }
    }
}
