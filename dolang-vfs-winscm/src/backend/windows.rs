//! Real Windows SCM backend.

use std::{
    ffi::OsStr,
    io,
    os::windows::ffi::OsStrExt,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{Arc, Mutex, Weak},
};

use dolang_vfs::{Error, ErrorKind, ExtContext, InvalidHandle, OperatingSystem};
use dolang_winterop::{
    ALL_SECURITY_INFORMATION, ApcCancelled, ApcContext, DACL_SECURITY_INFORMATION, Reactor,
    SACL_SECURITY_INFORMATION, SecDesc as VfsSecDesc, with_security_privilege,
};
use futures::channel::oneshot;
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
        ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS, ERROR_SUCCESS, GetLastError,
    },
    Security::{
        PROTECTED_DACL_SECURITY_INFORMATION, PROTECTED_SACL_SECURITY_INFORMATION,
        UNPROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_SACL_SECURITY_INFORMATION,
    },
    System::Services::{
        ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
        ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW, NotifyServiceStatusChangeW,
        OpenSCManagerW, OpenServiceW, PFN_SC_NOTIFY_CALLBACK, QUERY_SERVICE_CONFIGW,
        QueryServiceConfigW, QueryServiceObjectSecurity, QueryServiceStatusEx,
        SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_STATUS_PROCESS_INFO, SERVICE_NO_CHANGE,
        SERVICE_NOTIFY_2W, SERVICE_NOTIFY_STATUS_CHANGE, SERVICE_STATUS, SERVICE_STATUS_PROCESS,
        SetServiceObjectSecurity, StartServiceW,
    },
};

use crate::{
    manager::ScManager,
    service::Service,
    wire::{
        CreateServiceOptions, ServiceAccess, ServiceConfig, ServiceConfigUpdate,
        ServiceControlsAccepted, ServiceInfo, ServiceState, ServiceStatus, ServiceType,
        WinScmRequest, WinScmResponse,
    },
};

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Reads a NUL-terminated wide string from a raw pointer.
///
/// # Safety
///
/// `ptr` must be non-null and point to a valid NUL-terminated UTF-16 string.
unsafe fn from_wide(ptr: *const u16) -> String {
    // SAFETY: guaranteed by caller.
    let len = unsafe { (0..).take_while(|&i| *ptr.add(i) != 0).count() };
    // SAFETY: `ptr..ptr+len` is the substring established above, excluding
    // the NUL terminator.
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf16_lossy(slice)
}

unsafe fn from_multi_wide(mut ptr: *const u16) -> Vec<String> {
    let mut values = Vec::new();
    if ptr.is_null() {
        return values;
    }
    loop {
        // SAFETY: guaranteed by the caller, and advanced only across strings
        // within the same double-NUL-terminated buffer.
        let len = unsafe { (0..).take_while(|&i| *ptr.add(i) != 0).count() };
        if len == 0 {
            break;
        }
        // SAFETY: the preceding scan established this string's bounds.
        let value = unsafe { std::slice::from_raw_parts(ptr, len) };
        values.push(String::from_utf16_lossy(value));
        // SAFETY: `len + 1` advances past this string and its terminator.
        ptr = unsafe { ptr.add(len + 1) };
    }
    values
}

fn optional_wide(value: Option<&str>) -> Option<Vec<u16>> {
    value.map(wide)
}

fn multi_wide(values: &[String]) -> Vec<u16> {
    let mut result = Vec::new();
    for value in values {
        result.extend(OsStr::new(value).encode_wide());
        result.push(0);
    }
    result.push(0);
    if values.is_empty() {
        result.push(0);
    }
    result
}

fn optional_ptr(value: Option<&Vec<u16>>) -> *const u16 {
    value.map_or(ptr::null(), |value| value.as_ptr())
}

