//! 编译期契约:引擎必须能跨线程移动。
//!
//! 这些断言不是形式主义 —— `cpal::Stream` 本身**不是** `Send`
//! (它内部按平台持有 `PhantomData<*mut ()>`),所以引擎只好把设备流
//! 收拢到一个专用线程里持有,对外只暴露线程安全的句柄。如果哪天有人
//! 把 `cpal::Stream` 直接塞回 `Engine`,这个文件就会编译失败,
//! 而不是等到驱动在某个宿主里随机崩溃。

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

#[test]
fn 引擎可以跨线程移动() {
    assert_send::<pigasio_core::Engine>();
}

#[test]
fn 配置可以跨线程共享() {
    assert_send::<pigasio_core::Config>();
    assert_sync::<pigasio_core::Config>();
}

#[test]
fn cpal_设备句柄可以跨线程移动() {
    // 设备句柄只是 COM 指针的包装;线程亲和性只在创建流时才要求。
    assert_send::<cpal::Device>();
    assert_send::<cpal::Host>();
}

#[test]
fn 错误类型可以跨线程传递() {
    assert_send::<pigasio_core::Error>();
    assert_sync::<pigasio_core::Error>();
}
