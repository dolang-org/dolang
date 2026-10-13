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

use dolang_vfs::error::{Error, ErrorKind};
use dolang_vfs::extension::{ExtContext, ExtGuard, ExtOsHandle, InvalidHandle};
use dolang_winterop::security::{AccessMask, SecDesc as VfsSecDesc, SecDescControl, SecInfo};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
        ERROR_NO_MORE_ITEMS, ERROR_SUCCESS,
    },
    Security::{
        PROTECTED_DACL_SECURITY_INFORMATION, PROTECTED_SACL_SECURITY_INFORMATION,
        UNPROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_SACL_SECURITY_INFORMATION,
    },
    System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_CONFIG, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        HKEY_USERS, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_LINK, REG_OPTION_CREATE_LINK,
        REG_OPTION_NON_VOLATILE, REG_OPTION_OPEN_LINK, RegCreateKeyExW, RegDeleteKeyExW,
        RegDeleteValueW, RegEnumKeyExW, RegEnumValueW, RegGetKeySecurity, RegOpenCurrentUser,
        RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW, RegSetKeySecurity, RegSetValueExW,
    },
    System::SystemServices::ACCESS_SYSTEM_SECURITY,
};

use crate::{
    key::Key,
    value::Value,
    wire::{Access, KeyHandle, LinkTarget, PredefinedRoot, View, WinRegRequest, WinRegResponse},
};

/// Converts a non-predefined `HKEY` into an `OwnedHandle` that closes it via
/// `CloseHandle` rather than `RegCloseKey`.
///
/// `windows-sys`'s `HKEY` is `*mut core::ffi::c_void`, the same
/// representation as `RawHandle`, so this is a pointer reinterpretation, not
/// a numeric cast.
unsafe fn hkey_to_owned(hkey: HKEY) -> OwnedHandle {
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

fn checked_wide(value: &str, what: &str) -> Result<Vec<u16>, Error> {
    if value.contains('\0') {
        Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{what} contains NUL"),
        ))
    } else {
        Ok(wide(value))
    }
}

fn from_win32(operation: &str, code: u32) -> Error {
    Error::from_raw_os_error_with_message(
        code as i32,
        format!("{operation}: registry error {code}"),
    )
}

