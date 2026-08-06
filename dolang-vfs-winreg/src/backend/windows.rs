//! Real Windows registry backend.

use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle},
    },
    ptr,
};

use dolang_vfs::{Error, ErrorKind, ExtContext, ExtOsHandle, InvalidHandle, OperatingSystem};
use dolang_winterop::{
    ALL_SECURITY_INFORMATION, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    SACL_SECURITY_INFORMATION, SecDesc as VfsSecDesc,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
        ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    },
    Security::{
        PROTECTED_DACL_SECURITY_INFORMATION, PROTECTED_SACL_SECURITY_INFORMATION,
        UNPROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_SACL_SECURITY_INFORMATION,
    },
    System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_CONFIG, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        HKEY_USERS, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_OPTION_NON_VOLATILE, RegCreateKeyExW,
        RegDeleteKeyExW, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW,
        RegGetKeySecurity, RegOpenKeyExW, RegQueryValueExW, RegSetKeySecurity, RegSetValueExW,
    },
    System::SystemServices::ACCESS_SYSTEM_SECURITY,
};

use crate::{
    key::Key,
    value::Value,
    wire::{Access, KeyHandle, PredefinedRoot, View, WinRegRequest, WinRegResponse},
};

/// Converts a non-predefined `HKEY` into an `OwnedHandle` that closes it via
/// `CloseHandle` rather than `RegCloseKey`.
///
/// `windows-sys`'s `HKEY` is `*mut core::ffi::c_void`, the same
/// representation as `RawHandle`, so this is a pointer reinterpretation, not
/// a numeric cast.
fn hkey_to_owned(hkey: HKEY) -> OwnedHandle {
    // SAFETY: `hkey` came from RegOpenKeyExW/RegCreateKeyExW, never a
    // predefined pseudo-handle; Microsoft documents such handles as usable
    // with generic kernel-handle APIs including DuplicateHandle/CloseHandle,
    // so treating it as an ordinary owned kernel handle here is sound.
    unsafe { OwnedHandle::from_raw_handle(hkey as RawHandle) }
}

