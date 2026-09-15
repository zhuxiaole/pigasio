//! 把图标编进 exe 的 Windows 资源。
//!
//! 这一步管的是**文件在资源管理器里长什么样**。运行时那个窗口图标(任务栏、
//! Alt+Tab 上显示的)是另一回事,由 `ViewportBuilder::with_icon` 现设 ——
//! 两处都得有,否则会出现"文件有图标、任务栏却是空白"的怪状。

fn main() {
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