fn sam(view: View, access: Access) -> u32 {
    let view = match view {
        View::Native => 0,
        View::Wow32 => KEY_WOW64_32KEY,
        View::Wow64 => KEY_WOW64_64KEY,
    };
    access.0.bits() | view
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

unsafe fn open_key(parent: HKEY, subpath: &str, view: View, access: Access) -> Result<HKEY, Error> {
    unsafe { open_key_options(parent, subpath, view, access, 0) }
}

unsafe fn open_key_options(
    parent: HKEY,
    subpath: &str,
    view: View,
    access: Access,
    options: u32,
) -> Result<HKEY, Error> {
    let subpath_str = subpath;
    let subpath = checked_wide(subpath_str, "registry subpath")?;
    let open = || {
        let mut out: HKEY = ptr::null_mut();
        // SAFETY: `subpath` is NUL-terminated; `out` is a valid out pointer.
        let status = unsafe {
            RegOpenKeyExW(
                parent,
                subpath.as_ptr(),
                options,
                sam(view, access),
                &mut out,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(out)
    };
    let result = if access.0.bits() & ACCESS_SYSTEM_SECURITY != 0 {
        dolang_winterop::security::with_security_privilege(open)
    } else {
        open()
    };
    result.map_err(|error| from_io("open key", error))
}

unsafe fn open_link(
    parent: HKEY,
    subpath: &str,
    view: View,
    access: Access,
) -> Result<HKEY, Error> {
    unsafe { open_key_options(parent, subpath, view, access, REG_OPTION_OPEN_LINK) }
}

unsafe fn create_key(
    parent: HKEY,
    subpath: &str,
    view: View,
    access: Access,
) -> Result<HKEY, Error> {
    let subpath = checked_wide(subpath, "registry subpath")?;
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
    let result = if access.0.bits() & ACCESS_SYSTEM_SECURITY != 0 {
        dolang_winterop::security::with_security_privilege(create)
    } else {
        create()
    };
    result.map_err(|error| from_io("create key", error))
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryKey(
        key_handle: *mut core::ffi::c_void,
        key_information_class: i32,
        key_information: *mut core::ffi::c_void,
        length: u32,
        result_length: *mut u32,
    ) -> i32;

    fn NtDeleteKey(key_handle: *mut core::ffi::c_void) -> i32;
}

/// Returns the object-manager name of a registry key.
///
/// # Safety
///
/// `handle` must be a live registry-key handle for the duration of the call.
unsafe fn native_key_name(handle: HKEY) -> Result<String, Error> {
    const KEY_NAME_INFORMATION: i32 = 3;
    const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;
    const STATUS_BUFFER_TOO_SMALL: i32 = 0xc000_0023u32 as i32;
    let mut bytes = vec![0u8; 256];
    loop {
        let mut needed = 0;
        // SAFETY: the handle is live and the output buffer and length pointer are valid.
        let status = unsafe {
            NtQueryKey(
                handle.cast(),
                KEY_NAME_INFORMATION,
                bytes.as_mut_ptr().cast(),
                bytes.len() as u32,
                &mut needed,
            )
        };
        if status == STATUS_BUFFER_OVERFLOW || status == STATUS_BUFFER_TOO_SMALL {
            bytes.resize(needed as usize, 0);
            continue;
        }
        if status < 0 {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "query native registry name: NTSTATUS 0x{:08x}",
                    status as u32
                ),
            ));
        }
        if needed < 4 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "malformed native registry name",
            ));
        }
        let name_len = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
        let data = bytes
            .get(4..4 + name_len)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "malformed native registry name"))?;
        if data.len() % 2 != 0 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "malformed native registry name",
            ));
        }
        let units: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();
        return String::from_utf16(&units).map_err(|_| {
            Error::new(
                ErrorKind::InvalidData,
                "native registry name is not valid UTF-16",
            )
        });
    }
}

/// # Safety
///
/// `handle` must be a live, owned registry-key handle.
unsafe fn close_key(handle: HKEY) {
    unsafe { windows_sys::Win32::System::Registry::RegCloseKey(handle) };
}

