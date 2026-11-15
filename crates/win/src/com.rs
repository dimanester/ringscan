//! Minimal COM plumbing for the one COM API this crate uses:
//! `IFileOperation`, to move items to the Recycle Bin *without ever
//! deleting them permanently*.
//!
//! ## Why IFileOperation and not SHFileOperationW
//!
//! `SHFileOperationW(FO_DELETE, FOF_ALLOWUNDO | FOF_NOCONFIRMATION)` silently
//! destroys an item instead of recycling it when the Recycle Bin cannot take
//! it (item larger than the bin, drive or share without a bin, bin disabled),
//! and gives the caller no way to notice beforehand. `IFileOperation` reports
//! every deletion to an `IFileOperationProgressSink` *before* it happens:
//! `PreDeleteItem` receives `TSF_DELETE_RECYCLE_IF_POSSIBLE` in its flags
//! only when the item is going to the Recycle Bin. Our sink returns
//! `E_ABORT` for any item without that flag, which cancels that deletion and
//! everything after it, and remembers the item so the caller gets a
//! [`NotRecyclable`](crate::shell::NotRecyclable) error and can offer a
//! confirmed permanent delete instead. As a last line of defence the caller
//! also checks afterwards that the items are really gone *and* that no
//! deletion bypassed the sink.
//!
//! ## Why hand-written vtables
//!
//! windows-sys has no COM interface wrappers, and the `windows` crate's Shell
//! module is several MB of generated code that every consumer would have to
//! compile for three method calls. The layouts below follow
//! `shobjidl_core.h` (and windows 0.62's `IFileOperation_Vtbl`,
//! `IFileOperationProgressSink_Vtbl`, `IShellItem_Vtbl`); slot positions are
//! asserted at compile time. Unused slots are `usize` placeholders: we never
//! call them, and in the vtable we *implement* (the sink) every slot is a
//! real function with the exact signature.
//!
//! All of this runs on a dedicated short-lived STA thread (see
//! [`on_sta_thread`]), so the caller's own COM apartment, if any, does not
//! matter and the sink is only ever called from that thread.

use std::ffi::c_void;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{E_ABORT, E_NOINTERFACE, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows_sys::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOFX_EARLYFAILURE,
    FOFX_RECYCLEONDELETE, FileOperation, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows_sys::core::{GUID, HRESULT, IID_IUnknown, IUnknown_Vtbl};

use crate::sys::{check_hr, hresult_error};

const IID_IFILE_OPERATION: GUID = GUID::from_u128(0x947aab5f_0a5c_4c13_b4d6_4bf7836fc9f8);
const IID_IFILE_OPERATION_PROGRESS_SINK: GUID =
    GUID::from_u128(0x04b0f1a7_9490_44bc_96e1_4296a31252e2);
const IID_ISHELL_ITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
}

// --- apartments ----------------------------------------------------------------

/// `CoInitializeEx(STA)` for the current thread, undone on drop when it was
/// this guard that initialized (or re-entered) COM.
pub(crate) struct ComInit {
    uninit: bool,
}

impl ComInit {
    pub(crate) fn sta() -> io::Result<ComInit> {
        // SAFETY: plain FFI call.
        let hr = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        match hr {
            S_OK | S_FALSE => Ok(ComInit { uninit: true }),
            // Already a multi-threaded apartment: COM is usable as is.
            RPC_E_CHANGED_MODE => Ok(ComInit { uninit: false }),
            _ => Err(hresult_error(hr)),
        }
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: balances the successful CoInitializeEx in `sta`.
            unsafe { CoUninitialize() };
        }
    }
}

