use std::{ffi::OsStr, mem, os::windows::ffi::OsStrExt, ptr, slice};

use dolang_vfs::target::OperatingSystem;
use dolang_vfs::{
    error::{Error, ErrorKind},
    extension::ExtContext,
    path,
};
use dolang_winterop::security::{SecDesc, Sid};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_ALIAS_EXISTS, ERROR_ALREADY_ASSIGNED, ERROR_BAD_DEV_TYPE,
        ERROR_BAD_NET_NAME, ERROR_BAD_NETPATH, ERROR_CONNECTION_UNAVAIL,
        ERROR_DEVICE_ALREADY_REMEMBERED, ERROR_DEVICE_IN_USE, ERROR_EXTENDED_ERROR,
        ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_ADDRESS, ERROR_INVALID_HANDLE,
        ERROR_INVALID_PARAMETER, ERROR_INVALID_PASSWORD, ERROR_LOGON_FAILURE,
        ERROR_MEMBER_IN_ALIAS, ERROR_MEMBER_NOT_IN_ALIAS, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS,
        ERROR_NO_NET_OR_BAD_PATH, ERROR_NO_SUCH_ALIAS, ERROR_NO_SUCH_PRIVILEGE, ERROR_NONE_MAPPED,
        ERROR_NOT_CONNECTED, ERROR_NOT_FOUND, ERROR_OPEN_FILES, FILETIME, GetLastError, NO_ERROR,
        STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS,
    },
    NetworkManagement::NetManagement::{
        FILTER_NORMAL_ACCOUNT, LOCALGROUP_INFO_0, LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_0,
        MAX_PREFERRED_LENGTH, NERR_DefaultJoinRequired, NERR_DuplicateShare, NERR_GroupExists,
        NERR_GroupNotFound, NERR_InvalidComputer, NERR_InvalidWorkgroupName, NERR_NetNameNotFound,
        NERR_PasswordTooShort, NERR_ServerNotStarted, NERR_SetupAlreadyJoined,
        NERR_SetupDomainController, NERR_SetupNotJoined, NERR_Success, NERR_UnknownDevDir,
        NERR_UserExists, NERR_UserNotFound, NETSETUP_ACCT_CREATE, NETSETUP_ACCT_DELETE,
        NETSETUP_PROVISION_ONLINE_CALLER, NetApiBufferFree, NetGetJoinInformation, NetJoinDomain,
        NetLocalGroupAdd, NetLocalGroupAddMember, NetLocalGroupDel, NetLocalGroupDelMember,
        NetLocalGroupEnum, NetLocalGroupGetInfo, NetLocalGroupGetMembers, NetLocalGroupSetInfo,
        NetProvisionComputerAccount, NetRenameMachineInDomain, NetRequestOfflineDomainJoin,
        NetServerGetInfo, NetSetupDomainName, NetSetupUnjoined, NetSetupWorkgroupName,
        NetUnjoinDomain, NetUserAdd, NetUserDel, NetUserEnum, NetUserGetInfo, NetUserModalsGet,
        NetUserModalsSet, NetUserSetInfo, NetWkstaGetInfo, SERVER_INFO_101, USER_INFO_0,
        USER_INFO_1, USER_INFO_4, USER_INFO_1003, USER_INFO_1006, USER_INFO_1007, USER_INFO_1008,
        USER_INFO_1009, USER_INFO_1011, USER_INFO_1012, USER_INFO_1017, USER_INFO_1052,
        USER_INFO_1053, USER_MODALS_INFO_0, USER_MODALS_INFO_3, USER_MODALS_INFO_1001,
        USER_MODALS_INFO_1002, USER_MODALS_INFO_1003, USER_MODALS_INFO_1004, USER_MODALS_INFO_1005,
        WKSTA_INFO_100,
    },
    NetworkManagement::WNet::{
        CONNECT_UPDATE_PROFILE, NETRESOURCEW, RESOURCE_CONNECTED, RESOURCE_REMEMBERED,
        RESOURCETYPE_ANY, RESOURCETYPE_DISK, RESOURCETYPE_PRINT, UNIVERSAL_NAME_INFO_LEVEL,
        UNIVERSAL_NAME_INFOW, WNetAddConnection2W, WNetCancelConnection2W, WNetCloseEnum,
        WNetEnumResourceW, WNetGetLastErrorW, WNetGetUniversalNameW, WNetGetUserW, WNetOpenEnumW,
    },
    Security::{
        Authentication::Identity::{
            LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose,
            LsaEnumerateAccountRights, LsaFreeMemory, LsaNtStatusToWinError, LsaOpenPolicy,
            LsaRemoveAccountRights, POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
        },
        Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_DOMAIN_PASSWORD, CREDENTIALW, CredDeleteW,
            CredWriteW,
        },
        GetLengthSid, GetSecurityDescriptorLength, LookupAccountNameW, SID_NAME_USE,
    },
    Storage::FileSystem::{
        NetShareAdd, NetShareDel, NetShareEnum, NetShareGetInfo, NetShareSetInfo, SHARE_INFO_502,
        SHARE_INFO_1004, SHARE_INFO_1006, SHARE_INFO_1501, STYPE_DEVICE, STYPE_DISKTREE, STYPE_IPC,
        STYPE_MASK, STYPE_PRINTQ, STYPE_SPECIAL, STYPE_TEMPORARY,
    },
};

use crate::wire::{
    AccountPolicy, AccountPolicyUpdate, ConnectionCreate, ConnectionInfo, ConnectionKind,
    ConnectionState, GroupCreate, GroupInfo, GroupUpdate, JoinInfo, JoinKind, JoinRequest,
    MachineInfo, OfflineJoinRequest, ProvisionRequest, RenameRequest, ServerType, ShareCreate,
    ShareInfo, ShareKind, ShareUpdate, UnjoinRequest, UserCreate, UserFlags, UserInfo, UserUpdate,
    WinNetRequest, WinNetResponse,
};

/// Views `count` rows a Windows API returned.
///
/// An empty result may come back without a buffer, which
/// `slice::from_raw_parts` does not accept, so that case is answered without
/// touching the pointer. A non-empty result always has one.
unsafe fn rows<'a, T>(ptr: *const T, count: usize) -> &'a [T] {
    if count == 0 {
        return &[];
    }
    assert!(
        !ptr.is_null(),
        "Windows returned {count} rows without a buffer"
    );
    unsafe { slice::from_raw_parts(ptr, count) }
}

struct NetBuffer(*mut u8);
impl NetBuffer {
    /// The single structure a successful NetAPI query returned.
    ///
    /// Callers guarantee `T` is the structure for the level they asked for.
    unsafe fn get<T>(&self) -> &T {
        assert!(
            !self.0.is_null(),
            "NetAPI reported success without a buffer"
        );
        unsafe { &*self.0.cast::<T>() }
    }
    /// The `count` rows a NetAPI enumeration returned.
    ///
    /// Callers guarantee `T` is the structure for the level they asked for.
    unsafe fn rows<T>(&self, count: u32) -> &[T] {
        unsafe { rows(self.0.cast(), count as usize) }
    }
}
impl Drop for NetBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { NetApiBufferFree(self.0.cast()) };
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}
unsafe fn string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let len = unsafe { (0..).take_while(|&i| *ptr.add(i) != 0).count() };
    String::from_utf16_lossy(unsafe { slice::from_raw_parts(ptr, len) })
}
unsafe fn optional(ptr: *const u16) -> Option<String> {
    let value = unsafe { string(ptr) };
    (!value.is_empty()).then_some(value)
}

fn windows_path<'a>(value: Option<&'a path::PathBuf>, field: &str) -> Result<&'a str, Error> {
    match value {
        Some(path) => match path.kind() {
            path::Kind::Windows => Ok(path.as_str()),
            path::Kind::Unix => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("{field} must use Windows path syntax"),
            )),
        },
        None => Ok(""),
    }
}