fn resolve_native_target(root: PredefinedRoot, subpath: &str, view: View) -> Result<String, Error> {
    if subpath.contains('\0') {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "target subpath contains NUL",
        ));
    }
    let parts: Vec<&str> = subpath
        .split('\\')
        .filter(|part| !part.is_empty())
        .collect();
    for count in (1..=parts.len()).rev() {
        let prefix = parts[..count].join("\\");
        // Opening with REG_OPTION_OPEN_LINK is necessary for a link object,
        // but real Windows returns ERROR_FILE_NOT_FOUND for an ordinary key.
        // Fall back to a normal open so either kind can be the existing
        // ancestor. Both calls use a nonempty subpath, so a successful result
        // is a real key handle rather than a predefined pseudo-handle.
        // SAFETY: `predefined_hkey` returns a valid predefined registry handle.
        let opened = unsafe { open_link(predefined_hkey(root), &prefix, view, Access::READ) };
        let opened = match opened {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // SAFETY: same parent-handle argument as above.
                unsafe { open_key(predefined_hkey(root), &prefix, view, Access::READ) }
            }
            result => result,
        };
        match opened {
            Ok(handle) => {
                // SAFETY: `handle` was just returned by `open_link` and is
                // kept live until it is closed immediately below.
                let native = unsafe { native_key_name(handle) };
                // SAFETY: ownership of the freshly-opened handle is local.
                unsafe { close_key(handle) };
                let mut native = native?;
                if count < parts.len() {
                    native.push('\\');
                    native.push_str(&parts[count..].join("\\"));
                }
                return Ok(native);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let mut native = match root {
        PredefinedRoot::LocalMachine => r"\Registry\Machine".to_string(),
        PredefinedRoot::Users => r"\Registry\User".to_string(),
        PredefinedRoot::CurrentUser => {
            let mut handle = ptr::null_mut();
            // Unlike RegOpenKeyExW(HKEY_CURRENT_USER, ""), this returns a
            // closable real handle suitable for NtQueryKey.
            // SAFETY: `handle` is a valid out pointer.
            let status = unsafe { RegOpenCurrentUser(sam(view, Access::READ), &mut handle) };
            if status != ERROR_SUCCESS {
                return Err(from_win32("open current user", status));
            }
            // SAFETY: `handle` was returned by RegOpenCurrentUser and remains
            // live until the close immediately below.
            let native = unsafe { native_key_name(handle) };
            // SAFETY: ownership of the freshly-opened handle is local.
            unsafe { close_key(handle) };
            native?
        }
        PredefinedRoot::CurrentConfig => resolve_native_target(
            PredefinedRoot::LocalMachine,
            r"System\CurrentControlSet\Hardware Profiles\Current",
            view,
        )?,
        PredefinedRoot::ClassesRoot => {
            resolve_native_target(PredefinedRoot::LocalMachine, r"Software\Classes", view)?
        }
    };
    if !parts.is_empty() {
        native.push('\\');
        native.push_str(&parts.join("\\"));
    }
    Ok(native)
}

unsafe fn create_link(
    parent: HKEY,
    target_root: PredefinedRoot,
    target_subpath: &str,
    link_subpath: &str,
    view: View,
) -> Result<(), Error> {
    const KEY_SET_VALUE: u32 = 0x0002;
    let native = resolve_native_target(target_root, target_subpath, view)?;
    let link_path = checked_wide(link_subpath, "link subpath")?;
    let mut handle = ptr::null_mut();
    let mut disposition = 0;
    // SAFETY: all pointers reference live values and link_path is NUL terminated.
    let status = unsafe {
        RegCreateKeyExW(
            parent,
            link_path.as_ptr(),
            0,
            ptr::null_mut(),
            REG_OPTION_CREATE_LINK,
            view_sam(view) | KEY_SET_VALUE,
            ptr::null(),
            &mut handle,
            &mut disposition,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(from_win32("create registry link", status));
    }
    if disposition != 1 {
        // SAFETY: ownership of the freshly-created handle is local.
        unsafe { close_key(handle) };
        return Err(Error::new(
            ErrorKind::AlreadyExists,
            "registry link destination already exists",
        ));
    }
    let name = wide("SymbolicLinkValue");
    let units: Vec<u16> = native.encode_utf16().collect();
    let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    // SAFETY: handle is live and both buffers remain live for the call.
    let set = unsafe {
        RegSetValueExW(
            handle,
            name.as_ptr(),
            0,
            REG_LINK,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    // SAFETY: ownership of the freshly-created handle is local.
    unsafe { close_key(handle) };
    if set != ERROR_SUCCESS {
        // Best-effort rollback of the newly-created, still-empty link.
        unsafe { RegDeleteKeyExW(parent, link_path.as_ptr(), view_sam(view), 0) };
        return Err(from_win32("set registry link target", set));
    }
    Ok(())
}

fn native_projection(native: String, view: View) -> Result<LinkTarget, Error> {
    fn suffix(native: &str, prefix: &str) -> Option<String> {
        if native.eq_ignore_ascii_case(prefix) {
            return Some(String::new());
        }
        native
            .get(prefix.len()..)
            .filter(|rest| {
                rest.starts_with('\\') && native[..prefix.len()].eq_ignore_ascii_case(prefix)
            })
            .map(|rest| rest[1..].to_string())
    }
    for root in [
        PredefinedRoot::CurrentUser,
        PredefinedRoot::Users,
        PredefinedRoot::LocalMachine,
    ] {
        let prefix = resolve_native_target(root, "", view)?;
        if let Some(subpath) = suffix(&native, &prefix) {
            return Ok(LinkTarget {
                native,
                root: Some(root),
                subpath: Some(subpath),
            });
        }
    }
    Ok(LinkTarget {
        native,
        root: None,
        subpath: None,
    })
}

unsafe fn read_link(parent: HKEY, subpath: &str, view: View) -> Result<LinkTarget, Error> {
    let handle = unsafe { open_link(parent, subpath, view, Access::QUERY_VALUE)? };
    let result = unsafe { get_value(handle, Some("SymbolicLinkValue")) };
    // SAFETY: ownership of the freshly-opened handle is local.
    unsafe { close_key(handle) };
    let (_, value) =
        result?.ok_or_else(|| Error::new(ErrorKind::InvalidInput, "registry key is not a link"))?;
    let Value::Other { kind, data } = value else {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "registry key is not a link",
        ));
    };
    if kind != REG_LINK {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "registry key is not a link",
        ));
    }
    if data.len() % 2 != 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "registry link target has an odd byte length",
        ));
    }
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    let native = String::from_utf16(&units).map_err(|_| {
        Error::new(
            ErrorKind::InvalidData,
            "registry link target is not valid UTF-16",
        )
    })?;
    if native.contains('\0') {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "registry link target contains NUL",
        ));
    }
    native_projection(native, view)
}