/// Runs `f` on a fresh thread that is a single-threaded COM apartment.
pub(crate) fn on_sta_thread<T: Send>(f: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
    std::thread::scope(|s| {
        let worker = std::thread::Builder::new()
            .name("ringscan-com".into())
            .spawn_scoped(s, || {
                let _com = ComInit::sta()?;
                f()
            })?;
        worker
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

// --- interface pointers ----------------------------------------------------------

/// An owned COM interface pointer, released on drop.
struct ComPtr(*mut c_void);

impl ComPtr {
    /// The vtable, viewed as `V` (which must be a prefix of the real one).
    ///
    /// SAFETY: the pointer must implement an interface whose vtable starts
    /// with the layout of `V`.
    unsafe fn vtbl<V>(&self) -> &V {
        // SAFETY: a COM object starts with a pointer to its vtable.
        unsafe { &**(self.0 as *const *const V) }
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        // SAFETY: every COM interface starts with IUnknown.
        unsafe { (self.vtbl::<IUnknown_Vtbl>().Release)(self.0) };
    }
}

/// `IFileOperation`; slots we do not call are placeholders.
#[repr(C)]
struct IFileOperationVtbl {
    base: IUnknown_Vtbl,
    advise: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut u32) -> HRESULT,
    unadvise: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    set_operation_flags: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    _set_progress_message: usize,
    _set_progress_dialog: usize,
    _set_properties: usize,
    _set_owner_window: usize,
    _apply_properties_to_item: usize,
    _apply_properties_to_items: usize,
    _rename_item: usize,
    _rename_items: usize,
    _move_item: usize,
    _move_items: usize,
    _copy_item: usize,
    _copy_items: usize,
    delete_item: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT,
    _delete_items: usize,
    _new_item: usize,
    perform_operations: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    get_any_operations_aborted: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
}

/// The leading part of `IShellItem` up to `GetDisplayName`.
#[repr(C)]
struct IShellItemVtbl {
    base: IUnknown_Vtbl,
    _bind_to_handler: usize,
    _get_parent: usize,
    get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> HRESULT,
}

type ItemPtr = *mut c_void;
type Pcwstr = *const u16;

/// `IFileOperationProgressSink`, all 19 slots (we implement it).
#[repr(C)]
struct SinkVtbl {
    base: IUnknown_Vtbl,
    start_operations: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    finish_operations: unsafe extern "system" fn(*mut c_void, HRESULT) -> HRESULT,
    pre_rename_item: unsafe extern "system" fn(*mut c_void, u32, ItemPtr, Pcwstr) -> HRESULT,
    post_rename_item:
        unsafe extern "system" fn(*mut c_void, u32, ItemPtr, Pcwstr, HRESULT, ItemPtr) -> HRESULT,
    pre_move_item: unsafe extern "system" fn(*mut c_void, u32, ItemPtr, ItemPtr, Pcwstr) -> HRESULT,
    post_move_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        ItemPtr,
        ItemPtr,
        Pcwstr,
        HRESULT,
        ItemPtr,
    ) -> HRESULT,
    pre_copy_item: unsafe extern "system" fn(*mut c_void, u32, ItemPtr, ItemPtr, Pcwstr) -> HRESULT,
    post_copy_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        ItemPtr,
        ItemPtr,
        Pcwstr,
        HRESULT,
        ItemPtr,
    ) -> HRESULT,
    pre_delete_item: unsafe extern "system" fn(*mut c_void, u32, ItemPtr) -> HRESULT,
    post_delete_item:
        unsafe extern "system" fn(*mut c_void, u32, ItemPtr, HRESULT, ItemPtr) -> HRESULT,
    pre_new_item: unsafe extern "system" fn(*mut c_void, u32, ItemPtr, Pcwstr) -> HRESULT,
    post_new_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        ItemPtr,
        Pcwstr,
        Pcwstr,
        u32,
        HRESULT,
        ItemPtr,
    ) -> HRESULT,
    update_progress: unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT,
    reset_timer: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pause_timer: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    resume_timer: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

