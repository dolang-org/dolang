//! Login environment import for Windows.
//!
//! The OpenSSH server that ships with Windows never builds a user environment
//! block: `do_exec_windows` in Microsoft's fork copies only `PATH` out of the
//! registry (`HKLM` system path followed by `HKCU` user path) and hands the
//! child whatever else the sshd service process had. Since that service runs as
//! LocalSystem, a remote command sees the *service account's* `TEMP`,
//! `USERPROFILE`, `APPDATA` and so on, and never sees user variables such as
//! `CARGO_HOME` at all.
//!
//! [`import`] reconstructs what a logged-in session would have:
//!
//! 1. `USERPROFILE`, `APPDATA` and `LOCALAPPDATA` are resolved from the known
//!    folders of the process token, which is the user's. These are not registry
//!    variables — a normal logon derives them the same way — and they must be
//!    correct before anything else, because user variables are routinely defined
//!    in terms of them.
//! 2. The machine (`HKLM`) and user (`HKCU`) environment keys are applied in
//!    that order, so user values win. `REG_EXPAND_SZ` values are expanded after
//!    step 1, so `%USERPROFILE%\...` resolves against the real profile rather
//!    than `C:\Windows\system32\config\systemprofile`.
//!
//! `PATH` is deliberately skipped: sshd already composed it, and it may contain
//! entries from elsewhere that a recomposition here would drop.

use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr,
};

use windows_sys::Win32::{
    Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS},
    System::{
        Environment::ExpandEnvironmentStringsW,
        Registry::{
            HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ,
            RegCloseKey, RegEnumValueW, RegOpenKeyExW,
        },
    },
    UI::Shell::{FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData},
};

use crate::direct::Direct;

/// Machine-wide environment key.
const MACHINE_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

/// Per-user environment key, relative to `HKEY_CURRENT_USER`.
const USER_KEY: &str = "Environment";

/// Import the user environment that a logon would have produced.
///
/// `shell` is accepted and ignored: it names a Unix login shell, and a client
/// launching the helper over SSH does not know which operating system will
/// answer.
///
/// # Safety
///
/// Mutates the process environment, so the caller must be single-threaded.
pub(crate) unsafe fn import(_shell: Option<&OsStr>) -> io::Result<()> {
    // SAFETY: single-threaded, before tokio (see caller).
    unsafe {
        for (name, folder) in [
            ("USERPROFILE", &FOLDERID_Profile),
            ("APPDATA", &FOLDERID_RoamingAppData),
            ("LOCALAPPDATA", &FOLDERID_LocalAppData),
        ] {
            // A missing known folder is not fatal: the stale value is no worse
            // than what we started with.
            if let Ok(path) = Direct::known_folder(folder) {
                std::env::set_var(name, path);
            }
        }

        for (root, key) in [
            (HKEY_LOCAL_MACHINE, MACHINE_KEY),
            (HKEY_CURRENT_USER, USER_KEY),
        ] {
            for (name, value) in read_key(root, key)? {
                // sshd already composed PATH from both keys, and may have had
                // help; recomposing it here would discard that.
                if name.eq_ignore_ascii_case("PATH") {
                    continue;
                }
                std::env::set_var(&name, &value);
            }
        }
    }

    Ok(())
}

/// Read every string value of a registry key, expanding `REG_EXPAND_SZ`.
///
/// A key that does not exist yields no values rather than an error: the user
/// environment key in particular is absent on accounts that have never had one.
fn read_key(root: HKEY, path: &str) -> io::Result<Vec<(OsString, OsString)>> {
    let wide_path = wide(OsStr::new(path));

    let mut key: HKEY = ptr::null_mut();
    // SAFETY: wide_path is NUL terminated and key is a valid out pointer.
    let status = unsafe { RegOpenKeyExW(root, wide_path.as_ptr(), 0, KEY_READ, &mut key) };
    if status != ERROR_SUCCESS {
        return Ok(Vec::new());
    }
    let key = KeyHandle(key);

    let mut values = Vec::new();
    // Names are limited to 32_767 characters; data is not, so grow on demand.
    let mut name = vec![0u16; 32_768];
    let mut data = vec![0u16; 1024];
    let mut index = 0u32;

    loop {
        let mut name_len = name.len() as u32;
        let mut data_len = (data.len() * size_of::<u16>()) as u32;
        let mut kind = 0u32;

        // SAFETY: all buffers and lengths describe live allocations.
        let status = unsafe {
            RegEnumValueW(
                key.0,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                &mut kind,
                data.as_mut_ptr().cast(),
                &mut data_len,
            )
        };

        match status {
            ERROR_NO_MORE_ITEMS => break,
            // Retry the same index with a buffer large enough for the value.
            ERROR_MORE_DATA => {
                data.resize(data_len as usize / size_of::<u16>() + 1, 0);
                continue;
            }
            ERROR_SUCCESS => {}
            other => return Err(io::Error::from_raw_os_error(other as i32)),
        }

        index += 1;

        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            continue;
        }

        let name = OsString::from_wide(&name[..name_len as usize]);
        let value = trim_nul(&data[..data_len as usize / size_of::<u16>()]);
        let value = if kind == REG_EXPAND_SZ {
            expand(value)?
        } else {
            OsString::from_wide(value)
        };

        if !name.is_empty() {
            values.push((name, value));
        }
    }

    Ok(values)
}

/// Expand `%NAME%` references against the current process environment.
fn expand(value: &[u16]) -> io::Result<OsString> {
    let value = wide(&OsString::from_wide(value));

    // SAFETY: value is NUL terminated; a null destination asks for the length.
    let len = unsafe { ExpandEnvironmentStringsW(value.as_ptr(), ptr::null_mut(), 0) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buffer = vec![0u16; len as usize];
    // SAFETY: buffer holds the length reported above, including the terminator.
    let written = unsafe { ExpandEnvironmentStringsW(value.as_ptr(), buffer.as_mut_ptr(), len) };
    if written == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(OsString::from_wide(trim_nul(
        &buffer[..(written as usize).min(buffer.len())],
    )))
}

/// Drop trailing NUL padding, which registry data may or may not carry.
fn trim_nul(value: &[u16]) -> &[u16] {
    match value.iter().position(|unit| *unit == 0) {
        Some(end) => &value[..end],
        None => value,
    }
}

/// NUL-terminated UTF-16 copy of `value`.
fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Closes a registry key on drop.
struct KeyHandle(HKEY);

impl Drop for KeyHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful RegOpenKeyExW.
        unsafe { RegCloseKey(self.0) };
    }
}