unsafe fn delete_link_if_link(parent: HKEY, subpath: &str, view: View) -> Result<bool, Error> {
    const DELETE_ACCESS: u32 = 0x0001_0000;
    let handle = unsafe {
        open_link(
            parent,
            subpath,
            view,
            Access(AccessMask::from_bits_retain(
                Access::QUERY_VALUE.0.bits() | DELETE_ACCESS,
            )),
        )
    };
    let handle = match handle {
        Ok(handle) => handle,
        // Real Windows reports an ordinary key as not found when it is
        // opened with REG_OPTION_OPEN_LINK. The normal deletion path below
        // distinguishes an ordinary key from a genuinely missing path.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let result = unsafe { get_value(handle, Some("SymbolicLinkValue")) };
    let deleted = match result {
        Ok(Some((_, Value::Other { kind: REG_LINK, .. }))) => {
            // RegDeleteKeyExW resolves the pathname on some Windows versions
            // and can consequently delete the target. NtDeleteKey acts on
            // this REG_OPTION_OPEN_LINK handle, so it unambiguously deletes
            // the link object itself.
            // SAFETY: `handle` is live and was opened with DELETE access.
            let status = unsafe { NtDeleteKey(handle.cast()) };
            if status < 0 {
                Err(Error::new(
                    ErrorKind::Other,
                    format!("delete registry link: NTSTATUS 0x{:08x}", status as u32),
                ))
            } else {
                Ok(true)
            }
        }
        Ok(_) => Ok(false),
        Err(error) => Err(error),
    };
    // SAFETY: ownership of the freshly-opened handle is local.
    unsafe { close_key(handle) };
    deleted
}

/// Clears a key without ever following a registry symbolic link.
///
/// # Safety
///
/// `handle` must be a live registry-key handle with enumerate, query, set,
/// and delete access for the duration of the call.
unsafe fn clear_key(handle: HKEY) -> Result<(), Error> {
    // Always remove index zero: deleting it shifts the next child into the
    // same position. `delete_key` opens every child with REG_OPTION_OPEN_LINK
    // first and deletes link objects by handle.
    while let Some(name) = unsafe { enum_subkey(handle, 0)? } {
        unsafe { delete_key(handle, &name, View::Native, true)? };
    }
    while let Some(name) = unsafe { enum_value(handle, 0)? } {
        unsafe { delete_value(handle, Some(&name))? };
    }
    Ok(())
}

unsafe fn delete_key(parent: HKEY, subpath: &str, view: View, all: bool) -> Result<(), Error> {
    // Check this for every removal, not only recursive removal: pathname-based
    // deletion must never be given a link source.
    if unsafe { delete_link_if_link(parent, subpath, view)? } {
        return Ok(());
    }
    if all {
        // Open the ordinary key in the requested view, clear it one child at
        // a time without following links, then delete the empty key below.
        const DELETE_ACCESS: u32 = 0x0001_0000;
        const KEY_QUERY_VALUE: u32 = 0x0001;
        const KEY_SET_VALUE: u32 = 0x0002;
        const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
        let target = unsafe {
            open_key(
                parent,
                subpath,
                view,
                Access(AccessMask::from_bits_retain(
                    DELETE_ACCESS | KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_ENUMERATE_SUB_KEYS,
                )),
            )?
        };
        // SAFETY: `target` is live with all access required by `clear_key`.
        let result = unsafe { clear_key(target) };
        // SAFETY: `target` was returned by RegOpenKeyExW and has not otherwise
        // been closed.
        unsafe { close_key(target) };
        result?;
    }
    let subpath_wide = checked_wide(subpath, "registry subpath")?;
    // SAFETY: `subpath` is NUL-terminated.
    let status = unsafe { RegDeleteKeyExW(parent, subpath_wide.as_ptr(), view_sam(view), 0) };
    if status != ERROR_SUCCESS {
        if !all && status == ERROR_ACCESS_DENIED {
            const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
            let target = unsafe {
                open_key(
                    parent,
                    subpath,
                    view,
                    Access(AccessMask::from_bits_retain(KEY_ENUMERATE_SUB_KEYS)),
                )
            };
            if let Ok(target) = target {
                let has_children =
                    unsafe { enum_subkey(target, 0) }.is_ok_and(|name| name.is_some());
                unsafe { close_key(target) };
                if has_children {
                    return Err(Error::new(
                        ErrorKind::DirectoryNotEmpty,
                        "delete key: registry key has subkeys",
                    ));
                }
            }
        }
        return Err(from_win32("delete key", status));
    }
    Ok(())
}

unsafe fn enum_subkey(handle: HKEY, index: u32) -> Result<Option<String>, Error> {
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
unsafe fn enum_subkeys_page(
    handle: HKEY,
    mut index: u32,
    count: u32,
) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    while names.len() < count as usize {
        let Some(name) = (unsafe { enum_subkey(handle, index)? }) else {
            break;
        };
        names.push(name);
        index += 1;
    }
    Ok(names)
}

unsafe fn enum_value(handle: HKEY, index: u32) -> Result<Option<String>, Error> {
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
unsafe fn enum_values_page(
    handle: HKEY,
    mut index: u32,
    count: u32,
) -> Result<Vec<(String, Value)>, Error> {
    let mut name = vec![0u16; 16_384];
    let mut data = vec![0u8; 256];
    let mut values = Vec::new();
    loop {
        if values.len() == count as usize {
            return Ok(values);
        }
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

unsafe fn query_counts(handle: HKEY) -> Result<(u32, u32), Error> {
    let mut subkeys = 0;
    let mut values = 0;
    // SAFETY: `handle` is live; all unused output pointers may be null.
    let status = unsafe {
        RegQueryInfoKeyW(
            handle,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut subkeys,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut values,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok((subkeys, values))
    } else {
        Err(from_win32("query key info", status))
    }
}

unsafe fn get_value(handle: HKEY, name: Option<&str>) -> Result<Option<(String, Value)>, Error> {
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

unsafe fn set_value(handle: HKEY, name: Option<&str>, value: &Value) -> Result<(), Error> {
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
            if data.is_empty() {
                ptr::null()
            } else {
                data.as_ptr()
            },
            data.len() as u32,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(from_win32("set value", status));
    }
    Ok(())
}

/// Converts an `io::Error` carrying a raw Win32 error code (as produced by
/// [`sec_desc`]/[`update_sec_desc`] below) back into this crate's [`Error`]
/// type, using the same code-to-`ErrorKind` mapping as every other
/// operation in this file.
fn from_io(operation: &str, err: io::Error) -> Error {
    match err.raw_os_error() {
        Some(code) => from_win32(operation, code as u32),
        None => Error::new(err.kind().into(), format!("{operation}: {err}")),
    }
}

/// Fetches `handle`'s security descriptor via `RegGetKeySecurity`, which
/// (unlike the generic `GetSecurityInfo` the file backend uses) operates
/// directly on the `HKEY` and returns the same native self-relative byte
/// blob `SecDesc::from_bytes_with_mask` already parses — no owner/group/
/// dacl/sacl pointer decomposition needed.
unsafe fn sec_desc(handle: HKEY, mask: SecInfo) -> Result<VfsSecDesc, Error> {
    let mask = mask & SecInfo::ALL;
    let query_mask = if mask.is_empty() {
        SecInfo::OWNER
    } else {
        mask
    };
    let mut bytes = vec![0u8; 256];
    loop {
        let mut len = bytes.len() as u32;
        // SAFETY: `bytes`/`len` describe a live, correctly-sized buffer.
        let status = unsafe {
            RegGetKeySecurity(
                handle,
                query_mask.bits(),
                bytes.as_mut_ptr().cast(),
                &mut len,
            )
        };
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
unsafe fn update_sec_desc(handle: HKEY, descriptor: &VfsSecDesc) -> Result<(), Error> {
    let mut mask = (descriptor.mask() & SecInfo::ALL).bits();
    if mask == 0 {
        return Ok(());
    }
    if mask & SecInfo::DACL.bits() != 0 {
        mask |= if descriptor
            .control()
            .contains(SecDescControl::DACL_PROTECTED)
        {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
    }
    if mask & SecInfo::SACL.bits() != 0 {
        mask |= if descriptor
            .control()
            .contains(SecDescControl::SACL_PROTECTED)
        {
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
    let result = if mask & SecInfo::SACL.bits() != 0 {
        dolang_winterop::security::with_security_privilege(set)
    } else {
        set()
    };
    result.map_err(|error| from_io("set key security", error))
}

unsafe fn delete_value(handle: HKEY, name: Option<&str>) -> Result<(), Error> {
    let wide_name = wide(name.unwrap_or(""));
    // SAFETY: `wide_name` is NUL-terminated.
    let status = unsafe { RegDeleteValueW(handle, wide_name.as_ptr()) };
    if status != ERROR_SUCCESS {
        return Err(from_win32("delete value", status));
    }
    Ok(())
}

/// Wraps a value so it can cross a [`tokio::task::spawn_blocking`] boundary
/// even when it isn't `Send` — Win32 handles (`HKEY` and friends) are opaque
/// pointer types Rust doesn't know are safe to move between threads, even
/// though Microsoft documents them as such.
struct SendValue<T>(T);
// SAFETY: only used to ferry Win32 handle values (and results built from
// them) across a `spawn_blocking` call; those are documented as usable from
// any thread.
unsafe impl<T> Send for SendValue<T> {}

impl<T> SendValue<T> {
    /// Consumes the wrapper, forcing a whole-value closure capture rather
    /// than a disjoint capture of the (non-`Send`) inner field — see the
    /// call site in `OpenRoot` below.
    fn into_inner(self) -> T {
        self.0
    }
}

/// Runs `f` on the blocking thread pool, since every registry API this
/// backend calls is a synchronous Win32 call with no async equivalent —
/// running it inline on the async task would block the executor thread.
async fn blocking<T: 'static>(
    f: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    match tokio::task::spawn_blocking(move || SendValue(f())).await {
        Ok(SendValue(result)) => result,
        Err(_) => Err(Error::new(
            ErrorKind::Other,
            "registry operation task panicked",
        )),
    }
}

/// Runs `f` on the blocking thread pool while holding the key's cursor lock
/// for the whole call, so concurrent operations on the same key never
/// interleave.
async fn with_handle<T: 'static>(
    key: ExtGuard<Key>,
    f: impl FnOnce(HKEY) -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    blocking(move || {
        let guard = key.lock();
        f(*guard)
    })
    .await
}

/// Wraps a freshly-opened `HKEY` into the appropriate [`KeyHandle`]: a
/// native out-of-band handle when the peer's transport supports it (no
/// registration — ownership transfers fully to the peer), otherwise a
/// registered [`dolang_vfs::extension::ExtOpaque`].
unsafe fn key_response(ctx: &ExtContext<'_>, handle: HKEY) -> WinRegResponse {
    if ctx.native_capable() {
        WinRegResponse::Key(KeyHandle::Native(ExtOsHandle::new(unsafe {
            hkey_to_owned(handle)
        })))
    } else {
        WinRegResponse::Key(KeyHandle::Opaque(ctx.register(unsafe { Key::new(handle) })))
    }
}

pub(crate) async fn handle(
    ctx: &mut ExtContext<'_>,
    request: WinRegRequest,
) -> Result<WinRegResponse, Error> {
    match request {
        WinRegRequest::OpenRoot { root, view, access } => {
            // The raw `HKEY` predefined-root constant isn't `Send`, so it
            // can't be held live across the `.await` below (which would
            // make this whole `handle` future non-`Send`). Recompute it
            // afterward instead — `predefined_hkey` is a pure lookup.
            let handle = {
                let predefined = SendValue(predefined_hkey(root));
                blocking(move || {
                    // SAFETY: `predefined` contains a predefined registry handle.
                    unsafe { open_key(predefined.into_inner(), "", view, access) }
                })
                .await?
            };
            let predefined = predefined_hkey(root);
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
            if handle == predefined || access.0.bits() & ACCESS_SYSTEM_SECURITY != 0 {
                Ok(WinRegResponse::Key(KeyHandle::Opaque(
                    ctx.register(unsafe { Key::new(handle) }),
                )))
            } else {
                // SAFETY: ownership of the freshly-opened handle transfers to the response.
                Ok(unsafe { key_response(ctx, handle) })
            }
        }
        WinRegRequest::OpenKey {
            parent,
            subpath,
            view,
            access,
        } => {
            let guard = ctx.acquire::<Key>(parent).map_err(invalid_handle)?;
            let handle = with_handle(guard, move |h| {
                // SAFETY: `with_handle` keeps the acquired key live and locked.
                unsafe { open_key(h, &subpath, view, access) }
            })
            .await?;
            // Also always use an opaque handle if `ACCESS_SYSTEM_SECURITY`
            // was requested so a later SACL update remains on this backend,
            // whose token was used to open the handle and can enable the
            // privilege again for the update.
            if access.0.bits() & ACCESS_SYSTEM_SECURITY != 0 {
                Ok(WinRegResponse::Key(KeyHandle::Opaque(
                    ctx.register(unsafe { Key::new(handle) }),
                )))
            } else {
                // SAFETY: ownership of the freshly-opened handle transfers to the response.
                Ok(unsafe { key_response(ctx, handle) })
            }
        }
        WinRegRequest::OpenLink {
            parent,
            subpath,
            view,
            access,
        } => {
            let guard = ctx.acquire::<Key>(parent).map_err(invalid_handle)?;
            let handle = with_handle(guard, move |h| {
                // SAFETY: `with_handle` keeps the acquired key live and locked.
                unsafe { open_link(h, &subpath, view, access) }
            })
            .await?;
            // SAFETY: ownership of the freshly-opened handle transfers to the response.
            Ok(unsafe { key_response(ctx, handle) })
        }
        WinRegRequest::CreateKey {
            parent,
            subpath,
            view,
            access,
        } => {
            let guard = ctx.acquire::<Key>(parent).map_err(invalid_handle)?;
            let handle = with_handle(guard, move |h| {
                // SAFETY: `with_handle` keeps the acquired key live and locked.
                unsafe { create_key(h, &subpath, view, access) }
            })
            .await?;
            if access.0.bits() & ACCESS_SYSTEM_SECURITY != 0 {
                Ok(WinRegResponse::Key(KeyHandle::Opaque(
                    ctx.register(unsafe { Key::new(handle) }),
                )))
            } else {
                // SAFETY: ownership of the freshly-created handle transfers to the response.
                Ok(unsafe { key_response(ctx, handle) })
            }
        }
        WinRegRequest::CreateLink {
            parent,
            target_root,
            target_subpath,
            link_subpath,
            view,
        } => {
            let guard = ctx.acquire::<Key>(parent).map_err(invalid_handle)?;
            with_handle(guard, move |h| {
                // SAFETY: `with_handle` keeps the acquired key live and locked.
                unsafe { create_link(h, target_root, &target_subpath, &link_subpath, view) }
            })
            .await?;
            Ok(WinRegResponse::Ack)
        }
        WinRegRequest::ReadLink {
            parent,
            subpath,
            view,
        } => {
            let guard = ctx.acquire::<Key>(parent).map_err(invalid_handle)?;
            Ok(WinRegResponse::LinkTarget(
                with_handle(guard, move |h| {
                    // SAFETY: `with_handle` keeps the acquired key live and locked.
                    unsafe { read_link(h, &subpath, view) }
                })
                .await?,
            ))
        }
        WinRegRequest::AdoptNative { handle } => {
            let hkey = owned_to_hkey(handle.into_inner());
            Ok(WinRegResponse::Key(KeyHandle::Opaque(
                // SAFETY: `hkey` retains the ownership carried by `OwnedHandle`.
                ctx.register(unsafe { Key::new(hkey) }),
            )))
        }
        WinRegRequest::CloseKey { key } => {
            // Unlike `dolang-vfs-winscm`'s SC handles, `RegCloseKey` is
            // local object-manager bookkeeping with no RPC involved, so
            // there's no need to defer it to the blocking pool here.
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
            let result = with_handle(guard, move |h| {
                // SAFETY: `with_handle` keeps the acquired key live and locked.
                unsafe { delete_key(h, &subpath, view, all) }
            })
            .await;
            match result {
                Err(error) if ignore && error.kind() == ErrorKind::NotFound => {}
                result => result?,
            }
            Ok(WinRegResponse::Deleted)
        }
        WinRegRequest::EnumSubkey { key, index } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Name(
                with_handle(guard, move |h| unsafe { enum_subkey(h, index) }).await?,
            ))
        }
        WinRegRequest::OpenSubkeys { key } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::EnumerationLen(
                with_handle(guard, |h| Ok(unsafe { query_counts(h)? }.0)).await?,
            ))
        }
        WinRegRequest::EnumSubkeysPage { key, index, count } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::SubkeysPage(
                with_handle(guard, move |h| unsafe {
                    enum_subkeys_page(h, index, count)
                })
                .await?,
            ))
        }
        WinRegRequest::EnumValue { key, index } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Name(
                with_handle(guard, move |h| unsafe { enum_value(h, index) }).await?,
            ))
        }
        WinRegRequest::OpenValues { key } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::EnumerationLen(
                with_handle(guard, |h| Ok(unsafe { query_counts(h)? }.1)).await?,
            ))
        }
        WinRegRequest::EnumValuesPage { key, index, count } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::ValuesPage(
                with_handle(guard, move |h| unsafe { enum_values_page(h, index, count) }).await?,
            ))
        }
        WinRegRequest::GetValue { key, name } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::Value(
                with_handle(guard, move |h| unsafe { get_value(h, name.as_deref()) }).await?,
            ))
        }
        WinRegRequest::SetValue { key, name, value } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            with_handle(guard, move |h| unsafe {
                set_value(h, name.as_deref(), &value)
            })
            .await?;
            Ok(WinRegResponse::Ack)
        }
        WinRegRequest::DeleteValue { key, name } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            with_handle(guard, move |h| unsafe { delete_value(h, name.as_deref()) }).await?;
            Ok(WinRegResponse::Ack)
        }
        WinRegRequest::GetSecDesc { key, mask } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            Ok(WinRegResponse::SecDesc(
                with_handle(guard, move |h| unsafe { sec_desc(h, mask) }).await?,
            ))
        }
        WinRegRequest::UpdateSecDesc {
            key,
            sec_desc: descriptor,
        } => {
            let guard = ctx.acquire::<Key>(key).map_err(invalid_handle)?;
            with_handle(guard, move |h| unsafe { update_sec_desc(h, &descriptor) }).await?;
            Ok(WinRegResponse::Ack)
        }
    }
}

fn invalid_handle(_: InvalidHandle) -> Error {
    Error::new(ErrorKind::InvalidInput, "invalid registry key handle")
}