const _: () = {
    use core::mem::offset_of;
    const P: usize = size_of::<usize>();
    assert!(size_of::<IUnknown_Vtbl>() == 3 * P);
    assert!(offset_of!(IFileOperationVtbl, advise) == 3 * P);
    assert!(offset_of!(IFileOperationVtbl, set_operation_flags) == 5 * P);
    assert!(offset_of!(IFileOperationVtbl, delete_item) == 18 * P);
    assert!(offset_of!(IFileOperationVtbl, perform_operations) == 21 * P);
    assert!(size_of::<IFileOperationVtbl>() == 23 * P);
    assert!(offset_of!(IShellItemVtbl, get_display_name) == 5 * P);
    assert!(offset_of!(SinkVtbl, pre_delete_item) == 11 * P);
    assert!(offset_of!(SinkVtbl, post_delete_item) == 12 * P);
    assert!(size_of::<SinkVtbl>() == 19 * P);
};

/// Full file-system path of a shell item (for error messages).
///
/// SAFETY: `item` must be a live `IShellItem` pointer.
unsafe fn item_path(item: ItemPtr) -> Option<PathBuf> {
    if item.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `item` is an IShellItem.
    let vtbl = unsafe { &**(item as *const *const IShellItemVtbl) };
    let mut name: *mut u16 = std::ptr::null_mut();
    // SAFETY: valid out-pointer; the string is freed below.
    if unsafe { (vtbl.get_display_name)(item, SIGDN_FILESYSPATH, &mut name) } < 0 || name.is_null()
    {
        return None;
    }
    // SAFETY: GetDisplayName returns a NUL-terminated string.
    let units = unsafe {
        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
        std::slice::from_raw_parts(name, len).to_vec()
    };
    // SAFETY: the string was allocated with CoTaskMemAlloc.
    unsafe { CoTaskMemFree(name.cast()) };
    use std::os::windows::ffi::OsStringExt;
    Some(std::ffi::OsString::from_wide(&units).into())
}

// --- the progress sink -------------------------------------------------------------

/// What the sink observed during one `PerformOperations`.
#[derive(Default, Debug)]
pub(crate) struct Observed {
    /// First item that the shell wanted to delete without recycling (and
    /// that we therefore stopped).
    pub refused: Option<PathBuf>,
    /// Deletions that failed (or were cancelled), with their HRESULT.
    pub failed: Vec<(Option<PathBuf>, HRESULT)>,
    /// `PreDeleteItem` flags seen (diagnostics).
    pub pre_delete_flags: Vec<u32>,
}

/// Our `IFileOperationProgressSink` object. `vtbl` must stay the first field.
#[repr(C)]
struct Sink {
    vtbl: &'static SinkVtbl,
    refs: AtomicU32,
    observed: Mutex<Observed>,
}

static SINK_VTBL: SinkVtbl = SinkVtbl {
    base: IUnknown_Vtbl {
        QueryInterface: sink_query_interface,
        AddRef: sink_add_ref,
        Release: sink_release,
    },
    start_operations: ok0,
    finish_operations: ok_hr,
    pre_rename_item: ok_pre_name,
    post_rename_item: ok_post_rename,
    pre_move_item: ok_pre_move,
    post_move_item: ok_post_move,
    pre_copy_item: ok_pre_move,
    post_copy_item: ok_post_move,
    pre_delete_item: sink_pre_delete_item,
    post_delete_item: sink_post_delete_item,
    pre_new_item: ok_pre_name,
    post_new_item: ok_post_new,
    update_progress: ok_progress,
    reset_timer: ok0,
    pause_timer: ok0,
    resume_timer: ok0,
};

unsafe extern "system" fn sink_query_interface(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_NOINTERFACE;
    }
    // SAFETY: COM passes a valid IID pointer.
    let iid = unsafe { &*iid };
    if guid_eq(iid, &IID_IUnknown) || guid_eq(iid, &IID_IFILE_OPERATION_PROGRESS_SINK) {
        // SAFETY: `this` is our Sink; `out` is a valid out-pointer.
        unsafe {
            sink_add_ref(this);
            *out = this;
        }
        S_OK
    } else {
        // SAFETY: `out` is a valid out-pointer.
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE
    }
}