fn from_win32(operation: &str, code: u32) -> Error {
    let kind = match code {
        ERROR_ACCESS_DENIED => ErrorKind::PermissionDenied,
        ERROR_SERVICE_DOES_NOT_EXIST => ErrorKind::NotFound,
        ERROR_SERVICE_EXISTS => ErrorKind::AlreadyExists,
        _ => ErrorKind::Other,
    };
    Error::from_system_code(
        kind,
        format!("{operation}: SCM error {code}"),
        OperatingSystem::Windows,
        code as i32,
    )
}

fn last_error(operation: &str) -> Error {
    // SAFETY: no preconditions.
    from_win32(operation, unsafe { GetLastError() })
}

fn status_from_raw(status: &SERVICE_STATUS_PROCESS) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType(status.dwServiceType),
        current_state: ServiceState(status.dwCurrentState),
        controls_accepted: ServiceControlsAccepted(status.dwControlsAccepted),
        win32_exit_code: status.dwWin32ExitCode,
        service_specific_exit_code: status.dwServiceSpecificExitCode,
        check_point: status.dwCheckPoint,
        wait_hint: status.dwWaitHint,
        process_id: status.dwProcessId,
    }
}

/// Returns the shared SCM notification reactor, creating it if this is the
/// first live [`Service`] handle, or reusing the existing one if another
/// `Service` already has it open.
///
/// The returned `Arc<Reactor>` is stored on the `Service` struct itself
/// (see [`crate::service::Service`]) so the reactor's background thread
/// stays alive for exactly as long as at least one `Service` handle
/// referencing it is open, and no longer: this cache never calls
/// `ReactorControl::cancel()`/`join()` itself. Once every `Arc<Reactor>`
/// clone handed out here is dropped, the reactor's own internal refcount
/// reaches zero on its own and the background thread exits via the natural-
/// quiescence path `dolang_winterop::apc` already implements and tests
/// (`join_resolves_without_cancel_once_every_handle_is_dropped`).
pub(crate) fn reactor() -> io::Result<Arc<Reactor>> {
    static CACHE: Mutex<Weak<Reactor>> = Mutex::new(Weak::new());
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(reactor) = cache.upgrade() {
        return Ok(reactor);
    }
    // `Reactor::new()` only awaits a one-shot ready handshake with the
    // freshly spawned thread — no tokio runtime is required to drive it.
    let (reactor, control) = futures::executor::block_on(Reactor::new())?;
    // Deliberately dropped, not stored: see the doc comment above.
    drop(control);
    let reactor = Arc::new(reactor);
    *cache = Arc::downgrade(&reactor);
    Ok(reactor)
}

fn open_manager(access: ServiceAccess) -> Result<SC_HANDLE, Error> {
    let open = || {
        // SAFETY: both name pointers are null (local machine, default
        // database), which is documented as valid.
        let handle = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), access.0) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    };
    let result = if access.0 & ServiceAccess::ACCESS_SYSTEM_SECURITY.0 != 0 {
        with_security_privilege(open)
    } else {
        open()
    };
    result.map_err(|error| from_io("open SC manager", error))
}