fn status(operation: &str, code: u32) -> Error {
    let kind = if code == NERR_UserNotFound
        || code == NERR_GroupNotFound
        || code == ERROR_INVALID_HANDLE
        || code == ERROR_NO_SUCH_ALIAS
        || code == ERROR_NONE_MAPPED
        || code == NERR_NetNameNotFound
        || code == NERR_UnknownDevDir
        || code == NERR_SetupNotJoined
        || code == ERROR_BAD_NET_NAME
        || code == ERROR_BAD_NETPATH
        || code == ERROR_NO_NET_OR_BAD_PATH
        || code == ERROR_NOT_FOUND
    {
        ErrorKind::NotFound
    } else if code == NERR_UserExists
        || code == NERR_GroupExists
        || code == ERROR_ALIAS_EXISTS
        || code == ERROR_MEMBER_IN_ALIAS
        || code == NERR_DuplicateShare
        || code == NERR_SetupAlreadyJoined
        || code == ERROR_ALREADY_ASSIGNED
        || code == ERROR_DEVICE_ALREADY_REMEMBERED
    {
        ErrorKind::AlreadyExists
    } else if code == ERROR_MEMBER_NOT_IN_ALIAS {
        ErrorKind::NotFound
    } else if code == ERROR_ACCESS_DENIED || code == ERROR_LOGON_FAILURE {
        ErrorKind::PermissionDenied
    } else if code == ERROR_NOT_CONNECTED || code == ERROR_CONNECTION_UNAVAIL {
        ErrorKind::NotConnected
    } else if code == ERROR_DEVICE_IN_USE || code == ERROR_OPEN_FILES {
        ErrorKind::ResourceBusy
    } else if code == ERROR_INVALID_PARAMETER
        || code == ERROR_INVALID_PASSWORD
        || code == ERROR_NO_SUCH_PRIVILEGE
        || code == NERR_PasswordTooShort
        || code == NERR_SetupDomainController
        || code == NERR_InvalidComputer
        || code == NERR_InvalidWorkgroupName
        || code == NERR_DefaultJoinRequired
        || code == ERROR_BAD_DEV_TYPE
        || code == ERROR_INVALID_ADDRESS
    {
        ErrorKind::InvalidInput
    } else {
        ErrorKind::Other
    };
    Error::from_system_code(
        kind,
        format!("{operation}: Windows status {code}"),
        OperatingSystem::Windows,
        code as i32,
    )
}

fn lsa_status(operation: &str, code: i32) -> Error {
    status(operation, unsafe { LsaNtStatusToWinError(code) })
}

struct LsaPolicy(LSA_HANDLE);
impl Drop for LsaPolicy {
    fn drop(&mut self) {
        unsafe { LsaClose(self.0) };
    }
}

fn lsa_policy(access: u32) -> Result<LsaPolicy, Error> {
    let attributes = LSA_OBJECT_ATTRIBUTES {
        Length: size_of::<LSA_OBJECT_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    let mut handle = 0;
    let code = unsafe { LsaOpenPolicy(ptr::null(), &attributes, access, &mut handle) };
    if code == STATUS_SUCCESS {
        Ok(LsaPolicy(handle))
    } else {
        Err(lsa_status("LsaOpenPolicy", code))
    }
}

struct LsaBuffer(*mut core::ffi::c_void);
impl Drop for LsaBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LsaFreeMemory(self.0) };
        }
    }
}

fn lsa_string(value: &mut [u16]) -> Result<LSA_UNICODE_STRING, Error> {
    let bytes = value
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "account right name is too long"))?;
    Ok(LSA_UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: value.as_mut_ptr(),
    })
}

fn account_rights(sid: &Sid) -> Result<Vec<String>, Error> {
    let policy = lsa_policy(POLICY_LOOKUP_NAMES as u32)?;
    let mut sid = sid.to_bytes();
    let mut raw = ptr::null_mut();
    let mut count = 0;
    let code = unsafe {
        LsaEnumerateAccountRights(policy.0, sid.as_mut_ptr().cast(), &mut raw, &mut count)
    };
    if code == STATUS_OBJECT_NAME_NOT_FOUND {
        return Ok(Vec::new());
    }
    if code != STATUS_SUCCESS {
        return Err(lsa_status("LsaEnumerateAccountRights", code));
    }
    let buffer = LsaBuffer(raw.cast());
    let result = unsafe { rows(raw, count as usize) }
        .iter()
        .map(|row| {
            let units = usize::from(row.Length) / size_of::<u16>();
            String::from_utf16(unsafe { rows(row.Buffer, units) })
                .map_err(|e| Error::new(ErrorKind::InvalidData, e))
        })
        .collect::<Result<Vec<_>, _>>();
    drop(buffer);
    result
}

fn change_account_right(sid: &Sid, right: &str, grant: bool) -> Result<(), Error> {
    let access = POLICY_LOOKUP_NAMES | if grant { POLICY_CREATE_ACCOUNT } else { 0 };
    let policy = lsa_policy(access as u32)?;
    let mut sid = sid.to_bytes();
    let mut right = OsStr::new(right).encode_wide().collect::<Vec<_>>();
    if right.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "account right name must not be empty",
        ));
    }
    let right = lsa_string(&mut right)?;
    let code = unsafe {
        if grant {
            LsaAddAccountRights(policy.0, sid.as_mut_ptr().cast(), &right, 1)
        } else {
            LsaRemoveAccountRights(policy.0, sid.as_mut_ptr().cast(), false, &right, 1)
        }
    };
    if code == STATUS_SUCCESS || (!grant && code == STATUS_OBJECT_NAME_NOT_FOUND) {
        Ok(())
    } else {
        Err(lsa_status(
            if grant {
                "LsaAddAccountRights"
            } else {
                "LsaRemoveAccountRights"
            },
            code,
        ))
    }
}

fn modal_get(level: u32) -> Result<NetBuffer, Error> {
    let mut raw = ptr::null_mut();
    let code = unsafe { NetUserModalsGet(ptr::null(), level, &mut raw) };
    if code == NERR_Success {
        Ok(NetBuffer(raw))
    } else {
        Err(status(&format!("NetUserModalsGet level {level}"), code))
    }
}

fn account_policy() -> Result<AccountPolicy, Error> {
    let zero = modal_get(0)?;
    let zero = unsafe { zero.get::<USER_MODALS_INFO_0>() };
    let three = modal_get(3)?;
    let three = unsafe { three.get::<USER_MODALS_INFO_3>() };
    Ok(AccountPolicy {
        min_password_length: zero.usrmod0_min_passwd_len,
        max_password_age: (zero.usrmod0_max_passwd_age != u32::MAX)
            .then_some(u64::from(zero.usrmod0_max_passwd_age)),
        min_password_age: u64::from(zero.usrmod0_min_passwd_age),
        force_logoff: (zero.usrmod0_force_logoff != u32::MAX)
            .then_some(u64::from(zero.usrmod0_force_logoff)),
        password_history_length: zero.usrmod0_password_hist_len,
        lockout_duration: u64::from(three.usrmod3_lockout_duration),
        lockout_observation_window: u64::from(three.usrmod3_lockout_observation_window),
        lockout_threshold: three.usrmod3_lockout_threshold,
    })
}

fn modal_set<T>(level: u32, value: &T) -> Result<(), Error> {
    let mut parm = 0;
    let code =
        unsafe { NetUserModalsSet(ptr::null(), level, (value as *const T).cast(), &mut parm) };
    if code == NERR_Success {
        Ok(())
    } else {
        Err(status(
            &format!("NetUserModalsSet level {level} parameter {parm}"),
            code,
        ))
    }
}

fn seconds(value: u64, field: &str) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("{field} exceeds the Windows policy range"),
        )
    })
}

fn update_account_policy(update: AccountPolicyUpdate) -> Result<AccountPolicy, Error> {
    if let Some(value) = update.min_password_length {
        modal_set(
            1001,
            &USER_MODALS_INFO_1001 {
                usrmod1001_min_passwd_len: value,
            },
        )?;
    }
    if let Some(value) = update.max_password_age {
        modal_set(
            1002,
            &USER_MODALS_INFO_1002 {
                usrmod1002_max_passwd_age: match value {
                    Some(v) => seconds(v, "max_password_age")?,
                    None => u32::MAX,
                },
            },
        )?;
    }
    if let Some(value) = update.min_password_age {
        modal_set(
            1003,
            &USER_MODALS_INFO_1003 {
                usrmod1003_min_passwd_age: seconds(value, "min_password_age")?,
            },
        )?;
    }
    if let Some(value) = update.force_logoff {
        modal_set(
            1004,
            &USER_MODALS_INFO_1004 {
                usrmod1004_force_logoff: match value {
                    Some(v) => seconds(v, "force_logoff")?,
                    None => u32::MAX,
                },
            },
        )?;
    }
    if let Some(value) = update.password_history_length {
        modal_set(
            1005,
            &USER_MODALS_INFO_1005 {
                usrmod1005_password_hist_len: value,
            },
        )?;
    }
    if update.lockout_duration.is_some()
        || update.lockout_observation_window.is_some()
        || update.lockout_threshold.is_some()
    {
        let current = account_policy()?;
        modal_set(
            3,
            &USER_MODALS_INFO_3 {
                usrmod3_lockout_duration: seconds(
                    update.lockout_duration.unwrap_or(current.lockout_duration),
                    "lockout_duration",
                )?,
                usrmod3_lockout_observation_window: seconds(
                    update
                        .lockout_observation_window
                        .unwrap_or(current.lockout_observation_window),
                    "lockout_observation_window",
                )?,
                usrmod3_lockout_threshold: update
                    .lockout_threshold
                    .unwrap_or(current.lockout_threshold),
            },
        )?;
    }
    account_policy()
}