unsafe extern "system" fn sink_add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` points to a live Sink.
    let sink = unsafe { &*(this as *const Sink) };
    sink.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn sink_release(this: *mut c_void) -> u32 {
    // SAFETY: `this` points to a live Sink.
    let left = unsafe { &*(this as *const Sink) }
        .refs
        .fetch_sub(1, Ordering::AcqRel)
        - 1;
    if left == 0 {
        // SAFETY: last reference: the Sink was created by Box::into_raw.
        drop(unsafe { Box::from_raw(this as *mut Sink) });
    }
    left
}

/// Returns the sink's `Observed` state.
///
/// SAFETY: `this` must point to a live Sink.
unsafe fn observed<'a>(this: *mut c_void) -> std::sync::MutexGuard<'a, Observed> {
    // SAFETY: guaranteed by the caller.
    let sink = unsafe { &*(this as *const Sink) };
    sink.observed.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe extern "system" fn sink_pre_delete_item(
    this: *mut c_void,
    flags: u32,
    item: ItemPtr,
) -> HRESULT {
    // SAFETY: `this` is our Sink, `item` an IShellItem (or null).
    let mut obs = unsafe { observed(this) };
    obs.pre_delete_flags.push(flags);
    if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE as u32 != 0 {
        return S_OK;
    }
    // The shell is about to destroy this item for good: refuse. Any error
    // here cancels this deletion and all operations after it.
    if obs.refused.is_none() {
        // SAFETY: as above.
        obs.refused = Some(unsafe { item_path(item) }.unwrap_or_default());
    }
    E_ABORT
}

unsafe extern "system" fn sink_post_delete_item(
    this: *mut c_void,
    _flags: u32,
    item: ItemPtr,
    hr: HRESULT,
    _new_item: ItemPtr,
) -> HRESULT {
    if hr < 0 {
        // SAFETY: `this` is our Sink, `item` an IShellItem (or null).
        let path = unsafe { item_path(item) };
        // SAFETY: as above.
        unsafe { observed(this) }.failed.push((path, hr));
    }
    S_OK
}

unsafe extern "system" fn ok0(_: *mut c_void) -> HRESULT {
    S_OK
}
unsafe extern "system" fn ok_hr(_: *mut c_void, _: HRESULT) -> HRESULT {
    S_OK
}
unsafe extern "system" fn ok_pre_name(_: *mut c_void, _: u32, _: ItemPtr, _: Pcwstr) -> HRESULT {
    S_OK
}
unsafe extern "system" fn ok_post_rename(
    _: *mut c_void,
    _: u32,
    _: ItemPtr,
    _: Pcwstr,
    _: HRESULT,
    _: ItemPtr,
) -> HRESULT {
    S_OK
}
unsafe extern "system" fn ok_pre_move(
    _: *mut c_void,
    _: u32,
    _: ItemPtr,
    _: ItemPtr,
    _: Pcwstr,
) -> HRESULT {
    S_OK
}
unsafe extern "system" fn ok_post_move(
    _: *mut c_void,
    _: u32,
    _: ItemPtr,
    _: ItemPtr,
    _: Pcwstr,
    _: HRESULT,
    _: ItemPtr,
) -> HRESULT {
    S_OK
}
unsafe extern "system" fn ok_post_new(
    _: *mut c_void,
    _: u32,
    _: ItemPtr,
    _: Pcwstr,
    _: Pcwstr,
    _: u32,
    _: HRESULT,
    _: ItemPtr,
) -> HRESULT {
    S_OK
}
unsafe extern "system" fn ok_progress(_: *mut c_void, _: u32, _: u32) -> HRESULT {
    S_OK
}

// --- the operation -------------------------------------------------------------------

/// Outcome of [`recycle`] before the caller's own existence checks.
pub(crate) struct RecycleReport {
    /// HRESULT of `PerformOperations`.
    pub hr: HRESULT,
    /// `GetAnyOperationsAborted`.
    pub aborted: bool,
    pub observed: Observed,
}

/// Moves `paths` (plain absolute paths, NUL-terminated UTF-16) to the
/// Recycle Bin with `IFileOperation`; nothing is ever deleted permanently
/// (see the module docs). Must run on an STA thread ([`on_sta_thread`]).
pub(crate) fn recycle(paths: &[Vec<u16>]) -> io::Result<RecycleReport> {
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid CLSID/IID pointers and out-pointer.
    check_hr(unsafe {
        CoCreateInstance(
            &FileOperation,
            std::ptr::null_mut(),
            CLSCTX_ALL,
            &IID_IFILE_OPERATION,
            &mut raw,
        )
    })?;
    let op = ComPtr(raw);
    // SAFETY: `op` is an IFileOperation.
    let vt = unsafe { op.vtbl::<IFileOperationVtbl>() };

    // FOF_ALLOWUNDO + FOFX_RECYCLEONDELETE: recycle. FOF_NOCONFIRMATION,
    // FOF_SILENT, FOF_NOERRORUI: no UI (the app asks the user itself).
    // FOFX_EARLYFAILURE: stop at the first error. The sink below makes sure
    // "no confirmation" can never turn into a permanent delete.
    let flags = FOF_ALLOWUNDO
        | FOFX_RECYCLEONDELETE
        | FOF_NOCONFIRMATION
        | FOF_SILENT
        | FOF_NOERRORUI
        | FOFX_EARLYFAILURE;
    // SAFETY: valid object and flags.
    check_hr(unsafe { (vt.set_operation_flags)(op.0, flags) })?;

    let sink = Box::into_raw(Box::new(Sink {
        vtbl: &SINK_VTBL,
        refs: AtomicU32::new(1),
        observed: Mutex::new(Observed::default()),
    }));
    // Our own reference is released when `_sink_ref` drops.
    let _sink_ref = ComPtr(sink.cast());
    let mut cookie = 0u32;
    // SAFETY: `sink` is a valid IFileOperationProgressSink; Advise AddRefs it.
    check_hr(unsafe { (vt.advise)(op.0, sink.cast(), &mut cookie) })?;

    let queued = (|| {
        for p in paths {
            let mut item: *mut c_void = std::ptr::null_mut();
            // SAFETY: `p` is NUL-terminated; valid IID and out-pointer.
            check_hr(unsafe {
                SHCreateItemFromParsingName(
                    p.as_ptr(),
                    std::ptr::null_mut(),
                    &IID_ISHELL_ITEM,
                    &mut item,
                )
            })?;
            let item = ComPtr(item);
            // SAFETY: valid IFileOperation and IShellItem; no per-item sink.
            check_hr(unsafe { (vt.delete_item)(op.0, item.0, std::ptr::null_mut()) })?;
        }
        Ok::<(), io::Error>(())
    })();

    let result = queued.map(|()| {
        // SAFETY: valid object.
        let hr = unsafe { (vt.perform_operations)(op.0) };
        let mut aborted = 0i32;
        // SAFETY: valid object and out-pointer.
        unsafe { (vt.get_any_operations_aborted)(op.0, &mut aborted) };
        (hr, aborted != 0)
    });
    // SAFETY: `cookie` came from our Advise; Unadvise releases its reference.
    unsafe { (vt.unadvise)(op.0, cookie) };
    let (hr, aborted) = result?;
    // SAFETY: `_sink_ref` still holds a reference, so the sink is alive.
    let observed = std::mem::take(&mut *unsafe { observed(sink.cast()) });
    Ok(RecycleReport {
        hr,
        aborted,
        observed,
    })
}