fn open_service(manager: SC_HANDLE, name: &str, access: ServiceAccess) -> Result<SC_HANDLE, Error> {
    let name = wide(name);
    let open = || {
        // SAFETY: `name` is NUL-terminated; `manager` is a live SC manager handle.
        let handle = unsafe { OpenServiceW(manager, name.as_ptr(), access.0) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    };
    let result = if access.0 & ServiceAccess::ACCESS_SYSTEM_SECURITY.0 != 0 {
        with_security_privilege(open)
    } else {
        open()
    };
    result.map_err(|error| from_io("open service", error))
}

/// Enumerates services in `manager` matching `service_type`/`state_filter`,
/// via `EnumServicesStatusExW`.
///
/// `EnumServicesStatusExW` pages internally: a single call fills as many
/// whole entries as fit in the caller's buffer, then reports
/// `ERROR_MORE_DATA` (advancing an opaque resume handle) if more remain.
/// This loops until a call succeeds outright (no more data), fetching a
/// right-sized buffer for each chunk by first probing with a zero-length
/// buffer — `needed` on that probe is the exact size required for the next
/// chunk starting from the current resume position.
fn enum_services(
    manager: SC_HANDLE,
    service_type: u32,
    state_filter: u32,
) -> Result<Vec<ServiceInfo>, Error> {
    let mut resume_handle: u32 = 0;
    let mut services = Vec::new();
    loop {
        let mut needed = 0u32;
        let mut returned = 0u32;
        // SAFETY: `manager` is a live SC manager handle; a null/zero-length
        // buffer is documented as valid for sizing the next chunk.
        let probe_ok = unsafe {
            EnumServicesStatusExW(
                manager,
                SC_ENUM_PROCESS_INFO,
                service_type,
                state_filter,
                ptr::null_mut(),
                0,
                &mut needed,
                &mut returned,
                &mut resume_handle,
                ptr::null(),
            )
        };
        if probe_ok != 0 {
            // Nothing left to enumerate.
            break;
        }
        let probe_error = unsafe { GetLastError() };
        if probe_error != ERROR_MORE_DATA {
            return Err(from_win32("enumerate services", probe_error));
        }
        if needed == 0 {
            break;
        }

        let mut buf = vec![0u8; needed as usize];
        let mut fetched = 0u32;
        let mut needed2 = 0u32;
        // SAFETY: `buf` is sized exactly to `needed` from the probe above,
        // which `EnumServicesStatusExW` documents as sufficient for at
        // least the next entry.
        let fetch_ok = unsafe {
            EnumServicesStatusExW(
                manager,
                SC_ENUM_PROCESS_INFO,
                service_type,
                state_filter,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut needed2,
                &mut fetched,
                &mut resume_handle,
                ptr::null(),
            )
        };
        let more = if fetch_ok == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_MORE_DATA {
                return Err(from_win32("enumerate services", code));
            }
            true
        } else {
            false
        };

        // SAFETY: `buf` holds exactly `fetched` densely packed
        // `ENUM_SERVICE_STATUS_PROCESSW` entries, per
        // `EnumServicesStatusExW`'s documented output layout for
        // `SC_ENUM_PROCESS_INFO`; the name pointers inside point into `buf`
        // itself and stay valid for as long as `buf` is alive.
        let entries = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW,
                fetched as usize,
            )
        };
        for entry in entries {
            services.push(ServiceInfo {
                // SAFETY: NUL-terminated wide strings pointing into `buf`,
                // per the slice's own safety comment above.
                name: unsafe { from_wide(entry.lpServiceName) },
                display_name: unsafe { from_wide(entry.lpDisplayName) },
                status: status_from_raw(&entry.ServiceStatusProcess),
            });
        }

        if !more {
            break;
        }
    }
    Ok(services)
}

/// Opens a fresh handle to the service named `name`, scoped to
/// [`wait_for_status_change`]'s single use: registering exactly one
/// `NotifyServiceStatusChangeW` request. See [`crate::service::Service`]'s
/// doc comment for why this can't just reuse the `Service`'s own handle.
fn open_notify_handle(name: &str) -> Result<SC_HANDLE, Error> {
    let manager = open_manager(ServiceAccess::SC_MANAGER_CONNECT)?;
    let result = open_service(manager, name, ServiceAccess::SERVICE_QUERY_STATUS);
    // SAFETY: `manager` is a live handle from `open_manager` above, no
    // longer needed once `open_service` has (or hasn't) used it.
    unsafe {
        CloseServiceHandle(manager);
    }
    result
}

/// Closes a service handle when dropped.
///
/// Used for [`wait_for_status_change`]'s dedicated notification handle,
/// where the timing of the close matters: on cancellation it must happen
/// *before* the hazard drain (see that function), so it's closed
/// explicitly there via `drop(handle)` rather than left to run implicitly
/// at scope exit — this type exists so every other return path still gets
/// the close for free without repeating it.
struct AutoCloseHandle(SC_HANDLE);

