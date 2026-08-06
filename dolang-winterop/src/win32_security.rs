//! Windows security-descriptor primitives shared across VFS extensions.
//!
//! Only [`with_security_privilege`] lives here — everything else needed to
//! get/set a security descriptor (native self-relative byte-form
//! conversion via [`crate::SecDesc::from_bytes_with_mask`]/
//! [`crate::SecDesc::to_bytes`], and the actual Win32 API call) is either
//! already public or specific enough to each object type (file handles vs.
//! registry keys use different APIs entirely) that it doesn't belong here.

use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
};

use windows_sys::Win32::{
    Foundation::{ERROR_NOT_ALL_ASSIGNED, GetLastError, SetLastError},
    Security::{
        AdjustTokenPrivileges, DuplicateTokenEx, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
        RevertToSelf, SE_PRIVILEGE_ENABLED, SE_SECURITY_NAME, SecurityImpersonation,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_PRIVILEGES, TOKEN_QUERY,
        TokenImpersonation,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken, SetThreadToken},
};

/// Runs `f` with `SeSecurityPrivilege` enabled on the current thread,
/// reverting to the process token afterward.
///
/// Callers decide whether the operation requires the privilege. In particular,
/// it is required while opening a handle for `ACCESS_SYSTEM_SECURITY` and while
/// setting a SACL, but not while querying a SACL through a handle that already
/// has `ACCESS_SYSTEM_SECURITY` access.
pub fn with_security_privilege<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    struct RevertGuard;
    impl Drop for RevertGuard {
        fn drop(&mut self) {
            unsafe {
                RevertToSelf();
            }
        }
    }

    let mut process_token = ptr::null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut process_token,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let process_token = unsafe { OwnedHandle::from_raw_handle(process_token) };

    let mut token = ptr::null_mut();
    if unsafe {
        DuplicateTokenEx(
            process_token.as_raw_handle(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY | TOKEN_IMPERSONATE,
            ptr::null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut token,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };

    let mut luid = Default::default();
    if unsafe { LookupPrivilegeValueW(ptr::null(), SE_SECURITY_NAME, &mut luid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    unsafe { SetLastError(0) };
    if unsafe {
        AdjustTokenPrivileges(
            token.as_raw_handle(),
            0,
            &privileges,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SeSecurityPrivilege is not available",
        ));
    }
    if unsafe { SetThreadToken(ptr::null(), token.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _guard = RevertGuard;
    f()
}