unsafe fn sid_from_raw(raw: *mut core::ffi::c_void) -> Result<Sid, Error> {
    assert!(!raw.is_null(), "Windows returned an account without a SID");
    let len = unsafe { GetLengthSid(raw) } as usize;
    Sid::from_bytes(unsafe { slice::from_raw_parts(raw.cast(), len) })
        .map_err(|e| Error::new(ErrorKind::InvalidData, e))
}

fn get(name: &str) -> Result<(Sid, UserInfo), Error> {
    let name_w = wide(name);
    let mut raw = ptr::null_mut();
    let code = unsafe { NetUserGetInfo(ptr::null(), name_w.as_ptr(), 4, &mut raw) };
    if code != NERR_Success {
        return Err(status("NetUserGetInfo", code));
    }
    let buffer = NetBuffer(raw);
    let info = unsafe { buffer.get::<USER_INFO_4>() };
    let sid = unsafe { sid_from_raw(info.usri4_user_sid) }?;
    let result = UserInfo {
        sid: sid.clone(),
        name: unsafe { string(info.usri4_name) },
        full_name: unsafe { optional(info.usri4_full_name) },
        comment: unsafe { optional(info.usri4_comment) },
        user_comment: unsafe { optional(info.usri4_usr_comment) },
        home_dir: unsafe { optional(info.usri4_home_dir) }.map(path::PathBuf::from_windows),
        home_dir_drive: unsafe { optional(info.usri4_home_dir_drive) },
        profile: unsafe { optional(info.usri4_profile) }.map(path::PathBuf::from_windows),
        script_path: unsafe { optional(info.usri4_script_path) }.map(path::PathBuf::from_windows),
        flags: UserFlags::from_bits_retain(info.usri4_flags),
        password_age: u64::from(info.usri4_password_age),
        password_expired: info.usri4_password_expired != 0,
        last_logon: (info.usri4_last_logon != 0).then_some(u64::from(info.usri4_last_logon)),
        account_expires: (info.usri4_acct_expires != u32::MAX)
            .then_some(u64::from(info.usri4_acct_expires)),
        bad_password_count: info.usri4_bad_pw_count,
        logon_count: info.usri4_num_logons,
    };
    Ok((sid, result))
}

fn verified(name: &str, expected: &Sid) -> Result<UserInfo, Error> {
    let (actual, info) = get(name)?;
    if &actual != expected {
        return Err(Error::new(
            ErrorKind::NotFound,
            "cached user name now identifies a different SID",
        ));
    }
    Ok(info)
}

fn set<T>(name: &str, level: u32, value: &T) -> Result<(), Error> {
    let name = wide(name);
    let mut parm = 0;
    let code = unsafe {
        NetUserSetInfo(
            ptr::null(),
            name.as_ptr(),
            level,
            (value as *const T).cast(),
            &mut parm,
        )
    };
    if code == NERR_Success {
        Ok(())
    } else {
        Err(status(
            &format!("NetUserSetInfo level {level} parameter {parm}"),
            code,
        ))
    }
}

fn update(name: &str, expected: &Sid, update: UserUpdate) -> Result<UserInfo, Error> {
    let current = verified(name, expected)?;
    let rename = update.name.clone();
    let set_flags = update.set_flags();
    let clear_flags = update.clear_flags();
    macro_rules! text {
        ($field:ident, $level:expr, $ty:ident, $member:ident) => {
            if let Some(value) = update.$field {
                let mut w = wide(value.as_deref().unwrap_or(""));
                set(
                    name,
                    $level,
                    &$ty {
                        $member: w.as_mut_ptr(),
                    },
                )?;
            }
        };
    }
    if let Some(password) = update.password {
        let mut w = wide(&password);
        set(
            name,
            1003,
            &USER_INFO_1003 {
                usri1003_password: w.as_mut_ptr(),
            },
        )?;
    }
    text!(comment, 1007, USER_INFO_1007, usri1007_comment);
    text!(full_name, 1011, USER_INFO_1011, usri1011_full_name);
    text!(user_comment, 1012, USER_INFO_1012, usri1012_usr_comment);
    text!(
        home_dir_drive,
        1053,
        USER_INFO_1053,
        usri1053_home_dir_drive
    );
    macro_rules! path {
        ($field:ident, $level:expr, $ty:ident, $member:ident) => {
            if let Some(value) = update.$field {
                let mut w = wide(windows_path(value.as_ref(), stringify!($field))?);
                set(
                    name,
                    $level,
                    &$ty {
                        $member: w.as_mut_ptr(),
                    },
                )?;
            }
        };
    }
    path!(home_dir, 1006, USER_INFO_1006, usri1006_home_dir);
    path!(script_path, 1009, USER_INFO_1009, usri1009_script_path);
    path!(profile, 1052, USER_INFO_1052, usri1052_profile);
    if let Some(expires) = update.account_expires {
        let value = expires.map_or(u32::MAX, |v| u32::try_from(v).unwrap_or(u32::MAX));
        set(
            name,
            1017,
            &USER_INFO_1017 {
                usri1017_acct_expires: value,
            },
        )?;
    }
    if !set_flags.is_empty() || !clear_flags.is_empty() {
        let flags = (current.flags | set_flags) & !clear_flags;
        set(
            name,
            1008,
            &USER_INFO_1008 {
                usri1008_flags: flags.bits(),
            },
        )?;
    }
    let mut result = verified(name, expected)?;
    if let Some(new_name) = rename {
        let mut w = wide(&new_name);
        set(
            name,
            0,
            &USER_INFO_0 {
                usri0_name: w.as_mut_ptr(),
            },
        )?;
        result.name = new_name;
    }
    Ok(result)
}

fn account_sid(name: &str) -> Result<Sid, Error> {
    let name = wide(name);
    let mut sid_len = 0;
    let mut domain_len = 0;
    let mut use_kind: SID_NAME_USE = 0;
    unsafe {
        LookupAccountNameW(
            ptr::null(),
            name.as_ptr(),
            ptr::null_mut(),
            &mut sid_len,
            ptr::null_mut(),
            &mut domain_len,
            &mut use_kind,
        )
    };
    if unsafe { windows_sys::Win32::Foundation::GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
        return Err(status("LookupAccountNameW", unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        }));
    }
    let mut sid = vec![0u8; sid_len as usize];
    let mut domain = vec![0u16; domain_len as usize];
    if unsafe {
        LookupAccountNameW(
            ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_kind,
        )
    } == 0
    {
        return Err(status("LookupAccountNameW", unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        }));
    }
    Sid::from_bytes(&sid).map_err(|e| Error::new(ErrorKind::InvalidData, e))
}