impl Drop for AutoCloseHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live handle owned by this value; nothing
        // else can close it first since `AutoCloseHandle` isn't `Clone`.
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create_service(
    manager: SC_HANDLE,
    name: &str,
    display_name: &str,
    service_type: u32,
    start_type: u32,
    error_control: u32,
    binary_path: &str,
    options: &CreateServiceOptions,
    access: ServiceAccess,
) -> Result<SC_HANDLE, Error> {
    let name = wide(name);
    let display_name = wide(display_name);
    let binary_path = wide(binary_path);
    let load_order_group = optional_wide(options.load_order_group.as_deref());
    let dependencies =
        (!options.dependencies.is_empty()).then(|| multi_wide(&options.dependencies));
    let service_start_name = optional_wide(options.service_start_name.as_deref());
    let password = optional_wide(options.password.as_deref());
    let create = || {
        // SAFETY: every string pointer passed is NUL-terminated and kept alive
        // for the duration of the call.
        let handle = unsafe {
            CreateServiceW(
                manager,
                name.as_ptr(),
                display_name.as_ptr(),
                access.0,
                service_type,
                start_type,
                error_control,
                binary_path.as_ptr(),
                optional_ptr(load_order_group.as_ref()),
                ptr::null_mut(),
                optional_ptr(dependencies.as_ref()),
                optional_ptr(service_start_name.as_ref()),
                optional_ptr(password.as_ref()),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    };
    let result = if access.0 & ServiceAccess::ACCESS_SYSTEM_SECURITY.0 != 0 {
        with_security_privilege(create)
    } else {
        create()
    };
    result.map_err(|error| from_io("create service", error))
}

fn delete_service(handle: SC_HANDLE) -> Result<(), Error> {
    // SAFETY: `handle` is a live service handle.
    if unsafe { DeleteService(handle) } == 0 {
        return Err(last_error("delete service"));
    }
    Ok(())
}

fn start_service(handle: SC_HANDLE, args: &[String]) -> Result<(), Error> {
    let wide_args: Vec<Vec<u16>> = args.iter().map(|arg| wide(arg)).collect();
    let arg_ptrs: Vec<*const u16> = wide_args.iter().map(|arg| arg.as_ptr()).collect();
    let args_ptr = if arg_ptrs.is_empty() {
        ptr::null()
    } else {
        arg_ptrs.as_ptr()
    };
    // SAFETY: `handle` is live and every pointer remains valid for the call.
    if unsafe { StartServiceW(handle, arg_ptrs.len() as u32, args_ptr) } == 0 {
        return Err(last_error("start service"));
    }
    Ok(())
}

fn query_config(handle: SC_HANDLE) -> Result<ServiceConfig, Error> {
    let mut needed = 0u32;
    // SAFETY: a null, zero-length buffer is the documented sizing call.
    let ok = unsafe { QueryServiceConfigW(handle, ptr::null_mut(), 0, &mut needed) };
    // SAFETY: no preconditions.
    let code = unsafe { GetLastError() };
    if ok != 0 || code != ERROR_INSUFFICIENT_BUFFER {
        return Err(from_win32("query service config", code));
    }
    let words = (needed as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; words];
    // SAFETY: the allocation is aligned for the header and has `needed` bytes;
    // all pointers in the returned header refer into this live allocation.
    if unsafe { QueryServiceConfigW(handle, buffer.as_mut_ptr().cast(), needed, &mut needed) } == 0
    {
        return Err(last_error("query service config"));
    }
    // SAFETY: the successful call initialized a QUERY_SERVICE_CONFIGW header.
    let config = unsafe { &*(buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>()) };
    // SAFETY: successful QueryServiceConfigW returns valid NUL-terminated
    // pointers into `buffer` for these fields.
    let binary_path = unsafe { from_wide(config.lpBinaryPathName) };
    let load_order_group = if config.lpLoadOrderGroup.is_null() {
        None
    } else {
        // SAFETY: same as above.
        let value = unsafe { from_wide(config.lpLoadOrderGroup) };
        (!value.is_empty()).then_some(value)
    };
    // SAFETY: dependencies is either null or a double-NUL-terminated buffer.
    let dependencies = unsafe { from_multi_wide(config.lpDependencies) };
    let service_start_name = unsafe { from_wide(config.lpServiceStartName) };
    let display_name = unsafe { from_wide(config.lpDisplayName) };
    Ok(ServiceConfig {
        service_type: ServiceType(config.dwServiceType),
        start_type: crate::wire::StartType(config.dwStartType),
        error_control: crate::wire::ErrorControl(config.dwErrorControl),
        binary_path,
        load_order_group,
        tag_id: config.dwTagId,
        dependencies,
        service_start_name,
        display_name,
    })
}

fn change_config(handle: SC_HANDLE, update: &ServiceConfigUpdate) -> Result<(), Error> {
    let binary_path = optional_wide(update.binary_path.as_deref());
    let load_order_group = optional_wide(update.load_order_group.as_deref());
    let dependencies = update
        .dependencies
        .as_ref()
        .map(|values| multi_wide(values));
    let service_start_name = optional_wide(update.service_start_name.as_deref());
    let password = optional_wide(update.password.as_deref());
    let display_name = optional_wide(update.display_name.as_deref());
    // SAFETY: `handle` is live and all supplied string buffers remain valid
    // for the duration of the call; null pointers and SERVICE_NO_CHANGE leave
    // their corresponding fields unchanged.
    let ok = unsafe {
        ChangeServiceConfigW(
            handle,
            update
                .service_type
                .map_or(SERVICE_NO_CHANGE, |value| value.0),
            update.start_type.map_or(SERVICE_NO_CHANGE, |value| value.0),
            update
                .error_control
                .map_or(SERVICE_NO_CHANGE, |value| value.0),
            optional_ptr(binary_path.as_ref()),
            optional_ptr(load_order_group.as_ref()),
            ptr::null_mut(),
            optional_ptr(dependencies.as_ref()),
            optional_ptr(service_start_name.as_ref()),
            optional_ptr(password.as_ref()),
            optional_ptr(display_name.as_ref()),
        )
    };
    if ok == 0 {
        return Err(last_error("change service config"));
    }
    Ok(())
}

fn control_service(handle: SC_HANDLE, control: u32) -> Result<ServiceStatus, Error> {
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is a live service handle; `status` is a valid out pointer.
    if unsafe { ControlService(handle, control, &mut status) } == 0 {
        return Err(last_error("control service"));
    }
    Ok(ServiceStatus {
        service_type: ServiceType(status.dwServiceType),
        current_state: ServiceState(status.dwCurrentState),
        controls_accepted: ServiceControlsAccepted(status.dwControlsAccepted),
        win32_exit_code: status.dwWin32ExitCode,
        service_specific_exit_code: status.dwServiceSpecificExitCode,
        check_point: status.dwCheckPoint,
        wait_hint: status.dwWaitHint,
        process_id: 0,
    })
}

fn query_status(handle: SC_HANDLE) -> Result<ServiceStatus, Error> {
    let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed = 0u32;
    // SAFETY: `status`/`needed` describe a live, correctly-sized buffer for
    // `SC_STATUS_PROCESS_INFO`.
    let ok = unsafe {
        QueryServiceStatusEx(
            handle,
            SC_STATUS_PROCESS_INFO,
            &mut status as *mut _ as *mut u8,
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(last_error("query service status"));
    }
    Ok(status_from_raw(&status))
}

/// Converts an `io::Error` carrying a raw Win32 error code (as produced by
/// [`sec_desc`]/[`set_sec_desc`] below) back into this crate's [`Error`]
/// type, using the same code-to-`ErrorKind` mapping as every other
/// operation in this file.
fn from_io(operation: &str, err: io::Error) -> Error {
    match err.raw_os_error() {
        Some(code) => from_win32(operation, code as u32),
        None => Error::new(ErrorKind::Other, format!("{operation}: {err}")),
    }
}

/// Fetches `handle`'s security descriptor via `QueryServiceObjectSecurity`,
/// which (like `RegGetKeySecurity` in `dolang-vfs-winreg`) returns the same
/// native self-relative byte blob `SecDesc::from_bytes_with_mask` already
/// parses.
fn sec_desc(handle: SC_HANDLE, mask: u32) -> Result<VfsSecDesc, Error> {
    let mask = mask & ALL_SECURITY_INFORMATION;
    let query_mask = if mask == 0 {
        dolang_winterop::OWNER_SECURITY_INFORMATION
    } else {
        mask
    };
    let mut bytes = vec![0u8; 256];
    loop {
        let mut needed = 0u32;
        // SAFETY: `bytes`/`needed` describe a live, correctly-sized buffer.
        let ok = unsafe {
            QueryServiceObjectSecurity(
                handle,
                query_mask,
                bytes.as_mut_ptr().cast(),
                bytes.len() as u32,
                &mut needed,
            )
        };
        if ok != 0 {
            bytes.truncate(needed as usize);
            break;
        }
        // SAFETY: no preconditions.
        let code = unsafe { GetLastError() };
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(from_win32("get service security", code));
        }
        bytes.resize(needed as usize, 0);
    }
    VfsSecDesc::from_bytes_with_mask(&bytes, query_mask)
        .map_err(|error| Error::new(ErrorKind::Other, error.to_string()))
}

/// Sets `handle`'s security descriptor via `SetServiceObjectSecurity`,
/// passing the native self-relative byte blob `SecDesc::to_bytes` produces
/// straight through — same shape as `dolang-vfs-winreg`'s `set_sec_desc`.
fn set_sec_desc(handle: SC_HANDLE, descriptor: &VfsSecDesc) -> Result<(), Error> {
    let mut mask = descriptor.mask() & ALL_SECURITY_INFORMATION;
    if mask == 0 {
        return Ok(());
    }
    if mask & DACL_SECURITY_INFORMATION != 0 {
        mask |= if descriptor.dacl_protected() {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
    }
    if mask & SACL_SECURITY_INFORMATION != 0 {
        mask |= if descriptor.sacl_protected() {
            PROTECTED_SACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_SACL_SECURITY_INFORMATION
        };
    }
    let bytes = descriptor.to_bytes();
    let set = || {
        // SAFETY: `bytes` describes a live, native self-relative security
        // descriptor of length `bytes.len()`.
        let ok =
            unsafe { SetServiceObjectSecurity(handle, mask, bytes.as_ptr().cast_mut().cast()) };
        if ok != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    let result = if mask & SACL_SECURITY_INFORMATION != 0 {
        with_security_privilege(set)
    } else {
        set()
    };
    result.map_err(|error| from_io("set service security", error))
}

/// Per-registration state for an in-flight `NotifyServiceStatusChangeW`
/// call.
///
/// Boxed so its address is stable for as long as SCM might still write into
/// `buf`/invoke `callback` — both the buffer passed to
/// `NotifyServiceStatusChangeW` and the `pContext` value it hands back to
/// the callback must remain valid until the callback is known to have
/// already fired or to never fire again (see [`wait_for_status_change`]'s
/// cancellation path for how that's proven).
struct NotifyCell {
    buf: SERVICE_NOTIFY_2W,
    tx: Option<oneshot::Sender<ServiceStatus>>,
}

// SAFETY: `NotifyCell` is only ever touched from the reactor thread: it's
// created there, its address is only ever read by SCM/the trampoline it
// invokes (also on the reactor thread, per `NotifyServiceStatusChangeW`'s
// documented delivery), and it's freed there too.
unsafe impl Send for NotifyCell {}

unsafe extern "system" fn notify_trampoline(pparameter: *const std::ffi::c_void) {
    // SAFETY: `pparameter` is the `pContext` we passed in, which is always
    // a live `*mut NotifyCell` for as long as this callback can be invoked
    // (see `NotifyCell`'s doc comment). Panics are caught since unwinding
    // across this `extern "system"` boundary is undefined behavior.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let cell = &mut *(pparameter as *mut NotifyCell);
        if let Some(tx) = cell.tx.take() {
            let _ = tx.send(status_from_raw(&cell.buf.ServiceStatus));
        }
    }));
}

/// Async-waits for the named service's status to change, using the APC
/// reactor.
///
/// Must run on the reactor thread via [`dolang_winterop::Reactor::submit`] —
/// `NotifyServiceStatusChangeW` documents its callback as being invoked on
/// the thread that registered it, the next time that thread enters an
/// alertable wait, which is exactly what the reactor thread does in a loop.
///
/// Opens its own dedicated handle via [`open_notify_handle`] rather than
/// taking one from the caller — see [`crate::service::Service`]'s doc
/// comment for why.
async fn wait_for_status_change(
    apc_ctx: &mut ApcContext,
    name: String,
    mask: u32,
) -> Result<ServiceStatus, Error> {
    let handle = AutoCloseHandle(open_notify_handle(&name)?);

    let (tx, rx) = oneshot::channel();
    let mut cell = Box::new(NotifyCell {
        buf: unsafe { std::mem::zeroed() },
        tx: Some(tx),
    });
    cell.buf.dwVersion = SERVICE_NOTIFY_STATUS_CHANGE;
    cell.buf.pfnNotifyCallback = notify_trampoline_ptr();
    cell.buf.pContext = &mut *cell as *mut NotifyCell as *mut _;

    // SAFETY: `handle.0` is a live service handle; `cell.buf` stays alive
    // (boxed, stable address) until this function proves no further
    // callback can reference it, on every return path below.
    let register_result = unsafe { NotifyServiceStatusChangeW(handle.0, mask, &cell.buf) };
    if register_result != ERROR_SUCCESS {
        return Err(from_win32("register status notification", register_result));
    }

    match apc_ctx.cancel_guard(async |_ctx| rx.await).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(_)) => Err(Error::new(
            ErrorKind::Other,
            "status notification sender dropped unexpectedly",
        )),
        Err(ApcCancelled) => {
            // Close the dedicated handle now: SCM has no "unregister
            // notification" API, so closing the handle a request was
            // registered on is the only documented way to cancel it. This
            // has to happen before the drain below, not just at scope
            // exit, since the drain's whole point is to prove no callback
            // referencing `cell` can still be delivered *after* this
            // close.
            drop(handle);

            // Hazard: the callback may already have been queued for
            // delivery before the close above took effect. APCs on a
            // given thread are strictly FIFO (see `dolang_winterop::apc`'s
            // own module doc for the identical reasoning behind its flush
            // marker), so posting our own raw APC and awaiting it proves
            // that anything already queued ahead of it — including a
            // just-about-to-fire notify callback — has been fully
            // delivered and processed by the time our own callback runs.
            // Only then is it safe to drop `cell`.
            let (drained_tx, drained_rx) = oneshot::channel();
            apc_ctx
                .post_raw(move || {
                    let _ = drained_tx.send(());
                })
                .map_err(|error| Error::new(ErrorKind::Other, error.to_string()))?;
            let _ = drained_rx.await;
            Err(Error::new(ErrorKind::Other, "status change wait cancelled"))
        }
    }
}

