//! COM 类工厂。
//!
//! ASIO 宿主通过 `CoCreateInstance` 拿到驱动对象,中间必然经过
//! `IClassFactory::CreateInstance`。因为 ASIO 把 CLSID 当 IID 用,
//! `CreateInstance` 的 `riid` 参数收到的其实是我们的 CLSID —— 这里
//! 照单全收,不管请求哪个接口都返回同一个 `IASIO` 指针。

use std::sync::atomic::{AtomicU32, Ordering};

use crate::abi::*;
use crate::driver::DriverObject;

static CLASS_FACTORY_VTBL: IClassFactoryVtbl = IClassFactoryVtbl {
    query_interface: vt_query_interface,
    add_ref: vt_add_ref,
    release: vt_release,
    create_instance: vt_create_instance,
    lock_server: vt_lock_server,
};

/// 类工厂对象。同样要求第一个字段是虚表指针。
#[repr(C)]
pub struct ClassFactory {
    vtbl: *const IClassFactoryVtbl,
    ref_count: AtomicU32,
}

unsafe impl Send for ClassFactory {}
unsafe impl Sync for ClassFactory {}

/// 进程内所有类工厂共享的实例。
///
/// `DllGetClassObject` 每次调用都会返回它,`AddRef`/`Release` 因此是
/// 空操作 —— 这个对象和 DLL 同生命周期。
static CLASS_FACTORY: ClassFactory = ClassFactory {
    vtbl: &CLASS_FACTORY_VTBL,
    ref_count: AtomicU32::new(1),
};

/// 取得类工厂指针。调用方不需要 `Release`。
pub fn class_factory() -> *mut core::ffi::c_void {
    // 刻意泄漏引用计数:这个工厂在 DLL 卸载前一直有效。
    CLASS_FACTORY.ref_count.fetch_add(1, Ordering::Relaxed);
    &CLASS_FACTORY as *const ClassFactory as *mut core::ffi::c_void
}

/// 服务器是否可以被卸载。
///
/// 我们恒返回 `S_FALSE`(表示“不能卸载”)。驱动 DLL 一旦被载入宿主,
/// 让它在中途卸载只会给宿主留下悬空的函数指针,而节约的那点内存
/// 毫无意义 —— FlexASIO 也是同样的选择。
pub fn can_unload_now() -> i32 {
    S_FALSE
}

unsafe fn factory<'a>(this: *mut core::ffi::c_void) -> &'a ClassFactory {
    &*(this as *const ClassFactory)
}

unsafe extern "system" fn vt_query_interface(
    this: *mut core::ffi::c_void,
    riid: *const Guid,
    ppv: *mut *mut core::ffi::c_void,
) -> i32 {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = core::ptr::null_mut();
    if riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = *riid;
    if iid == IID_IUNKNOWN || iid == IID_ICLASS_FACTORY {
        CLASS_FACTORY.ref_count.fetch_add(1, Ordering::Relaxed);
        *ppv = this;
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "system" fn vt_add_ref(this: *mut core::ffi::c_void) -> u32 {
    factory(this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn vt_release(this: *mut core::ffi::c_void) -> u32 {
    // 工厂是静态对象,不真正释放,只递减计数。
    let previous = factory(this).ref_count.fetch_sub(1, Ordering::AcqRel);
    previous.saturating_sub(1)
}

unsafe extern "system" fn vt_create_instance(
    this: *mut core::ffi::c_void,
    outer_unknown: *mut core::ffi::c_void,
    riid: *const Guid,
    ppv: *mut *mut core::ffi::c_void,
) -> i32 {
    let _ = this;
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = core::ptr::null_mut();

    // 聚合不支持。ASIO 宿主也不会用。
    if !outer_unknown.is_null() {
        return CLASS_E_NOAGGREGATION;
    }

    // ASIO 会把 CLSID 当 IID 传进来。它不关心返回值以外的东西,
    // 但为了严谨我们还是校验一下,免得被别的代码误用。
    if !riid.is_null() {
        let iid = *riid;
        if iid != CLSID_PIGASIO && iid != IID_IUNKNOWN {
            log::debug!("CreateInstance 收到未实现的 IID {iid:?}");
            return E_NOINTERFACE;
        }
    }

    let object = DriverObject::create();
    log::info!("已创建 PigASIO 驱动实例 {object:p}");
    *ppv = object as *mut core::ffi::c_void;
    S_OK
}

unsafe extern "system" fn vt_lock_server(_this: *mut core::ffi::c_void, _lock: i32) -> i32 {
    // 我们不做对象计数,因为 `DllCanUnloadNow` 恒返回 S_FALSE。
    S_OK
}