fn group_get(name: &str) -> Result<(Sid, GroupInfo), Error> {
    let w = wide(name);
    let mut raw = ptr::null_mut();
    let code = unsafe { NetLocalGroupGetInfo(ptr::null(), w.as_ptr(), 1, &mut raw) };
    if code != NERR_Success {
        return Err(status("NetLocalGroupGetInfo", code));
    }
    let buffer = NetBuffer(raw);
    let row = unsafe { buffer.get::<LOCALGROUP_INFO_1>() };
    let actual = unsafe { string(row.lgrpi1_name) };
    let sid = account_sid(&actual)?;
    Ok((
        sid.clone(),
        GroupInfo {
            sid,
            name: actual,
            comment: unsafe { optional(row.lgrpi1_comment) },
        },
    ))
}
fn group_verified(name: &str, expected: &Sid) -> Result<GroupInfo, Error> {
    let (sid, info) = group_get(name)?;
    if &sid != expected {
        return Err(Error::new(
            ErrorKind::NotFound,
            "cached group name now identifies a different SID",
        ));
    }
    Ok(info)
}
fn group_set<T>(name: &str, level: u32, value: &T) -> Result<(), Error> {
    let name = wide(name);
    let mut parm = 0;
    let code = unsafe {
        NetLocalGroupSetInfo(
            ptr::null(),
            name.as_ptr(),
            level,
            (value as *const T).cast(),
            &mut parm,
        )
    };
    if code == NERR_Success {
        Ok(())
    } else {
        Err(status(
            &format!("NetLocalGroupSetInfo level {level} parameter {parm}"),
            code,
        ))
    }
}
fn group_update(name: &str, expected: &Sid, update: GroupUpdate) -> Result<GroupInfo, Error> {
    group_verified(name, expected)?;
    if let Some(comment) = update.comment {
        let mut w = wide(comment.as_deref().unwrap_or(""));
        group_set(
            name,
            1002,
            &windows_sys::Win32::NetworkManagement::NetManagement::LOCALGROUP_INFO_1002 {
                lgrpi1002_comment: w.as_mut_ptr(),
            },
        )?;
    }
    let mut result = group_verified(name, expected)?;
    if let Some(new_name) = update.name {
        let mut w = wide(&new_name);
        group_set(
            name,
            0,
            &LOCALGROUP_INFO_0 {
                lgrpi0_name: w.as_mut_ptr(),
            },
        )?;
        result.name = new_name;
    }
    Ok(result)
}
fn group_create(create: GroupCreate) -> Result<(String, Sid), Error> {
    let mut name = wide(&create.name);
    let raw = LOCALGROUP_INFO_0 {
        lgrpi0_name: name.as_mut_ptr(),
    };
    let mut parm = 0;
    let code = unsafe {
        NetLocalGroupAdd(
            ptr::null(),
            0,
            (&raw as *const LOCALGROUP_INFO_0).cast(),
            &mut parm,
        )
    };
    if code != NERR_Success {
        return Err(status(&format!("NetLocalGroupAdd parameter {parm}"), code));
    }
    let (sid, _) = match group_get(&create.name) {
        Ok(group) => group,
        Err(error) => {
            unsafe { NetLocalGroupDel(ptr::null(), name.as_ptr()) };
            return Err(error);
        }
    };
    if let Some(comment) = create.comment
        && let Err(e) = group_update(
            &create.name,
            &sid,
            GroupUpdate::default().comment(Some(comment)),
        )
    {
        unsafe { NetLocalGroupDel(ptr::null(), name.as_ptr()) };
        return Err(e);
    }
    Ok((create.name, sid))
}
fn group_member(name: &str, sid: &Sid, member: Sid, add: bool) -> Result<(), Error> {
    group_verified(name, sid)?;
    let name = wide(name);
    let mut bytes = member.to_bytes();
    let row = LOCALGROUP_MEMBERS_INFO_0 {
        lgrmi0_sid: bytes.as_mut_ptr().cast(),
    };
    let code = unsafe {
        if add {
            NetLocalGroupAddMember(ptr::null(), name.as_ptr(), row.lgrmi0_sid)
        } else {
            NetLocalGroupDelMember(ptr::null(), name.as_ptr(), row.lgrmi0_sid)
        }
    };
    if code == NERR_Success {
        Ok(())
    } else {
        Err(status(
            if add {
                "NetLocalGroupAddMember"
            } else {
                "NetLocalGroupDelMember"
            },
            code,
        ))
    }
}

fn create(create: UserCreate) -> Result<(String, Sid), Error> {
    let mut name = wide(&create.name);
    let mut password = wide(&create.password);
    let mut home = wide(windows_path(
        create.update.home_dir.as_ref().and_then(Option::as_ref),
        "home_dir",
    )?);
    let mut comment = wide(
        create
            .update
            .comment
            .as_ref()
            .and_then(|v| v.as_deref())
            .unwrap_or(""),
    );
    let mut script = wide(windows_path(
        create.update.script_path.as_ref().and_then(Option::as_ref),
        "script_path",
    )?);
    let flags = ((UserFlags::NORMAL_ACCOUNT | create.update.set_flags())
        & !create.update.clear_flags())
    .bits();
    let raw = USER_INFO_1 {
        usri1_name: name.as_mut_ptr(),
        usri1_password: password.as_mut_ptr(),
        usri1_password_age: 0,
        usri1_priv: 1,
        usri1_home_dir: home.as_mut_ptr(),
        usri1_comment: comment.as_mut_ptr(),
        usri1_flags: flags,
        usri1_script_path: script.as_mut_ptr(),
    };
    let mut parm = 0;
    let code = unsafe {
        NetUserAdd(
            ptr::null(),
            1,
            (&raw as *const USER_INFO_1).cast(),
            &mut parm,
        )
    };
    if code != NERR_Success {
        return Err(status(&format!("NetUserAdd parameter {parm}"), code));
    }
    let (sid, _) = match get(&create.name) {
        Ok(user) => user,
        Err(error) => {
            unsafe { NetUserDel(ptr::null(), name.as_ptr()) };
            return Err(error);
        }
    };
    if let Err(error) = update(&create.name, &sid, create.update) {
        unsafe { NetUserDel(ptr::null(), name.as_ptr()) };
        return Err(error);
    }
    Ok((create.name, sid))
}

fn share_kind(value: u32) -> Result<ShareKind, Error> {
    match value & STYPE_MASK {
        STYPE_DISKTREE => Ok(ShareKind::DiskTree),
        STYPE_PRINTQ => Ok(ShareKind::PrintQueue),
        STYPE_DEVICE => Ok(ShareKind::Device),
        STYPE_IPC => Ok(ShareKind::Ipc),
        value => Err(Error::new(
            ErrorKind::InvalidData,
            format!("unknown share type {value}"),
        )),
    }
}
fn share_type(kind: ShareKind, special: bool, temporary: bool) -> u32 {
    let base = match kind {
        ShareKind::DiskTree => STYPE_DISKTREE,
        ShareKind::PrintQueue => STYPE_PRINTQ,
        ShareKind::Device => STYPE_DEVICE,
        ShareKind::Ipc => STYPE_IPC,
    };
    base | if special { STYPE_SPECIAL } else { 0 } | if temporary { STYPE_TEMPORARY } else { 0 }
}
unsafe fn share_descriptor(ptr: *mut core::ffi::c_void) -> Result<Option<SecDesc>, Error> {
    // A share left to the server service's default security carries a null
    // descriptor, which is distinct from an empty one.
    if ptr.is_null() {
        return Ok(None);
    }
    let len = unsafe { GetSecurityDescriptorLength(ptr) } as usize;
    let bytes = unsafe { slice::from_raw_parts(ptr.cast::<u8>(), len) };
    SecDesc::from_bytes(bytes).map(Some).map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("invalid share security descriptor: {e}"),
        )
    })
}
unsafe fn share_info(row: &SHARE_INFO_502) -> Result<ShareInfo, Error> {
    let ty = row.shi502_type;
    let path = unsafe { string(row.shi502_path) };
    Ok(ShareInfo {
        name: unsafe { string(row.shi502_netname) },
        kind: share_kind(ty)?,
        special: ty & STYPE_SPECIAL != 0,
        temporary: ty & STYPE_TEMPORARY != 0,
        comment: unsafe { optional(row.shi502_remark) },
        max_uses: (row.shi502_max_uses != u32::MAX).then_some(row.shi502_max_uses),
        current_uses: row.shi502_current_uses,
        path: path::PathBuf::from_windows(&path),
        sec_desc: unsafe { share_descriptor(row.shi502_security_descriptor) }?,
    })
}
fn share_get(name: &str) -> Result<ShareInfo, Error> {
    let name = wide(name);
    let mut raw = ptr::null_mut();
    let code = unsafe { NetShareGetInfo(ptr::null(), name.as_ptr(), 502, &mut raw) };
    if code != NERR_Success {
        return Err(status("NetShareGetInfo", code));
    }
    let buffer = NetBuffer(raw);
    unsafe { share_info(buffer.get::<SHARE_INFO_502>()) }
}
fn share_set<T>(name: &[u16], level: u32, value: &T) -> Result<(), Error> {
    let mut parm = 0;
    let code = unsafe {
        NetShareSetInfo(
            ptr::null(),
            name.as_ptr(),
            level,
            (value as *const T).cast(),
            &mut parm,
        )
    };
    if code == NERR_Success {
        Ok(())
    } else {
        Err(status(
            &format!("NetShareSetInfo level {level} parameter {parm}"),
            code,
        ))
    }
}
fn share_update(name: &str, update: ShareUpdate) -> Result<ShareInfo, Error> {
    let n = wide(name);
    if let Some(comment) = update.comment {
        let mut value = comment.as_deref().map(wide).unwrap_or_else(|| vec![0]);
        share_set(
            &n,
            1004,
            &SHARE_INFO_1004 {
                shi1004_remark: value.as_mut_ptr(),
            },
        )?;
    }
    if let Some(max) = update.max_uses {
        share_set(
            &n,
            1006,
            &SHARE_INFO_1006 {
                shi1006_max_uses: max.unwrap_or(u32::MAX),
            },
        )?;
    }
    if let Some(descriptor) = update.sec_desc {
        let mut bytes = descriptor.to_bytes();
        share_set(
            &n,
            1501,
            &SHARE_INFO_1501 {
                shi1501_reserved: 0,
                shi1501_security_descriptor: bytes.as_mut_ptr().cast(),
            },
        )?;
    }
    share_get(name)
}
fn share_create(create: ShareCreate) -> Result<ShareInfo, Error> {
    let name = create.name;
    let mut name_w = wide(&name);
    let path = windows_path(Some(&create.path), "path")?;
    let mut path_w = wide(path);
    let mut comment_w = create
        .comment
        .as_deref()
        .map(wide)
        .unwrap_or_else(|| vec![0]);
    let mut descriptor = create.sec_desc.map(|v| v.to_bytes());
    let mut row = SHARE_INFO_502 {
        shi502_netname: name_w.as_mut_ptr(),
        shi502_type: share_type(create.kind, create.special, create.temporary),
        shi502_remark: comment_w.as_mut_ptr(),
        shi502_permissions: 0,
        shi502_max_uses: create.max_uses.unwrap_or(u32::MAX),
        shi502_current_uses: 0,
        shi502_path: path_w.as_mut_ptr(),
        shi502_passwd: ptr::null_mut(),
        shi502_reserved: 0,
        shi502_security_descriptor: descriptor
            .as_mut()
            .map_or(ptr::null_mut(), |v| v.as_mut_ptr().cast()),
    };
    let mut parm = 0;
    let code = unsafe {
        NetShareAdd(
            ptr::null(),
            502,
            (&mut row as *mut SHARE_INFO_502).cast(),
            &mut parm,
        )
    };
    if code != NERR_Success {
        return Err(status(&format!("NetShareAdd parameter {parm}"), code));
    }
    match share_get(&name) {
        Ok(info) => Ok(info),
        Err(error) => {
            unsafe { NetShareDel(ptr::null(), name_w.as_ptr(), 0) };
            Err(error)
        }
    }
}