fn notify_trampoline_ptr() -> PFN_SC_NOTIFY_CALLBACK {
    Some(notify_trampoline)
}

fn invalid_handle(_: InvalidHandle) -> Error {
    Error::new(ErrorKind::InvalidInput, "invalid SCM handle")
}

pub(crate) async fn handle(
    ctx: &mut ExtContext<'_>,
    request: WinScmRequest,
) -> Result<WinScmResponse, Error> {
    match request {
        WinScmRequest::OpenManager { access } => {
            let handle = open_manager(access)?;
            Ok(WinScmResponse::Manager(ctx.register(ScManager(handle))))
        }
        WinScmRequest::CloseManager { manager } => {
            ctx.unregister::<ScManager>(manager)
                .map_err(invalid_handle)?;
            Ok(WinScmResponse::Closed)
        }
        WinScmRequest::OpenService {
            manager,
            name,
            access,
        } => {
            let guard = ctx.acquire::<ScManager>(manager).map_err(invalid_handle)?;
            let handle = open_service(guard.0, &name, access)?;
            let reactor =
                reactor().map_err(|error| Error::new(ErrorKind::Other, error.to_string()))?;
            Ok(WinScmResponse::Svc(ctx.register(Service {
                handle,
                reactor,
                name,
            })))
        }
        WinScmRequest::CreateService {
            manager,
            name,
            display_name,
            service_type,
            start_type,
            error_control,
            binary_path,
            options,
            access,
        } => {
            let guard = ctx.acquire::<ScManager>(manager).map_err(invalid_handle)?;
            let handle = create_service(
                guard.0,
                &name,
                &display_name,
                service_type.0,
                start_type.0,
                error_control.0,
                &binary_path,
                &options,
                access,
            )?;
            let reactor =
                reactor().map_err(|error| Error::new(ErrorKind::Other, error.to_string()))?;
            Ok(WinScmResponse::Svc(ctx.register(Service {
                handle,
                reactor,
                name,
            })))
        }
        WinScmRequest::EnumServices {
            manager,
            service_type,
            state_filter,
        } => {
            let guard = ctx.acquire::<ScManager>(manager).map_err(invalid_handle)?;
            let services = enum_services(guard.0, service_type.0, state_filter.0)?;
            Ok(WinScmResponse::Services(services))
        }
        WinScmRequest::DeleteService { service } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            delete_service(guard.handle)?;
            Ok(WinScmResponse::Deleted)
        }
        WinScmRequest::CloseService { service } => {
            ctx.unregister::<Service>(service).map_err(invalid_handle)?;
            Ok(WinScmResponse::Closed)
        }
        WinScmRequest::StartService { service, args } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            start_service(guard.handle, &args)?;
            Ok(WinScmResponse::Ack)
        }
        WinScmRequest::ControlService { service, control } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            let status = control_service(guard.handle, control.0)?;
            Ok(WinScmResponse::Status(status))
        }
        WinScmRequest::QueryStatus { service } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            let status = query_status(guard.handle)?;
            Ok(WinScmResponse::Status(status))
        }
        WinScmRequest::QueryConfig { service } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            Ok(WinScmResponse::Config(query_config(guard.handle)?))
        }
        WinScmRequest::ChangeConfig { service, update } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            change_config(guard.handle, &update)?;
            Ok(WinScmResponse::Ack)
        }
        WinScmRequest::GetSecDesc { service, mask } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            let descriptor = sec_desc(guard.handle, mask)?;
            Ok(WinScmResponse::SecDesc(descriptor))
        }
        WinScmRequest::SetSecDesc {
            service,
            sec_desc: descriptor,
        } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            set_sec_desc(guard.handle, &descriptor)?;
            Ok(WinScmResponse::Ack)
        }
        WinScmRequest::WaitForStatusChange { service, mask } => {
            let guard = ctx.acquire::<Service>(service).map_err(invalid_handle)?;
            let name = guard.name.clone();
            let reactor = guard.reactor.clone();
            drop(guard);
            let task = reactor
                .submit(async move |apc_ctx| wait_for_status_change(apc_ctx, name, mask.0).await)
                .map_err(|_closed| Error::new(ErrorKind::Other, "SCM reactor unavailable"))?;
            let status = task.await.map_err(|_cancelled| {
                Error::new(ErrorKind::Other, "status change wait cancelled")
            })??;
            Ok(WinScmResponse::Status(status))
        }
    }
}