fn owned_to_hkey(handle: OwnedHandle) -> HKEY {
    handle.into_raw_handle() as HKEY
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn from_win32(operation: &str, code: u32) -> Error {
    let kind = match code {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => ErrorKind::NotFound,
        ERROR_ACCESS_DENIED => ErrorKind::PermissionDenied,
        ERROR_ALREADY_EXISTS => ErrorKind::AlreadyExists,
        _ => ErrorKind::Other,
    };
    Error::from_system_code(
        kind,
        format!("{operation}: registry error {code}"),
        OperatingSystem::Windows,
        code as i32,
    )
}

fn sam(view: View, access: Access) -> u32 {
    let view = match view {
        View::Native => 0,
        View::Wow32 => KEY_WOW64_32KEY,
        View::Wow64 => KEY_WOW64_64KEY,
    };
    access.0 | view
}

fn view_sam(view: View) -> u32 {
    match view {
        View::Native => 0,
        View::Wow32 => KEY_WOW64_32KEY,
        View::Wow64 => KEY_WOW64_64KEY,
    }
}

fn predefined_hkey(root: PredefinedRoot) -> HKEY {
    match root {
        PredefinedRoot::ClassesRoot => HKEY_CLASSES_ROOT,
        PredefinedRoot::CurrentUser => HKEY_CURRENT_USER,
        PredefinedRoot::LocalMachine => HKEY_LOCAL_MACHINE,
        PredefinedRoot::Users => HKEY_USERS,
        PredefinedRoot::CurrentConfig => HKEY_CURRENT_CONFIG,
    }
}

fn open_key(parent: HKEY, subpath: &str, view: View, access: Access) -> Result<HKEY, Error> {
    let subpath = wide(subpath);
    let open = || {
        let mut out: HKEY = ptr::null_mut();
        // SAFETY: `subpath` is NUL-terminated; `out` is a valid out pointer.
        let status =
            unsafe { RegOpenKeyExW(parent, subpath.as_ptr(), 0, sam(view, access), &mut out) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(out)
    };
    let result = if access.0 & ACCESS_SYSTEM_SECURITY != 0 {
        dolang_winterop::with_security_privilege(open)
    } else {
        open()
    };
    result.map_err(|error| from_io("open key", error))
}

fn create_key(parent: HKEY, subpath: &str, view: View, access: Access) -> Result<HKEY, Error> {
    let subpath = wide(subpath);
    let create = || {
        let mut out: HKEY = ptr::null_mut();
        // SAFETY: `subpath` is NUL-terminated; `out` is a valid out pointer; no
        // class string or security attributes are needed.
        let status = unsafe {
            RegCreateKeyExW(
                parent,
                subpath.as_ptr(),
                0,
                ptr::null_mut(),
                REG_OPTION_NON_VOLATILE,
                sam(view, access),
                ptr::null(),
                &mut out,
                ptr::null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(out)
    };
    let result = if access.0 & ACCESS_SYSTEM_SECURITY != 0 {
        dolang_winterop::with_security_privilege(create)
    } else {
        create()
    };
    result.map_err(|error| from_io("create key", error))
}

fn delete_key(parent: HKEY, subpath: &str, view: View, all: bool) -> Result<(), Error> {
    if all {
        // RegDeleteTreeW has no WOW64-view parameter. Open the target in the
        // requested view, clear its contents through that handle, then delete
        // the now-empty target with RegDeleteKeyExW below.
        const DELETE_ACCESS: u32 = 0x0001_0000;
        const KEY_QUERY_VALUE: u32 = 0x0001;
        const KEY_SET_VALUE: u32 = 0x0002;
        const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
        let target = open_key(
            parent,
            subpath,
            view,
            Access(DELETE_ACCESS | KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_ENUMERATE_SUB_KEYS),
        )?;
        // SAFETY: `target` is a live key handle and a null subkey asks
        // RegDeleteTreeW to clear its values and descendants without deleting
        // the target key itself.
        let status = unsafe { RegDeleteTreeW(target, ptr::null()) };
        // SAFETY: `target` was returned by RegOpenKeyExW and has not otherwise
        // been closed.
        unsafe {
            windows_sys::Win32::System::Registry::RegCloseKey(target);
        }
        if status != ERROR_SUCCESS {
            return Err(from_win32("delete registry tree", status));
        }
    }
    let subpath = wide(subpath);
    // SAFETY: `subpath` is NUL-terminated.
    let status = unsafe { RegDeleteKeyExW(parent, subpath.as_ptr(), view_sam(view), 0) };
    if status != ERROR_SUCCESS {
        return Err(from_win32("delete key", status));
    }
    Ok(())
}

fn enum_subkey(handle: HKEY, index: u32) -> Result<Option<String>, Error> {
    let mut name = vec![0u16; 256]; // MAX_PATH-class limit for key names
    loop {
        let mut name_len = name.len() as u32;
        // SAFETY: `name` and `name_len` describe a live, correctly-sized buffer.
        let status = unsafe {
            RegEnumKeyExW(
                handle,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        match status {
            ERROR_NO_MORE_ITEMS => return Ok(None),
            ERROR_MORE_DATA => {
                name.resize(name.len() * 2, 0);
                continue;
            }
            ERROR_SUCCESS => {
                return Ok(Some(
                    OsString::from_wide(&name[..name_len as usize])
                        .to_string_lossy()
                        .into_owned(),
                ));
            }
            other => return Err(from_win32("enumerate subkey", other)),
        }
    }
}

/// Fetches every subkey name under `handle` in one pass, unlike calling
/// [`enum_subkey`] for every index.
fn enum_all_subkeys(handle: HKEY) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    let mut index = 0u32;
    while let Some(name) = enum_subkey(handle, index)? {
        names.push(name);
        index += 1;
    }
    Ok(names)
}

fn enum_value(handle: HKEY, index: u32) -> Result<Option<String>, Error> {
    let mut name = vec![0u16; 16_384]; // registry value names are limited to 16,383 characters
    loop {
        let mut name_len = name.len() as u32;
        let mut kind = 0u32;
        // SAFETY: `name`/`name_len` describe a live buffer; the data
        // arguments are null since we only want the name here.
        let status = unsafe {
            RegEnumValueW(
                handle,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                &mut kind,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        match status {
            ERROR_NO_MORE_ITEMS => return Ok(None),
            ERROR_MORE_DATA => {
                name.resize(name.len() * 2, 0);
                continue;
            }
            ERROR_SUCCESS => {
                return Ok(Some(
                    OsString::from_wide(&name[..name_len as usize])
                        .to_string_lossy()
                        .into_owned(),
                ));
            }
            other => return Err(from_win32("enumerate value", other)),
        }
    }
}

/// Fetches every value under `handle` (name, kind, and data) in one pass,
/// using `RegEnumValueW`'s own data-return parameters instead of a separate
/// `RegQueryValueExW` per value.
fn enum_all_values(handle: HKEY) -> Result<Vec<(String, Value)>, Error> {
    let mut name = vec![0u16; 16_384];
    let mut data = vec![0u8; 256];
    let mut values = Vec::new();
    let mut index = 0u32;
    loop {
        let mut name_len = name.len() as u32;
        let mut data_len = data.len() as u32;
        let mut kind = 0u32;
        // SAFETY: `name`/`name_len` and `data`/`data_len` describe live,
        // correctly-sized buffers.
        let status = unsafe {
            RegEnumValueW(
                handle,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                &mut kind,
                data.as_mut_ptr(),
                &mut data_len,
            )
        };
        match status {
            ERROR_NO_MORE_ITEMS => return Ok(values),
            ERROR_MORE_DATA => {
                // Either (or both) buffer may have been too small; grow both
                // to be safe and retry the same index.
                name.resize(name.len() * 2, 0);
                data.resize(data.len().max(data_len as usize) * 2, 0);
                continue;
            }
            ERROR_SUCCESS => {
                let value_name = OsString::from_wide(&name[..name_len as usize])
                    .to_string_lossy()
                    .into_owned();
                let value = Value::from_raw(kind, &data[..data_len as usize]);
                values.push((value_name, value));
                index += 1;
            }
            other => return Err(from_win32("enumerate all values", other)),
        }
    }
}

fn get_value(handle: HKEY, name: Option<&str>) -> Result<Option<(String, Value)>, Error> {
    let wide_name = wide(name.unwrap_or(""));
    let mut kind = 0u32;
    let mut data = vec![0u8; 256];
    loop {
        let mut data_len = data.len() as u32;
        // SAFETY: `wide_name` is NUL-terminated; `data`/`data_len` describe a
        // live buffer sized by `data_len`.
        let status = unsafe {
            RegQueryValueExW(
                handle,
                wide_name.as_ptr(),
                ptr::null_mut(),
                &mut kind,
                data.as_mut_ptr(),
                &mut data_len,
            )
        };
        match status {
            ERROR_FILE_NOT_FOUND => return Ok(None),
            ERROR_MORE_DATA => {
                data.resize(data_len as usize, 0);
                continue;
            }
            ERROR_SUCCESS => {
                data.truncate(data_len as usize);
                let value = Value::from_raw(kind, &data);
                return Ok(Some((name.unwrap_or("").to_string(), value)));
            }
            other => return Err(from_win32("get value", other)),
        }
    }
}

fn set_value(handle: HKEY, name: Option<&str>, value: &Value) -> Result<(), Error> {
    let wide_name = wide(name.unwrap_or(""));
    let (kind, data) = value.to_raw();
    // SAFETY: `wide_name` is NUL-terminated; `data` describes a live buffer
    // of length `data.len()`.
    let status = unsafe {
        RegSetValueExW(
            handle,
            wide_name.as_ptr(),
            0,
            kind,
            data.as_ptr(),
            data.len() as u32,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(from_win32("set value", status));
    }
    Ok(())
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

/// Fetches `handle`'s security descriptor via `RegGetKeySecurity`, which
/// (unlike the generic `GetSecurityInfo` the file backend uses) operates
/// directly on the `HKEY` and returns the same native self-relative byte
/// blob `SecDesc::from_bytes_with_mask` already parses — no owner/group/
/// dacl/sacl pointer decomposition needed.
fn sec_desc(handle: HKEY, mask: u32) -> Result<VfsSecDesc, Error> {
    let mask = mask & ALL_SECURITY_INFORMATION;
    let query_mask = if mask == 0 {
        OWNER_SECURITY_INFORMATION
    } else {
        mask
    };
    let mut bytes = vec![0u8; 256];
    loop {
        let mut len = bytes.len() as u32;
        // SAFETY: `bytes`/`len` describe a live, correctly-sized buffer.
        let status =
            unsafe { RegGetKeySecurity(handle, query_mask, bytes.as_mut_ptr().cast(), &mut len) };
        match status {
            ERROR_SUCCESS => {
                bytes.truncate(len as usize);
                break;
            }
            ERROR_INSUFFICIENT_BUFFER => bytes.resize(len as usize, 0),
            other => {
                return Err(from_win32("get key security", other));
            }
        }
    }
    VfsSecDesc::from_bytes_with_mask(&bytes, query_mask)
        .map_err(|error| Error::new(ErrorKind::Other, error.to_string()))
}

/// Sets `handle`'s security descriptor via `RegSetKeySecurity`, passing the
/// native self-relative byte blob `SecDesc::to_bytes` produces straight
/// through.
fn set_sec_desc(handle: HKEY, descriptor: &VfsSecDesc) -> Result<(), Error> {
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
        let status = unsafe { RegSetKeySecurity(handle, mask, bytes.as_ptr().cast_mut().cast()) };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    };
    let result = if mask & SACL_SECURITY_INFORMATION != 0 {
        dolang_winterop::with_security_privilege(set)
    } else {
        set()
    };
    result.map_err(|error| from_io("set key security", error))
}

fn delete_value(handle: HKEY, name: Option<&str>) -> Result<(), Error> {
    let wide_name = wide(name.unwrap_or(""));
    // SAFETY: `wide_name` is NUL-terminated.
    let status = unsafe { RegDeleteValueW(handle, wide_name.as_ptr()) };
    if status != ERROR_SUCCESS {
        return Err(from_win32("delete value", status));
    }
    Ok(())
}

/// Runs `f` while holding the key's cursor lock for the whole call, so
/// concurrent operations on the same key never interleave.
fn with_handle<R>(key: &Key, f: impl FnOnce(HKEY) -> R) -> R {
    let guard = key
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(*guard)
}

/// Wraps a freshly-opened `HKEY` into the appropriate [`KeyHandle`]: a
/// native out-of-band handle when the peer's transport supports it (no
/// registration — ownership transfers fully to the peer), otherwise a
/// registered [`dolang_vfs::ExtOpaque`].
fn key_response(ctx: &ExtContext<'_>, handle: HKEY) -> WinRegResponse {
    if ctx.native_capable() {
        WinRegResponse::Key(KeyHandle::Native(ExtOsHandle::new(hkey_to_owned(handle))))
    } else {
        WinRegResponse::Key(KeyHandle::Opaque(ctx.register(Key::new(handle))))
    }
}

pub(crate) async fn handle(
    ctx: &mut ExtContext<'_>,
    request: WinRegRequest,
) -> Result<WinRegResponse, Error> {
    match request {
        WinRegRequest::OpenRoot { root, view, access } => {
            let predefined = predefined_hkey(root);
            let handle = open_key(predefined, "", view, access)?;
            // `RegOpenKeyExW` on a predefined root with an empty subkey
            // hands back the same `HKEY_*` pseudo-handle constant rather
            // than a fresh kernel object — those constants aren't real NT
            // handles, so `DuplicateHandle` (the native-handle response
            // path) rejects them with `ERROR_INVALID_HANDLE`. Always use
            // the opaque path for a root that comes back this way.
            // Also always use an opaque handle if `ACCESS_SYSTEM_SECURITY`
            // was requested so a later SACL update remains on this backend,
            // whose token was used to open the handle and can enable the
            // privilege again for the update.
            if handle == predefined || access.0 & ACCESS_SYSTEM_SECURITY != 0 {
                Ok(WinRegResponse::Key(KeyHandle::Opaque(
                    ctx.register(Key::new(handle)),
                )))
            } else {
                Ok(key_response(ctx, handle))
            }
        }
        WinRegRequest::OpenKey {
            parent,
            subpath,
            view,
            access,
        } => {
            let guard = ctx.acquire::<Key>(parent).map_err(invalid_handle)?;
            let handle = with_handle(&guard, |h| open_key(h, &subpath, view, access))?;
            // Also always use an opaque handle if `ACCESS_SYSTEM_SECURITY`
            // was requested so a later SACL update remains on this backend,
            // whose token was used to open the handle and can enable the
            // privilege again for the update.
            if access.0 & ACCESS_SYSTEM_SECURITY != 0 {
                Ok(WinRegResponse::Key(KeyHandle::Opaque(
                    ctx.register(Key::new(handle)),
                )))
            } else {
                Ok(key_response(ctx, handle))
            }
        }
        WinRegRequest::CreateKey {
            parent,
            subpath,
            view,
            access,
        } => {
            let guard = ctx.acquire::<Key>(parent).map_err(invalid_handle)?;
            let handle = with_handle(&guard, |h| create_key(h, &subpath, view, access))?;
            if access.0 & ACCESS_SYSTEM_SECURITY != 0 {
                Ok(WinRegResponse::Key(KeyHandle::Opaque(
                    ctx.register(Key::new(handle)),
                )))
            } else {
                Ok(key_response(ctx, handle))
            }
        }
        WinRegRequest::AdoptNative { handle } => {
            let hkey = owned_to_hkey(handle.into_inner());
            Ok(WinRegResponse::Key(KeyHandle::Opaque(
                ctx.register(Key::new(hkey)),
            )))
        }
        WinRegRequest::CloseKey { key } => {
            ctx.unregister::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Closed)
        }
        WinRegRequest::DeleteKey {
            parent,
            subpath,
            view,
            all,
            ignore,
        } => {
            let guard = ctx.acquire::<Key>(parent).map_err(invalid_handle)?;
            let result = with_handle(&guard, |h| delete_key(h, &subpath, view, all));
            match result {
                Err(error) if ignore && error.kind() == ErrorKind::NotFound => {}
                result => result?,
            }
            Ok(WinRegResponse::Deleted)
        }
        WinRegRequest::EnumSubkey { key, index } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Name(with_handle(&guard, |h| {
                enum_subkey(h, index)
            })?))
        }
        WinRegRequest::EnumAllSubkeys { key } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Subkeys(with_handle(
                &guard,
                enum_all_subkeys,
            )?))
        }
        WinRegRequest::EnumValue { key, index } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Name(with_handle(&guard, |h| {
                enum_value(h, index)
            })?))
        }
        WinRegRequest::EnumAllValues { key } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Values(with_handle(
                &guard,
                enum_all_values,
            )?))
        }
        WinRegRequest::GetValue { key, name } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Value(with_handle(&guard, |h| {
                get_value(h, name.as_deref())
            })?))
        }
        WinRegRequest::SetValue { key, name, value } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            with_handle(&guard, |h| set_value(h, name.as_deref(), &value))?;
            Ok(WinRegResponse::Ack)
        }
        WinRegRequest::DeleteValue { key, name } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            with_handle(&guard, |h| delete_value(h, name.as_deref()))?;
            Ok(WinRegResponse::Ack)
        }
        WinRegRequest::GetSecDesc { key, mask } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::SecDesc(with_handle(&guard, |h| {
                sec_desc(h, mask)
            })?))
        }
        WinRegRequest::SetSecDesc {
            key,
            sec_desc: descriptor,
        } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            with_handle(&guard, |h| set_sec_desc(h, &descriptor))?;
            Ok(WinRegResponse::Ack)
        }
    }
}

fn invalid_handle(_: InvalidHandle) -> Error {
    Error::new(ErrorKind::InvalidInput, "invalid registry key handle")
}