/// The pointer NetAPI wants for an optional wide-string argument.
fn optional_ptr(value: &Option<Vec<u16>>) -> *const u16 {
    value.as_ref().map_or(ptr::null(), |v| v.as_ptr())
}

/// A buffer aligned for the pointer-bearing structures the MPR writes into it.
struct WideBuffer(Vec<usize>);
impl WideBuffer {
    fn new(bytes: usize) -> Self {
        Self(vec![0; bytes.div_ceil(mem::size_of::<usize>()).max(1)])
    }
    fn bytes(&self) -> u32 {
        // Sized from a constant below u32::MAX and only ever grown to a size
        // the MPR itself asked for, so this cannot overflow in practice.
        u32::try_from(self.0.len() * mem::size_of::<usize>()).unwrap_or(u32::MAX)
    }
    fn grow(&mut self, bytes: u32) {
        let want = (bytes as usize).div_ceil(mem::size_of::<usize>()).max(1);
        if want > self.0.len() {
            self.0.resize(want, 0);
        } else {
            // The MPR reported a size that does not actually advance; grow
            // anyway so a retry loop cannot spin forever.
            self.0.resize(self.0.len() * 2, 0);
        }
    }
    fn as_mut_ptr(&mut self) -> *mut core::ffi::c_void {
        self.0.as_mut_ptr().cast()
    }
    fn as_ptr(&self) -> *const core::ffi::c_void {
        self.0.as_ptr().cast()
    }
}

/// An open MPR enumeration, closed on drop.
struct EnumHandle(*mut core::ffi::c_void);
impl Drop for EnumHandle {
    fn drop(&mut self) {
        unsafe { WNetCloseEnum(self.0) };
    }
}

/// Reports a failed `WNet*` call.
///
/// The provider-specific detail behind `ERROR_EXTENDED_ERROR` is only available
/// from `WNetGetLastErrorW`, and only on the thread that made the call, so it
/// has to be collected here rather than left to the caller.
fn wnet_status(operation: &str, code: u32) -> Error {
    if code == ERROR_EXTENDED_ERROR {
        let mut provider_code = 0;
        let mut message = [0u16; 512];
        let mut provider = [0u16; 256];
        let fetched = unsafe {
            WNetGetLastErrorW(
                &mut provider_code,
                message.as_mut_ptr(),
                message.len() as u32,
                provider.as_mut_ptr(),
                provider.len() as u32,
            )
        };
        if fetched == NO_ERROR {
            let message = unsafe { string(message.as_ptr()) };
            let provider = unsafe { string(provider.as_ptr()) };
            return Error::from_system_code(
                ErrorKind::Other,
                format!("{operation}: {provider} reported error {provider_code}: {message}"),
                OperatingSystem::Windows,
                provider_code as i32,
            );
        }
    }
    status(operation, code)
}

fn connection_kind(value: u32) -> ConnectionKind {
    // Providers may report resource types this binding does not model; they are
    // still real connections, so they enumerate as `Any` rather than failing the
    // whole listing.
    match value {
        RESOURCETYPE_DISK => ConnectionKind::Disk,
        RESOURCETYPE_PRINT => ConnectionKind::Print,
        _ => ConnectionKind::Any,
    }
}
fn connection_type(kind: ConnectionKind) -> u32 {
    match kind {
        ConnectionKind::Disk => RESOURCETYPE_DISK,
        ConnectionKind::Print => RESOURCETYPE_PRINT,
        ConnectionKind::Any => RESOURCETYPE_ANY,
    }
}

/// The account a connection authenticated as, when the provider reports one.
///
/// Best effort: a remembered connection that is not currently established has no
/// user to report, which is not an error.
fn connection_user(name: &str) -> Option<String> {
    let name = wide(name);
    let mut buffer = vec![0u16; 256];
    loop {
        let mut len = buffer.len() as u32;
        let code = unsafe { WNetGetUserW(name.as_ptr(), buffer.as_mut_ptr(), &mut len) };
        if code == ERROR_MORE_DATA && len as usize > buffer.len() {
            buffer.resize(len as usize, 0);
            continue;
        }
        if code != NO_ERROR {
            return None;
        }
        return unsafe { optional(buffer.as_ptr()) };
    }
}

/// Enumerates one connection scope.
fn connection_scope(scope: u32, state: ConnectionState) -> Result<Vec<ConnectionInfo>, Error> {
    let mut raw = ptr::null_mut();
    let code = unsafe { WNetOpenEnumW(scope, RESOURCETYPE_ANY, 0, ptr::null(), &mut raw) };
    if code != NO_ERROR {
        return Err(wnet_status("WNetOpenEnumW", code));
    }
    let handle = EnumHandle(raw);
    let mut buffer = WideBuffer::new(16 * 1024);
    let mut entries = Vec::new();
    loop {
        // `u32::MAX` asks for as many entries as the buffer will hold.
        let mut count = u32::MAX;
        let mut size = buffer.bytes();
        let code =
            unsafe { WNetEnumResourceW(handle.0, &mut count, buffer.as_mut_ptr(), &mut size) };
        if code == ERROR_NO_MORE_ITEMS {
            return Ok(entries);
        }
        if code == ERROR_MORE_DATA {
            buffer.grow(size);
            continue;
        }
        if code != NO_ERROR {
            return Err(wnet_status("WNetEnumResourceW", code));
        }
        let rows = unsafe {
            slice::from_raw_parts(buffer.as_ptr().cast::<NETRESOURCEW>(), count as usize)
        };
        for row in rows {
            entries.push(ConnectionInfo {
                local: unsafe { optional(row.lpLocalName) },
                remote: unsafe { string(row.lpRemoteName) },
                provider: unsafe { optional(row.lpProvider) },
                user: None,
                kind: connection_kind(row.dwType),
                state,
                persistent: state == ConnectionState::Remembered,
            });
        }
    }
}

/// Whether two enumerated entries name the same connection.
///
/// A device-redirecting connection is identified by its device; a deviceless one
/// only by its remote name.
fn same_connection(left: &ConnectionInfo, right: &ConnectionInfo) -> bool {
    match (&left.local, &right.local) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        (None, None) => left.remote.eq_ignore_ascii_case(&right.remote),
        _ => false,
    }
}

/// Every connection, live or merely remembered.
///
/// The two scopes overlap: a persistent mapping whose server is reachable
/// appears in both. Connected wins, and presence in the remembered scope is what
/// sets `persistent`.
fn connections() -> Result<Vec<ConnectionInfo>, Error> {
    let mut entries = connection_scope(RESOURCE_CONNECTED, ConnectionState::Connected)?;
    for remembered in connection_scope(RESOURCE_REMEMBERED, ConnectionState::Remembered)? {
        match entries
            .iter_mut()
            .find(|entry| same_connection(entry, &remembered))
        {
            Some(entry) => entry.persistent = true,
            None => entries.push(remembered),
        }
    }
    for entry in &mut entries {
        entry.user = connection_user(entry.name());
    }
    Ok(entries)
}

/// Looks up one connection by local device or remote name.
///
/// Resolved from the enumeration rather than `WNetGetConnectionW`, which reports
/// only the remote name and so cannot fill in provider, state or persistence.
fn connection_get(name: &str) -> Result<ConnectionInfo, Error> {
    connections()?
        .into_iter()
        .find(|entry| {
            entry
                .local
                .as_deref()
                .is_some_and(|local| local.eq_ignore_ascii_case(name))
                || entry.remote.eq_ignore_ascii_case(name)
        })
        .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no connection named {name}")))
}

/// The Credential Manager target for a remote resource: its server.
fn credential_target(remote: &str) -> Result<&str, Error> {
    let invalid = || {
        Error::new(
            ErrorKind::InvalidInput,
            format!(r"remote must be a UNC name of the form \\server\share, got {remote}"),
        )
    };
    let rest = remote
        .strip_prefix(r"\\")
        .or_else(|| remote.strip_prefix("//"))
        .ok_or_else(invalid)?;
    let server = rest.split(['\\', '/']).next().unwrap_or_default();
    if server.is_empty() {
        return Err(invalid());
    }
    Ok(server)
}

/// Saves credentials for a server so a persistent connection can reconnect.
///
/// `WNetAddConnection2W` does not do this itself, so without it a mapping
/// restored at logon has nothing to authenticate with.
fn credential_store(target: &str, user: &str, password: &str) -> Result<(), Error> {
    let mut target_w = wide(target);
    let mut user_w = wide(user);
    // The blob is the password's UTF-16 code units, without the terminator.
    let mut blob: Vec<u16> = password.encode_utf16().collect();
    let size = u32::try_from(blob.len() * mem::size_of::<u16>())
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "password is too long to store"))?;
    let credential = CREDENTIALW {
        Flags: 0,
        Type: CRED_TYPE_DOMAIN_PASSWORD,
        TargetName: target_w.as_mut_ptr(),
        Comment: ptr::null_mut(),
        LastWritten: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        CredentialBlobSize: size,
        CredentialBlob: blob.as_mut_ptr().cast(),
        // Local machine rather than enterprise: a CI credential should stay on
        // the box instead of roaming with the profile.
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: ptr::null_mut(),
        TargetAlias: ptr::null_mut(),
        UserName: user_w.as_mut_ptr(),
    };
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(status("CredWriteW", unsafe { GetLastError() }));
    }
    Ok(())
}

/// Removes saved credentials for a server, tolerating their absence.
fn credential_forget(target: &str) -> Result<(), Error> {
    let target = wide(target);
    if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_DOMAIN_PASSWORD, 0) } != 0 {
        return Ok(());
    }
    let code = unsafe { GetLastError() };
    if code == ERROR_NOT_FOUND {
        return Ok(());
    }
    Err(status("CredDeleteW", code))
}

fn connection_add(create: ConnectionCreate) -> Result<ConnectionInfo, Error> {
    // Windows only remembers connections that redirect a local device, and
    // silently ignores CONNECT_UPDATE_PROFILE otherwise, so the mapping would
    // evaporate at the next logon rather than fail here.
    if create.persistent && create.local.is_none() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "a persistent connection must redirect a local device",
        ));
    }
    let store = create.stores_credentials();
    if store && create.user.is_none() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "saving credentials requires a user name",
        ));
    }
    let mut remote_w = wide(&create.remote);
    let mut local_w = create.local.as_deref().map(wide);
    let user_w = create.user.as_deref().map(wide);
    let password_w = create.password.as_deref().map(wide);
    let resource = NETRESOURCEW {
        dwScope: 0,
        dwType: connection_type(create.kind),
        dwDisplayType: 0,
        dwUsage: 0,
        lpLocalName: local_w
            .as_mut()
            .map_or(ptr::null_mut(), |value| value.as_mut_ptr()),
        lpRemoteName: remote_w.as_mut_ptr(),
        lpComment: ptr::null_mut(),
        lpProvider: ptr::null_mut(),
    };
    let flags = if create.persistent {
        CONNECT_UPDATE_PROFILE
    } else {
        0
    };
    let code = unsafe {
        WNetAddConnection2W(
            &resource,
            optional_ptr(&password_w),
            optional_ptr(&user_w),
            flags,
        )
    };
    if code != NO_ERROR {
        return Err(wnet_status("WNetAddConnection2W", code));
    }
    let name = create
        .local
        .clone()
        .unwrap_or_else(|| create.remote.clone());
    let finish = || -> Result<ConnectionInfo, Error> {
        if store {
            let user = create.user.as_deref().unwrap_or_default();
            let password = create.password.as_deref().unwrap_or_default();
            credential_store(credential_target(&create.remote)?, user, password)?;
        }
        connection_get(&name)
    };
    match finish() {
        Ok(info) => Ok(info),
        Err(error) => {
            let name = wide(&name);
            unsafe { WNetCancelConnection2W(name.as_ptr(), flags, 1) };
            Err(error)
        }
    }
}

fn connection_cancel(
    name: &str,
    force: bool,
    forget_credentials: Option<bool>,
) -> Result<(), Error> {
    let info = connection_get(name)?;
    // Removing the profile entry is what makes a persistent mapping stay gone.
    let flags = if info.persistent {
        CONNECT_UPDATE_PROFILE
    } else {
        0
    };
    let name_w = wide(name);
    let code = unsafe { WNetCancelConnection2W(name_w.as_ptr(), flags, i32::from(force)) };
    if code != NO_ERROR {
        return Err(wnet_status("WNetCancelConnection2W", code));
    }
    if forget_credentials.unwrap_or(info.persistent) {
        credential_forget(credential_target(&info.remote)?)?;
    }
    Ok(())
}

fn connection_universal_name(path: &path::PathBuf) -> Result<String, Error> {
    let value = windows_path(Some(path), "path")?;
    let path_w = wide(value);
    let mut buffer = WideBuffer::new(1024);
    loop {
        let mut size = buffer.bytes();
        let code = unsafe {
            WNetGetUniversalNameW(
                path_w.as_ptr(),
                UNIVERSAL_NAME_INFO_LEVEL,
                buffer.as_mut_ptr(),
                &mut size,
            )
        };
        if code == ERROR_MORE_DATA {
            buffer.grow(size);
            continue;
        }
        if code != NO_ERROR {
            return Err(wnet_status("WNetGetUniversalNameW", code));
        }
        let info = unsafe { &*buffer.as_ptr().cast::<UNIVERSAL_NAME_INFOW>() };
        return Ok(unsafe { string(info.lpUniversalName) });
    }
}

fn join_info() -> Result<JoinInfo, Error> {
    let mut raw = ptr::null_mut();
    let mut native = 0;
    let code = unsafe { NetGetJoinInformation(ptr::null(), &mut raw, &mut native) };
    if code != NERR_Success {
        return Err(status("NetGetJoinInformation", code));
    }
    let buffer = NetBuffer(raw.cast());
    let name = unsafe { optional(buffer.0.cast::<u16>()) };
    let kind = if native == NetSetupUnjoined {
        JoinKind::Unjoined
    } else if native == NetSetupWorkgroupName {
        JoinKind::Workgroup
    } else if native == NetSetupDomainName {
        JoinKind::Domain
    } else {
        JoinKind::Unknown
    };
    // The name is only meaningful as a workgroup or domain name; an unjoined
    // machine reports its own name here, which would read as membership.
    let name = matches!(kind, JoinKind::Workgroup | JoinKind::Domain)
        .then_some(name)
        .flatten();
    Ok(JoinInfo { kind, name })
}

fn machine_info() -> Result<MachineInfo, Error> {
    let mut raw = ptr::null_mut();
    let code = unsafe { NetWkstaGetInfo(ptr::null(), 100, &mut raw) };
    if code != NERR_Success {
        return Err(status("NetWkstaGetInfo level 100", code));
    }
    let buffer = NetBuffer(raw);
    let wksta = unsafe { buffer.get::<WKSTA_INFO_100>() };
    let name = unsafe { string(wksta.wki100_computername) };
    let domain = unsafe { string(wksta.wki100_langroup) };
    let version_major = wksta.wki100_ver_major;
    let version_minor = wksta.wki100_ver_minor;
    drop(buffer);

    // Machine identity comes from the workstation service. A stopped server
    // service costs only the role mask and comment, so it is reported as a
    // missing half rather than failing the whole query.
    let mut raw = ptr::null_mut();
    let code = unsafe { NetServerGetInfo(ptr::null(), 101, &mut raw) };
    let (comment, server_type, server_started) = if code == NERR_Success {
        let buffer = NetBuffer(raw);
        let server = unsafe { buffer.get::<SERVER_INFO_101>() };
        (
            unsafe { optional(server.sv101_comment) },
            ServerType::from_bits_retain(server.sv101_type),
            true,
        )
    } else if code == NERR_ServerNotStarted {
        (None, ServerType::empty(), false)
    } else {
        return Err(status("NetServerGetInfo level 101", code));
    };

    Ok(MachineInfo {
        name,
        domain,
        version_major,
        version_minor,
        comment,
        server_type,
        server_started,
    })
}

fn join_domain(request: JoinRequest) -> Result<(), Error> {
    let domain = wide(&request.domain);
    let ou = request.ou.as_deref().map(wide);
    let account = request.account.as_deref().map(wide);
    let password = request.password.as_deref().map(wide);
    let code = unsafe {
        NetJoinDomain(
            ptr::null(),
            domain.as_ptr(),
            optional_ptr(&ou),
            optional_ptr(&account),
            optional_ptr(&password),
            request.options.bits(),
        )
    };
    if code != NERR_Success {
        return Err(status("NetJoinDomain", code));
    }
    Ok(())
}

fn unjoin_domain(request: UnjoinRequest) -> Result<(), Error> {
    let account = request.account.as_deref().map(wide);
    let password = request.password.as_deref().map(wide);
    let options = if request.delete_account {
        NETSETUP_ACCT_DELETE
    } else {
        0
    };
    let code = unsafe {
        NetUnjoinDomain(
            ptr::null(),
            optional_ptr(&account),
            optional_ptr(&password),
            options,
        )
    };
    if code != NERR_Success {
        return Err(status("NetUnjoinDomain", code));
    }
    Ok(())
}

fn rename_machine(request: RenameRequest) -> Result<(), Error> {
    let name = wide(&request.name);
    let account = request.account.as_deref().map(wide);
    let password = request.password.as_deref().map(wide);
    let options = if request.create_account {
        NETSETUP_ACCT_CREATE
    } else {
        0
    };
    let code = unsafe {
        NetRenameMachineInDomain(
            ptr::null(),
            name.as_ptr(),
            optional_ptr(&account),
            optional_ptr(&password),
            options,
        )
    };
    if code != NERR_Success {
        return Err(status("NetRenameMachineInDomain", code));
    }
    Ok(())
}

fn provision_computer(request: ProvisionRequest) -> Result<Vec<u8>, Error> {
    let domain = wide(&request.domain);
    let machine = wide(&request.machine);
    let ou = request.ou.as_deref().map(wide);
    let dc = request.dc.as_deref().map(wide);
    let mut raw = ptr::null_mut();
    let mut length = 0;
    let code = unsafe {
        NetProvisionComputerAccount(
            domain.as_ptr(),
            machine.as_ptr(),
            optional_ptr(&ou),
            optional_ptr(&dc),
            request.options.bits(),
            &mut raw,
            &mut length,
            // Only the binary form applies an offline join; asking for the
            // text form as well would leave a second buffer to free.
            ptr::null_mut(),
        )
    };
    if code != NERR_Success {
        return Err(status("NetProvisionComputerAccount", code));
    }
    let buffer = NetBuffer(raw);
    if buffer.0.is_null() || length == 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "NetProvisionComputerAccount returned an empty blob",
        ));
    }
    Ok(unsafe { slice::from_raw_parts(buffer.0, length as usize) }.to_vec())
}

fn apply_offline_join(request: OfflineJoinRequest) -> Result<(), Error> {
    if request.blob.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "offline join blob must not be empty",
        ));
    }
    let length = u32::try_from(request.blob.len())
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "offline join blob is too large"))?;
    let path = wide(windows_path(Some(&request.windows_path), "windows_path")?);
    let options = if request.online {
        NETSETUP_PROVISION_ONLINE_CALLER
    } else {
        0
    };
    let code = unsafe {
        NetRequestOfflineDomainJoin(request.blob.as_ptr(), length, options, path.as_ptr())
    };
    if code != NERR_Success {
        return Err(status("NetRequestOfflineDomainJoin", code));
    }
    Ok(())
}

fn dispatch(request: WinNetRequest) -> Result<WinNetResponse, Error> {
    match request {
        WinNetRequest::UserByName { name } => {
            let (sid, info) = get(&name)?;
            Ok(WinNetResponse::User {
                name: info.name,
                sid,
            })
        }
        WinNetRequest::UserBySid { sid } => Err(Error::new(
            ErrorKind::NotFound,
            format!("SID {sid} requires VFS name resolution"),
        )),
        WinNetRequest::UsersPage { mut resume } => {
            let mut native_resume = u32::try_from(resume).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "user enumeration resume handle exceeds u32",
                )
            })?;
            let mut raw = ptr::null_mut();
            let mut read = 0;
            let mut total = 0;
            let code = unsafe {
                NetUserEnum(
                    ptr::null(),
                    0,
                    FILTER_NORMAL_ACCOUNT,
                    &mut raw,
                    MAX_PREFERRED_LENGTH,
                    &mut read,
                    &mut total,
                    &mut native_resume,
                )
            };
            if code != NERR_Success && code != ERROR_MORE_DATA {
                return Err(status("NetUserEnum", code));
            }
            let buffer = NetBuffer(raw);
            let rows = unsafe { buffer.rows::<USER_INFO_0>(read) };
            let mut users = Vec::with_capacity(rows.len());
            for row in rows {
                let name = unsafe { string(row.usri0_name) };
                match get(&name) {
                    Ok((_, info)) => users.push(info),
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            resume = u64::from(native_resume);
            Ok(WinNetResponse::UsersPage {
                users,
                resume,
                done: code == NERR_Success,
            })
        }
        WinNetRequest::CreateUser(options) => {
            let (name, sid) = create(options)?;
            Ok(WinNetResponse::User { name, sid })
        }
        WinNetRequest::Info { name, sid } => {
            Ok(WinNetResponse::Info(Box::new(verified(&name, &sid)?)))
        }
        WinNetRequest::Update {
            name,
            sid,
            update: patch,
        } => Ok(WinNetResponse::Info(Box::new(update(&name, &sid, patch)?))),
        WinNetRequest::Delete { name, sid } => {
            verified(&name, &sid)?;
            let n = wide(&name);
            let code = unsafe { NetUserDel(ptr::null(), n.as_ptr()) };
            if code != NERR_Success {
                return Err(status("NetUserDel", code));
            }
            Ok(WinNetResponse::Deleted)
        }
        WinNetRequest::GroupByName { name } => {
            let (sid, info) = group_get(&name)?;
            Ok(WinNetResponse::Group {
                name: info.name,
                sid,
            })
        }
        WinNetRequest::GroupsPage { mut resume } => {
            let mut native_resume = usize::try_from(resume).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "group enumeration resume handle exceeds usize",
                )
            })?;
            let mut raw = ptr::null_mut();
            let mut read = 0;
            let mut total = 0;
            let code = unsafe {
                NetLocalGroupEnum(
                    ptr::null(),
                    0,
                    &mut raw,
                    MAX_PREFERRED_LENGTH,
                    &mut read,
                    &mut total,
                    &mut native_resume,
                )
            };
            if code != NERR_Success && code != ERROR_MORE_DATA {
                return Err(status("NetLocalGroupEnum", code));
            }
            let buffer = NetBuffer(raw);
            let rows = unsafe { buffer.rows::<LOCALGROUP_INFO_0>(read) };
            let mut groups = Vec::with_capacity(rows.len());
            for row in rows {
                let name = unsafe { string(row.lgrpi0_name) };
                match group_get(&name) {
                    Ok((_, info)) => groups.push(info),
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            resume = u64::try_from(native_resume).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "group enumeration resume handle exceeds u64",
                )
            })?;
            Ok(WinNetResponse::GroupsPage {
                groups,
                resume,
                done: code == NERR_Success,
            })
        }
        WinNetRequest::CreateGroup(create) => {
            let (name, sid) = group_create(create)?;
            Ok(WinNetResponse::Group { name, sid })
        }
        WinNetRequest::GroupInfo { name, sid } => {
            Ok(WinNetResponse::GroupInfo(group_verified(&name, &sid)?))
        }
        WinNetRequest::GroupUpdate { name, sid, update } => Ok(WinNetResponse::GroupInfo(
            group_update(&name, &sid, update)?,
        )),
        WinNetRequest::GroupMembersPage {
            name,
            sid,
            mut resume,
        } => {
            let mut native_resume = usize::try_from(resume).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "group member resume handle exceeds usize",
                )
            })?;
            group_verified(&name, &sid)?;
            let name = wide(&name);
            let mut raw = ptr::null_mut();
            let mut read = 0;
            let mut total = 0;
            let code = unsafe {
                NetLocalGroupGetMembers(
                    ptr::null(),
                    name.as_ptr(),
                    0,
                    &mut raw,
                    MAX_PREFERRED_LENGTH,
                    &mut read,
                    &mut total,
                    &mut native_resume,
                )
            };
            if code != NERR_Success && code != ERROR_MORE_DATA {
                return Err(status("NetLocalGroupGetMembers", code));
            }
            let buffer = NetBuffer(raw);
            let members = unsafe { buffer.rows::<LOCALGROUP_MEMBERS_INFO_0>(read) }
                .iter()
                .map(|row| unsafe { sid_from_raw(row.lgrmi0_sid) })
                .collect::<Result<Vec<_>, _>>()?;
            resume = u64::try_from(native_resume).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "group member resume handle exceeds u64",
                )
            })?;
            Ok(WinNetResponse::GroupMembersPage {
                members,
                resume,
                done: code == NERR_Success,
            })
        }
        WinNetRequest::GroupAddMember { name, sid, member } => {
            group_member(&name, &sid, member, true)?;
            Ok(WinNetResponse::Unit)
        }
        WinNetRequest::GroupRemoveMember { name, sid, member } => {
            group_member(&name, &sid, member, false)?;
            Ok(WinNetResponse::Unit)
        }
        WinNetRequest::GroupDelete { name, sid } => {
            group_verified(&name, &sid)?;
            let name = wide(&name);
            let code = unsafe { NetLocalGroupDel(ptr::null(), name.as_ptr()) };
            if code != NERR_Success {
                return Err(status("NetLocalGroupDel", code));
            }
            Ok(WinNetResponse::Deleted)
        }
        WinNetRequest::AccountRights { sid } => {
            Ok(WinNetResponse::AccountRights(account_rights(&sid)?))
        }
        WinNetRequest::GrantAccountRight { sid, right } => {
            change_account_right(&sid, &right, true)?;
            Ok(WinNetResponse::Unit)
        }
        WinNetRequest::RevokeAccountRight { sid, right } => {
            change_account_right(&sid, &right, false)?;
            Ok(WinNetResponse::Unit)
        }
        WinNetRequest::AccountPolicy => Ok(WinNetResponse::AccountPolicy(account_policy()?)),
        WinNetRequest::UpdateAccountPolicy(update) => Ok(WinNetResponse::AccountPolicy(
            update_account_policy(update)?,
        )),
        WinNetRequest::ShareInfo { name } => {
            Ok(WinNetResponse::ShareInfo(Box::new(share_get(&name)?)))
        }
        WinNetRequest::SharesPage { mut resume } => {
            let mut native_resume = u32::try_from(resume).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "share enumeration resume handle exceeds u32",
                )
            })?;
            let mut raw = ptr::null_mut();
            let mut read = 0;
            let mut total = 0;
            let code = unsafe {
                NetShareEnum(
                    ptr::null(),
                    502,
                    &mut raw,
                    MAX_PREFERRED_LENGTH,
                    &mut read,
                    &mut total,
                    &mut native_resume,
                )
            };
            if code != NERR_Success && code != ERROR_MORE_DATA {
                return Err(status("NetShareEnum", code));
            }
            let buffer = NetBuffer(raw);
            let shares = unsafe { buffer.rows::<SHARE_INFO_502>(read) }
                .iter()
                .map(|row| unsafe { share_info(row) })
                .collect::<Result<Vec<_>, _>>()?;
            resume = u64::from(native_resume);
            Ok(WinNetResponse::SharesPage {
                shares,
                resume,
                done: code == NERR_Success,
            })
        }
        WinNetRequest::CreateShare(create) => {
            Ok(WinNetResponse::ShareInfo(Box::new(share_create(create)?)))
        }
        WinNetRequest::UpdateShare { name, update } => Ok(WinNetResponse::ShareInfo(Box::new(
            share_update(&name, update)?,
        ))),
        WinNetRequest::DeleteShare { name } => {
            let n = wide(&name);
            let code = unsafe { NetShareDel(ptr::null(), n.as_ptr(), 0) };
            if code != NERR_Success {
                return Err(status("NetShareDel", code));
            }
            Ok(WinNetResponse::Deleted)
        }
        WinNetRequest::JoinStatus => Ok(WinNetResponse::JoinInfo(join_info()?)),
        WinNetRequest::MachineInfo => Ok(WinNetResponse::MachineInfo(Box::new(machine_info()?))),
        WinNetRequest::JoinDomain(request) => {
            join_domain(*request)?;
            Ok(WinNetResponse::Unit)
        }
        WinNetRequest::UnjoinDomain(request) => {
            unjoin_domain(request)?;
            Ok(WinNetResponse::Unit)
        }
        WinNetRequest::RenameMachine(request) => {
            rename_machine(request)?;
            Ok(WinNetResponse::Unit)
        }
        WinNetRequest::ProvisionComputer(request) => {
            Ok(WinNetResponse::Blob(provision_computer(*request)?))
        }
        WinNetRequest::ApplyOfflineJoin(request) => {
            apply_offline_join(*request)?;
            Ok(WinNetResponse::Unit)
        }
        WinNetRequest::ConnectionInfo { name } => Ok(WinNetResponse::ConnectionInfo(Box::new(
            connection_get(&name)?,
        ))),
        WinNetRequest::ConnectionsPage { resume } => {
            // The MPR enumerates through an open handle rather than a resume
            // cookie, so the whole listing is drained here and returned as a
            // single page. Connection counts are bounded by drive letters.
            if resume != 0 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "connection enumeration does not resume",
                ));
            }
            Ok(WinNetResponse::ConnectionsPage {
                connections: connections()?,
                resume: 0,
                done: true,
            })
        }
        WinNetRequest::AddConnection(create) => Ok(WinNetResponse::ConnectionInfo(Box::new(
            connection_add(*create)?,
        ))),
        WinNetRequest::CancelConnection {
            name,
            force,
            forget_credentials,
        } => {
            connection_cancel(&name, force, forget_credentials)?;
            Ok(WinNetResponse::Deleted)
        }
        WinNetRequest::UniversalName { path } => Ok(WinNetResponse::UniversalName(
            connection_universal_name(&path)?,
        )),
    }
}

pub(crate) async fn handle(
    _ctx: &mut ExtContext<'_>,
    request: WinNetRequest,
) -> Result<WinNetResponse, Error> {
    tokio::task::spawn_blocking(move || dispatch(request))
        .await
        .map_err(|e| Error::new(ErrorKind::Other, e))?
}
