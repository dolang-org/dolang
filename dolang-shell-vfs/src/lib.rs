#![deny(warnings)]
#![allow(async_fn_in_trait)]

pub use dolang_rpc::DefaultHandle;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
pub use typed_path::{
    PathType, Utf8TypedPath, Utf8TypedPathBuf, Utf8UnixPath, Utf8UnixPathBuf, Utf8WindowsPath,
    Utf8WindowsPathBuf, Utf8WindowsPrefix,
};
mod client;
mod direct;
mod error;
mod pipe;
mod protocol;
mod read_dir;
mod sec_desc;
mod server;
#[cfg(unix)]
mod service;
mod sid;
#[cfg(windows)]
mod windows;

pub use error::{Error, OperatingSystem, Result, SystemError};
pub use sec_desc::{
    ALL_SECURITY_INFORMATION, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, SACL_SECURITY_INFORMATION, SecDesc, SecDescError,
};
pub use sid::{Sid, SidError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionMode {
    Native,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Architecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessStatus {
    Exited(i32),
    Signaled(i32),
}

impl ProcessStatus {
    pub const fn success(self) -> bool {
        matches!(self, Self::Exited(0))
    }

    pub const fn code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            Self::Signaled(_) => None,
        }
    }

    pub const fn signal(self) -> Option<i32> {
        match self {
            Self::Exited(_) => None,
            Self::Signaled(signal) => Some(signal),
        }
    }

    pub(crate) fn from_native(status: std::process::ExitStatus) -> io::Result<Self> {
        if let Some(code) = status.code() {
            return Ok(Self::Exited(code));
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Ok(Self::Signaled(signal));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process returned an unrepresentable terminal status",
        ))
    }
}

impl Architecture {
    pub fn current() -> Self {
        #[cfg(target_arch = "x86_64")]
        return Self::X86_64;
        #[cfg(target_arch = "aarch64")]
        return Self::Aarch64;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        compile_error!("unsupported target architecture");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatingSystemFamily {
    Unix,
    Windows,
}

impl OperatingSystem {
    pub fn family(&self) -> OperatingSystemFamily {
        match self {
            Self::Linux | Self::Macos => OperatingSystemFamily::Unix,
            Self::Windows => OperatingSystemFamily::Windows,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub operating_system: OperatingSystem,
    pub architecture: Architecture,
    pub logical_cpu_count: u32,
    pub is_wine: Option<bool>,
}

/// Snapshot of a VFS target's process security context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecurityInfo {
    Unix(UnixSecurityInfo),
    Windows(WindowsTokenInfo),
}

impl SecurityInfo {
    pub fn current() -> crate::Result<Self> {
        #[cfg(unix)]
        return Ok(Self::Unix(UnixSecurityInfo::current()?));
        #[cfg(windows)]
        return Ok(Self::Windows(WindowsTokenInfo::current()?));
    }
}

/// Unix identity information for a VFS target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnixSecurityInfo {
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub group_ids: Vec<u32>,
}

#[cfg(unix)]
impl UnixSecurityInfo {
    fn current() -> crate::Result<Self> {
        use nix::unistd::{getegid, geteuid, getgid, getuid};

        let euid = geteuid();
        let egid = getegid();

        Ok(Self {
            uid: getuid().as_raw(),
            gid: getgid().as_raw(),
            euid: euid.as_raw(),
            egid: egid.as_raw(),
            group_ids: current_group_ids(euid, egid)?,
        })
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn current_group_ids(_euid: nix::unistd::Uid, _egid: nix::unistd::Gid) -> crate::Result<Vec<u32>> {
    Ok(nix::unistd::getgroups()
        .map_err(io::Error::from)?
        .into_iter()
        .map(|gid| gid.as_raw())
        .collect())
}

#[cfg(target_os = "macos")]
fn current_group_ids(euid: nix::unistd::Uid, egid: nix::unistd::Gid) -> crate::Result<Vec<u32>> {
    use std::{ffi::CString, ptr, slice};

    // macOS limits the public getgroups/getgrouplist interfaces and resolves
    // extended memberships through opendirectoryd. This SPI returns the full
    // list in a libc-allocated buffer owned by the caller.
    unsafe extern "C" {
        fn getgrouplist_2(
            name: *const libc::c_char,
            base_gid: libc::gid_t,
            groups: *mut *mut libc::gid_t,
        ) -> i32;
    }

    let user = nix::unistd::User::from_uid(euid)
        .map_err(io::Error::from)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "effective user not found"))?;
    let name = CString::new(user.name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "user name contains NUL"))?;
    let mut groups = ptr::null_mut();
    let count = unsafe { getgrouplist_2(name.as_ptr(), egid.as_raw(), &mut groups) };
    if count < 0 {
        if !groups.is_null() {
            unsafe { libc::free(groups.cast()) };
        }
        return Err(io::Error::other("getgrouplist_2 failed").into());
    }
    if count == 0 {
        if !groups.is_null() {
            unsafe { libc::free(groups.cast()) };
        }
        return Ok(Vec::new());
    }
    if count > 0 && groups.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "getgrouplist_2 returned a null group list",
        )
        .into());
    }
    let result = unsafe { slice::from_raw_parts(groups, count as usize) }.to_vec();
    unsafe { libc::free(groups.cast()) };
    Ok(result)
}

/// Windows token information for a VFS target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsTokenInfo {
    pub is_elevated: bool,
    pub user_sid: Sid,
    pub owner_sid: Sid,
    pub primary_group_sid: Sid,
    pub groups: Vec<TokenGroup>,
}

/// A Windows token group SID and its attribute mask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenGroup {
    pub sid: Sid,
    pub attributes: u32,
}

/// Classification returned by Windows account-name lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidNameUse {
    User,
    Group,
    Domain,
    Alias,
    WellKnownGroup,
    DeletedAccount,
    Invalid,
    Unknown,
    Computer,
    Label,
    LogonSession,
}

/// A Windows SID together with its resolved account name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidName {
    pub sid: Sid,
    pub name: String,
    pub domain: String,
    pub kind: SidNameUse,
}

impl WindowsTokenInfo {
    /// Returns the logon SID identified by the token group attributes.
    pub fn logon_sid(&self) -> Option<&Sid> {
        const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
        self.groups
            .iter()
            .find(|group| group.attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID)
            .map(|group| &group.sid)
    }
}

#[cfg(windows)]
impl WindowsTokenInfo {
    fn current() -> crate::Result<Self> {
        use std::{
            io, mem,
            os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
            ptr, slice,
        };
        use windows_sys::Win32::{
            Foundation::HANDLE,
            Security::{
                GetLengthSid, GetTokenInformation, IsValidSid, PSID, TOKEN_ELEVATION, TOKEN_GROUPS,
                TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_PRIMARY_GROUP, TOKEN_QUERY, TOKEN_USER,
                TokenElevation, TokenGroups, TokenOwner, TokenPrimaryGroup, TokenUser,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };

        fn query(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
            let mut required = 0;
            unsafe {
                GetTokenInformation(token, class, ptr::null_mut(), 0, &mut required);
            }
            if required == 0 {
                return Err(io::Error::last_os_error());
            }
            let word_size = mem::size_of::<usize>();
            let mut buffer = vec![0usize; (required as usize).div_ceil(word_size)];
            if unsafe {
                GetTokenInformation(
                    token,
                    class,
                    buffer.as_mut_ptr().cast(),
                    required,
                    &mut required,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(buffer)
        }

        unsafe fn copy_sid(sid: PSID) -> io::Result<Sid> {
            if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid token SID",
                ));
            }
            let length = unsafe { GetLengthSid(sid) } as usize;
            let bytes = unsafe { slice::from_raw_parts(sid.cast::<u8>(), length) };
            Sid::from_bytes(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }

        unsafe fn view<T>(buffer: &[usize]) -> &T {
            unsafe { &*buffer.as_ptr().cast::<T>() }
        }

        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let token = token.as_raw_handle();

        let elevation = query(token, TokenElevation)?;
        let user = query(token, TokenUser)?;
        let owner = query(token, TokenOwner)?;
        let primary_group = query(token, TokenPrimaryGroup)?;
        let groups = query(token, TokenGroups)?;

        let elevation = unsafe { view::<TOKEN_ELEVATION>(&elevation) };
        let user = unsafe { copy_sid(view::<TOKEN_USER>(&user).User.Sid) }?;
        let owner = unsafe { copy_sid(view::<TOKEN_OWNER>(&owner).Owner) }?;
        let primary_group =
            unsafe { copy_sid(view::<TOKEN_PRIMARY_GROUP>(&primary_group).PrimaryGroup) }?;
        let groups_info = unsafe { view::<TOKEN_GROUPS>(&groups) };
        let native_groups = unsafe {
            slice::from_raw_parts(
                groups_info.Groups.as_ptr(),
                usize::try_from(groups_info.GroupCount).unwrap(),
            )
        };
        let groups = native_groups
            .iter()
            .map(|group| {
                Ok(TokenGroup {
                    sid: unsafe { copy_sid(group.Sid) }?,
                    attributes: group.Attributes,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;

        Ok(Self {
            is_elevated: elevation.TokenIsElevated != 0,
            user_sid: user,
            owner_sid: owner,
            primary_group_sid: primary_group,
            groups,
        })
    }
}

impl TargetInfo {
    pub fn current() -> Self {
        Self {
            operating_system: OperatingSystem::current(),
            architecture: Architecture::current(),
            logical_cpu_count: std::thread::available_parallelism()
                .map_or(1, |count| u32::try_from(count.get()).unwrap_or(u32::MAX)),
            is_wine: current_wine_status(),
        }
    }
}

#[cfg(windows)]
fn current_wine_status() -> Option<bool> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::core::w;

    const WINE_GET_VERSION: &[u8] = b"wine_get_version\0";

    let ntdll = unsafe { GetModuleHandleW(w!("ntdll.dll")) };
    Some(!ntdll.is_null() && unsafe { GetProcAddress(ntdll, WINE_GET_VERSION.as_ptr()) }.is_some())
}

#[cfg(not(windows))]
fn current_wine_status() -> Option<bool> {
    None
}

/// Snapshot of a VFS target's initial process context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Environment variables from the target process.
    pub env: HashMap<String, String>,
    /// Target process's current working directory.
    pub cwd: Utf8TypedPathBuf,
    /// Path to the target process's current executable.
    pub current_exe: Utf8TypedPathBuf,
    /// Target operating system and processor information.
    pub target: TargetInfo,
    /// Target process security information.
    pub security: SecurityInfo,
}

impl Query {
    pub fn current() -> crate::Result<Self> {
        Ok(Self {
            env: std::env::vars().collect(),
            cwd: typed_path(std::env::current_dir()?)?,
            current_exe: typed_path(std::env::current_exe()?)?,
            target: TargetInfo::current(),
            security: SecurityInfo::current()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileType {
    File,
    Dir,
    Symlink,
    Fifo,
    CharacterDevice,
    BlockDevice,
    Socket,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    mode: u32,
}

impl Permissions {
    pub fn from_mode(mode: u32) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn set_mode(&mut self, mode: u32) {
        self.mode = mode;
    }

    pub fn readonly(&self) -> bool {
        self.mode & 0o222 == 0
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.mode &= !0o222;
        } else {
            self.mode |= 0o200;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub len: u64,
    pub file_type: FileType,
    pub atime: i64,
    pub atime_nsec: i64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
    pub family: MetadataFamily,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataFamily {
    Unix(UnixMetadata),
    Windows(WindowsMetadata),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnixMetadata {
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub blksize: u64,
    pub blocks: u64,
    pub platform: UnixMetadataPlatform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UnixMetadataPlatform {
    Linux,
    Macos { flags: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowsMetadata {
    pub mode: u32,
    pub attrs: u32,
}

impl Metadata {
    pub fn unix(&self) -> Option<&UnixMetadata> {
        match &self.family {
            MetadataFamily::Unix(metadata) => Some(metadata),
            MetadataFamily::Windows(_) => None,
        }
    }

    pub fn windows(&self) -> Option<&WindowsMetadata> {
        match &self.family {
            MetadataFamily::Unix(_) => None,
            MetadataFamily::Windows(metadata) => Some(metadata),
        }
    }

    pub fn permissions(&self) -> Permissions {
        let mode = match &self.family {
            MetadataFamily::Unix(metadata) => metadata.mode,
            MetadataFamily::Windows(metadata) => metadata.mode,
        };
        Permissions::from_mode(mode)
    }

    pub fn attrs(&self) -> Attrs {
        match &self.family {
            MetadataFamily::Windows(metadata) => Attrs::from_win_attrs(metadata.attrs),
            MetadataFamily::Unix(UnixMetadata {
                platform: UnixMetadataPlatform::Macos { flags },
                ..
            }) => Attrs::from_macos_flags(*flags),
            MetadataFamily::Unix(_) => Attrs::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsMetadata {
    pub capacity: u64,
    pub free: u64,
    pub available: u64,
    pub block_size: u32,
    pub family: FsMetadataFamily,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FsMetadataFamily {
    Unix(UnixFsMetadata),
    Windows(WindowsFsMetadata),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnixFsMetadata {
    pub blocks: u64,
    pub blocks_free: u64,
    pub blocks_available: u64,
    pub files: u64,
    pub files_free: u64,
    pub files_available: u64,
    pub fragment_size: u32,
    pub fsid: Option<u64>,
    pub name_max: u32,
    pub platform: UnixFsMetadataPlatform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UnixFsMetadataPlatform {
    Linux { flags: u64 },
    Macos { flags: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowsFsMetadata {
    pub flags: u32,
    pub volume_serial_number: u32,
    pub component_length_max: u32,
}

impl FsMetadata {
    pub fn unix(&self) -> Option<&UnixFsMetadata> {
        match &self.family {
            FsMetadataFamily::Unix(metadata) => Some(metadata),
            FsMetadataFamily::Windows(_) => None,
        }
    }

    pub fn windows(&self) -> Option<&WindowsFsMetadata> {
        match &self.family {
            FsMetadataFamily::Unix(_) => None,
            FsMetadataFamily::Windows(metadata) => Some(metadata),
        }
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn read_only(&self) -> bool {
        match &self.family {
            FsMetadataFamily::Unix(metadata) => metadata.platform.flags() & 1 != 0,
            FsMetadataFamily::Windows(metadata) => metadata.flags & 0x0008_0000 != 0,
        }
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn no_suid(&self) -> Option<bool> {
        match &self.family {
            FsMetadataFamily::Unix(metadata) => Some(metadata.platform.flags() & 2 != 0),
            FsMetadataFamily::Windows(_) => None,
        }
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn no_exec(&self) -> Option<bool> {
        self.linux_flag(8)
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn synchronous(&self) -> Option<bool> {
        self.linux_flag(16)
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn no_dev(&self) -> Option<bool> {
        self.linux_flag(4)
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn no_atime(&self) -> Option<bool> {
        self.linux_flag(1024)
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn no_dir_atime(&self) -> Option<bool> {
        self.linux_flag(2048)
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn relatime(&self) -> Option<bool> {
        self.linux_flag(1 << 21)
    }

    fn linux_flag(&self, flag: u64) -> Option<bool> {
        match &self.family {
            FsMetadataFamily::Unix(UnixFsMetadata {
                platform: UnixFsMetadataPlatform::Linux { flags },
                ..
            }) => Some(flags & flag != 0),
            _ => None,
        }
    }
}

impl UnixFsMetadataPlatform {
    pub fn flags(&self) -> u64 {
        match self {
            Self::Linux { flags } | Self::Macos { flags } => *flags,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attrs {
    pub readonly: Option<bool>,
    pub hidden: Option<bool>,
    pub system: Option<bool>,
    pub archive: Option<bool>,
    pub reparse_point: Option<bool>,
    pub compressed: Option<bool>,
    pub encrypted: Option<bool>,
    pub temporary: Option<bool>,
    pub offline: Option<bool>,
    pub not_content_indexed: Option<bool>,
    pub immutable: Option<bool>,
    pub append_only: Option<bool>,
    pub no_dump: Option<bool>,
    pub no_atime: Option<bool>,
    pub no_copy_on_write: Option<bool>,
    pub dir_sync: Option<bool>,
    pub casefold: Option<bool>,
    pub data_journaling: Option<bool>,
    pub no_compress: Option<bool>,
    pub project_inherit: Option<bool>,
    pub secure_delete: Option<bool>,
    pub sync: Option<bool>,
    pub no_tail_merge: Option<bool>,
    pub top_dir: Option<bool>,
    pub undelete: Option<bool>,
    pub direct_access: Option<bool>,
    pub extent_format: Option<bool>,
    pub opaque: Option<bool>,
    pub win_attrs: Option<u32>,
    pub unix_flags: Option<u32>,
}

impl Attrs {
    pub fn is_empty_patch(&self) -> bool {
        self.readonly.is_none()
            && self.hidden.is_none()
            && self.system.is_none()
            && self.archive.is_none()
            && self.reparse_point.is_none()
            && self.compressed.is_none()
            && self.encrypted.is_none()
            && self.temporary.is_none()
            && self.offline.is_none()
            && self.not_content_indexed.is_none()
            && self.immutable.is_none()
            && self.append_only.is_none()
            && self.no_dump.is_none()
            && self.no_atime.is_none()
            && self.no_copy_on_write.is_none()
            && self.dir_sync.is_none()
            && self.casefold.is_none()
            && self.data_journaling.is_none()
            && self.no_compress.is_none()
            && self.project_inherit.is_none()
            && self.secure_delete.is_none()
            && self.sync.is_none()
            && self.no_tail_merge.is_none()
            && self.top_dir.is_none()
            && self.undelete.is_none()
            && self.direct_access.is_none()
            && self.extent_format.is_none()
            && self.opaque.is_none()
            && self.win_attrs.is_none()
            && self.unix_flags.is_none()
    }

    pub fn from_win_attrs(attrs: u32) -> Self {
        Self {
            readonly: Some(attrs & 0x0000_0001 != 0),
            hidden: Some(attrs & 0x0000_0002 != 0),
            system: Some(attrs & 0x0000_0004 != 0),
            archive: Some(attrs & 0x0000_0020 != 0),
            reparse_point: Some(attrs & 0x0000_0400 != 0),
            compressed: Some(attrs & 0x0000_0800 != 0),
            encrypted: Some(attrs & 0x0000_4000 != 0),
            temporary: Some(attrs & 0x0000_0100 != 0),
            offline: Some(attrs & 0x0000_1000 != 0),
            not_content_indexed: Some(attrs & 0x0000_2000 != 0),
            win_attrs: Some(attrs),
            ..Self::default()
        }
    }

    pub fn from_macos_flags(flags: u32) -> Self {
        Self {
            hidden: Some(flags & 0x0000_8000 != 0),
            compressed: Some(flags & 0x0000_0020 != 0),
            immutable: Some(flags & 0x0000_0002 != 0),
            append_only: Some(flags & 0x0000_0004 != 0),
            no_dump: Some(flags & 0x0000_0001 != 0),
            opaque: Some(flags & 0x0000_0008 != 0),
            unix_flags: Some(flags),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WellKnownPath {
    HomeDir,
    CacheDir,
    TempDir,
}

pub(crate) fn metadata_from_std(metadata: std::fs::Metadata) -> Metadata {
    #[cfg(unix)]
    {
        use nix::sys::stat::{SFlag, mode_t};
        #[cfg(target_os = "macos")]
        use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
        use std::os::unix::fs::MetadataExt;

        let mode = metadata.mode();
        let file_type = match SFlag::from_bits_truncate(mode as mode_t) & SFlag::S_IFMT {
            SFlag::S_IFREG => crate::FileType::File,
            SFlag::S_IFDIR => crate::FileType::Dir,
            SFlag::S_IFLNK => crate::FileType::Symlink,
            SFlag::S_IFIFO => crate::FileType::Fifo,
            SFlag::S_IFCHR => crate::FileType::CharacterDevice,
            SFlag::S_IFBLK => crate::FileType::BlockDevice,
            SFlag::S_IFSOCK => crate::FileType::Socket,
            _ => crate::FileType::Unknown,
        };

        Metadata {
            len: metadata.len(),
            file_type,
            atime: metadata.atime(),
            atime_nsec: metadata.atime_nsec(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
            family: MetadataFamily::Unix(UnixMetadata {
                mode,
                dev: metadata.dev(),
                ino: metadata.ino(),
                nlink: metadata.nlink(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                rdev: metadata.rdev(),
                blksize: metadata.blksize(),
                blocks: metadata.blocks(),
                #[cfg(target_os = "linux")]
                platform: UnixMetadataPlatform::Linux,
                #[cfg(target_os = "macos")]
                platform: UnixMetadataPlatform::Macos {
                    flags: metadata.st_flags(),
                },
            }),
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        let file_type = if metadata.is_file() {
            crate::FileType::File
        } else if metadata.is_dir() {
            crate::FileType::Dir
        } else if metadata.file_type().is_symlink() {
            crate::FileType::Symlink
        } else {
            crate::FileType::Unknown
        };

        Metadata {
            len: metadata.len(),
            file_type,
            atime: system_time_to_parts(metadata.accessed().ok()).0,
            atime_nsec: i64::from(system_time_to_parts(metadata.accessed().ok()).1),
            mtime: system_time_to_parts(metadata.modified().ok()).0,
            mtime_nsec: i64::from(system_time_to_parts(metadata.modified().ok()).1),
            ctime: system_time_to_parts(metadata.created().ok()).0,
            ctime_nsec: i64::from(system_time_to_parts(metadata.created().ok()).1),
            family: MetadataFamily::Windows(WindowsMetadata {
                mode: if metadata.permissions().readonly() {
                    0o444
                } else {
                    0o666
                },
                attrs: metadata.file_attributes(),
            }),
        }
    }
}

#[cfg(windows)]
fn system_time_to_parts(time: Option<std::time::SystemTime>) -> (i64, u32) {
    use std::time::UNIX_EPOCH;

    let Some(time) = time else {
        return (0, 0);
    };

    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            duration.subsec_nanos(),
        ),
        Err(err) => {
            let duration = err.duration();
            let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
            if duration.subsec_nanos() == 0 {
                (-secs, 0)
            } else {
                (-secs - 1, 1_000_000_000 - duration.subsec_nanos())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChownIdentity {
    Id(u32),
    Name(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XattrNamespace<'a> {
    Default,
    Named(&'a str),
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XattrEntry {
    pub name: String,
    pub namespace: Option<String>,
    pub size: Option<u64>,
    pub flags: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEntry {
    pub name: String,
    pub r#type: String,
    pub size: u64,
    pub alloc_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    file_name: String,
    file_type: FileType,
    family: DirEntryFamily,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirEntryFamily {
    Unix { ino: u64 },
    Windows,
}

impl DirEntry {
    pub fn file_name(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new(&self.file_name)
    }

    pub fn ino(&self) -> Option<u64> {
        match self.family {
            DirEntryFamily::Unix { ino } => Some(ino),
            DirEntryFamily::Windows => None,
        }
    }

    pub fn file_type(&self) -> FileType {
        self.file_type
    }
}

pub use read_dir::ReadDir;

pub fn native_path(path: Utf8TypedPath<'_>) -> io::Result<PathBuf> {
    let matches_target = if cfg!(windows) {
        path.is_windows()
    } else {
        path.is_unix()
    };
    if !matches_target {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path style does not match VFS target",
        ));
    }
    Ok(PathBuf::from(path.as_str()))
}

pub fn typed_path(path: PathBuf) -> io::Result<Utf8TypedPathBuf> {
    let path = path
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "path is not valid UTF-8"))?;
    Ok(if cfg!(windows) {
        Utf8TypedPathBuf::from_windows(path)
    } else {
        Utf8TypedPathBuf::from_unix(path)
    })
}

pub const fn target_path_type() -> PathType {
    if cfg!(windows) {
        PathType::Windows
    } else {
        PathType::Unix
    }
}

#[allow(async_fn_in_trait)]
pub trait OpenOptions {
    type File: FileHandle;

    fn read(&mut self, read: bool) -> &mut Self;
    fn write(&mut self, write: bool) -> &mut Self;
    fn append(&mut self, append: bool) -> &mut Self;
    fn create(&mut self, create: bool) -> &mut Self;
    fn create_new(&mut self, create_new: bool) -> &mut Self;
    fn truncate(&mut self, truncate: bool) -> &mut Self;
    fn no_follow(&mut self, no_follow: bool) -> &mut Self;
    async fn open(&self, path: Utf8TypedPath<'_>) -> Result<Self::File>;
}

pub trait FileHandle: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Sized {
    async fn to_stdio_send(&self) -> Result<StdioSend>;
    async fn to_stdio_recv(&self) -> Result<StdioRecv>;
    async fn close(self) -> Result<()>;
    async fn set_len(&mut self, size: u64) -> Result<()>;
    async fn metadata(&mut self) -> Result<Metadata>;
    async fn fs_metadata(&mut self) -> Result<FsMetadata>;
    async fn sec_desc(&mut self, mask: u32) -> Result<SecDesc>;
    async fn set_sec_desc(&mut self, sec_desc: &SecDesc) -> Result<()>;
    async fn xattrs(&mut self, namespace: XattrNamespace<'_>) -> Result<Vec<XattrEntry>>;
    async fn xattr(&mut self, name: &str, namespace: Option<&str>) -> Result<Vec<u8>>;
    async fn streams(&mut self) -> Result<Vec<StreamEntry>>;
    async fn set_xattr(&mut self, name: &str, namespace: Option<&str>, value: &[u8]) -> Result<()>;
    async fn remove_xattr(&mut self, name: &str, namespace: Option<&str>) -> Result<()>;
    async fn try_into_std(self) -> std::result::Result<std::fs::File, Self>;
}

#[allow(async_fn_in_trait)]
pub trait Child {
    async fn wait(&mut self) -> Result<ProcessStatus>;
    async fn terminate(self) -> Result<ProcessStatus>
    where
        Self: Sized;
}

#[allow(async_fn_in_trait)]
pub trait Command {
    type Child: Child;
    type StdioSend: AsyncWrite + Unpin;
    type StdioRecv: AsyncRead + Unpin;

    fn arg(&mut self, arg: &str) -> &mut Self;
    fn env(&mut self, key: &str, val: &str) -> &mut Self;
    fn env_remove(&mut self, key: &str) -> &mut Self;
    fn current_dir(&mut self, dir: Utf8TypedPath<'_>) -> &mut Self;
    fn stdin(&mut self, stdio: Self::StdioRecv) -> io::Result<&mut Self>;
    fn stdout(&mut self, stdio: Self::StdioSend) -> io::Result<&mut Self>;
    /// Inherit the host process's standard input.
    ///
    /// Opaque remote clients treat terminal input as null because Tokio cannot
    /// cancel an outstanding terminal read. Redirected input is relayed to the
    /// remote process.
    fn stdin_inherit(&mut self) -> io::Result<&mut Self>;
    fn stdout_inherit(&mut self) -> io::Result<&mut Self>;
    fn stdin_null(&mut self) -> &mut Self;
    fn stdout_null(&mut self) -> &mut Self;
    fn stderr(&mut self, stdio: Self::StdioSend) -> io::Result<&mut Self>;
    fn stderr_inherit(&mut self) -> io::Result<&mut Self>;
    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self>;
    fn stderr_null(&mut self) -> &mut Self;
    async fn spawn(self) -> Result<Self::Child>;
}

#[allow(async_fn_in_trait)]
pub trait Vfs {
    type File: FileHandle;
    type StdioSend: AsyncWrite + Unpin;
    type StdioRecv: AsyncRead + Unpin;
    type OpenOptions<'a>: OpenOptions<File = Self::File>
    where
        Self: 'a;
    type Command<'a>: Command<StdioSend = Self::StdioSend, StdioRecv = Self::StdioRecv>
    where
        Self: 'a;

    fn open_options(&self) -> Self::OpenOptions<'_>;
    fn command(&self, program: Utf8TypedPath<'_>) -> Self::Command<'_>;
    async fn pipe(&self) -> Result<(Self::StdioSend, Self::StdioRecv)>;
    async fn query(&self) -> Result<Query>;
    async fn user_name(&self, uid: u32) -> Result<String>;
    async fn user_id(&self, name: &str) -> Result<u32>;
    async fn group_name(&self, gid: u32) -> Result<String>;
    async fn group_id(&self, name: &str) -> Result<u32>;
    async fn sid_name(&self, sid: &Sid) -> Result<SidName>;
    async fn account_name(&self, name: &str) -> Result<SidName>;
    async fn read_dir(&self, path: Utf8TypedPath<'_>) -> Result<ReadDir>;
    async fn which(
        &self,
        program: Utf8TypedPath<'_>,
        path: Option<&str>,
        cwd: Option<Utf8TypedPath<'_>>,
    ) -> Result<Option<Utf8TypedPathBuf>>;
    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> Result<Utf8TypedPathBuf>;
    async fn clear_cache(&self) -> Result<()>;
    async fn xattrs(
        &self,
        path: Utf8TypedPath<'_>,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>>;
    async fn xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>>;
    async fn set_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<()>;
    async fn remove_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<()>;
    async fn streams(&self, path: Utf8TypedPath<'_>, follow: bool) -> Result<Vec<StreamEntry>>;

    async fn remove(&self, path: Utf8TypedPath<'_>, all: bool, ignore: bool) -> Result<()>;
    async fn metadata(&self, path: Utf8TypedPath<'_>) -> Result<Metadata>;
    async fn fs_metadata(&self, path: Utf8TypedPath<'_>, follow: bool) -> Result<FsMetadata>;
    async fn sec_desc(&self, path: Utf8TypedPath<'_>, mask: u32, follow: bool) -> Result<SecDesc>;
    async fn set_sec_desc(
        &self,
        path: Utf8TypedPath<'_>,
        sec_desc: &SecDesc,
        follow: bool,
    ) -> Result<()>;
    async fn create_dir(&self, path: Utf8TypedPath<'_>, all: bool) -> Result<()>;
    async fn remove_dir(&self, path: Utf8TypedPath<'_>, all: bool, ignore: bool) -> Result<()>;
    async fn copy(&self, from: Utf8TypedPath<'_>, to: Utf8TypedPath<'_>, all: bool) -> Result<()>;
    async fn rename(&self, from: Utf8TypedPath<'_>, to: Utf8TypedPath<'_>) -> Result<()>;
    async fn move_(&self, from: Utf8TypedPath<'_>, to: Utf8TypedPath<'_>, all: bool) -> Result<()>;
    async fn symlink(
        &self,
        cwd: Utf8TypedPath<'_>,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<()>;
    async fn hard_link(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> Result<()>;
    async fn symlink_dir(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> Result<()>;
    async fn symlink_file(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> Result<()>;
    async fn symlink_metadata(&self, path: Utf8TypedPath<'_>) -> Result<Metadata>;
    async fn attrs(&self, path: Utf8TypedPath<'_>, follow: bool) -> Result<Attrs>;
    async fn set_attrs(&self, path: Utf8TypedPath<'_>, attrs: Attrs) -> Result<()>;
    async fn canonicalize(&self, path: Utf8TypedPath<'_>) -> Result<Utf8TypedPathBuf>;
    async fn read_link(&self, path: Utf8TypedPath<'_>) -> Result<Utf8TypedPathBuf>;
    async fn glob(
        &self,
        pattern: impl Into<String>,
        root: Utf8TypedPath<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<Utf8TypedPathBuf>>;
    async fn set_permissions(&self, path: Utf8TypedPath<'_>, perm: Permissions) -> Result<()>;
    async fn set_times(
        &self,
        path: Utf8TypedPath<'_>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> Result<()>;
    async fn chown(
        &self,
        path: Utf8TypedPath<'_>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> Result<()>;
}

pub use direct::{Direct, DirectFile, DirectOpenOptions};
pub use pipe::{StdioRecv, StdioSend};

/// Marker for a regular file retained by a VFS RPC session.
#[derive(Debug)]
pub struct FileMarker;

#[derive(Debug)]
pub struct StdioSendMarker;

#[derive(Debug)]
pub struct StdioRecvMarker;

/// Marker for a child process retained by a VFS RPC session.
#[derive(Debug)]
pub struct ChildMarker;

#[derive(Debug)]
pub enum AnyFile {
    Client(client::ClientFile),
    Direct(DirectFile),
}

macro_rules! dispatch_file_mut {
    ($self:expr, $method:ident($($arg:expr),* $(,)?)) => {{
        match $self {
            AnyFile::Client(file) => Pin::new(file).$method($($arg),*),
            AnyFile::Direct(file) => Pin::new(file).$method($($arg),*),
        }
    }};
}

macro_rules! match_file {
    ($self:expr, $file:ident => $body:expr) => {{
        match $self {
            AnyFile::Client($file) => $body,
            AnyFile::Direct($file) => $body,
        }
    }};
}

impl AsyncRead for AnyFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        dispatch_file_mut!(self.as_mut().get_mut(), poll_read(cx, buf))
    }
}

impl AsyncWrite for AnyFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        dispatch_file_mut!(self.as_mut().get_mut(), poll_write(cx, buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        dispatch_file_mut!(self.as_mut().get_mut(), poll_flush(cx))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        dispatch_file_mut!(self.as_mut().get_mut(), poll_shutdown(cx))
    }
}

impl AsyncSeek for AnyFile {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        dispatch_file_mut!(self.as_mut().get_mut(), start_seek(position))
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        dispatch_file_mut!(self.as_mut().get_mut(), poll_complete(cx))
    }
}

impl FileHandle for AnyFile {
    async fn to_stdio_send(&self) -> crate::Result<StdioSend> {
        match self {
            Self::Client(file) => file.to_stdio_send().await,
            Self::Direct(file) => file.to_stdio_send().await,
        }
    }

    async fn to_stdio_recv(&self) -> crate::Result<StdioRecv> {
        match self {
            Self::Client(file) => file.to_stdio_recv().await,
            Self::Direct(file) => file.to_stdio_recv().await,
        }
    }

    async fn close(self) -> crate::Result<()> {
        match self {
            Self::Client(file) => file.close().await,
            Self::Direct(file) => file.close().await,
        }
    }

    async fn set_len(&mut self, size: u64) -> crate::Result<()> {
        match_file!(self, file => file.set_len(size).await)
    }

    async fn metadata(&mut self) -> crate::Result<Metadata> {
        match_file!(self, file => file.metadata().await)
    }

    async fn fs_metadata(&mut self) -> crate::Result<FsMetadata> {
        match_file!(self, file => file.fs_metadata().await)
    }

    async fn sec_desc(&mut self, mask: u32) -> crate::Result<SecDesc> {
        match_file!(self, file => file.sec_desc(mask).await)
    }

    async fn set_sec_desc(&mut self, sec_desc: &SecDesc) -> crate::Result<()> {
        match_file!(self, file => file.set_sec_desc(sec_desc).await)
    }

    async fn xattrs(&mut self, namespace: XattrNamespace<'_>) -> crate::Result<Vec<XattrEntry>> {
        match_file!(self, file => file.xattrs(namespace).await)
    }

    async fn xattr(&mut self, name: &str, namespace: Option<&str>) -> crate::Result<Vec<u8>> {
        match_file!(self, file => file.xattr(name, namespace).await)
    }

    async fn streams(&mut self) -> crate::Result<Vec<StreamEntry>> {
        match_file!(self, file => file.streams().await)
    }

    async fn set_xattr(
        &mut self,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> crate::Result<()> {
        match_file!(self, file => file.set_xattr(name, namespace, value).await)
    }

    async fn remove_xattr(&mut self, name: &str, namespace: Option<&str>) -> crate::Result<()> {
        match_file!(self, file => file.remove_xattr(name, namespace).await)
    }

    async fn try_into_std(self) -> std::result::Result<std::fs::File, Self> {
        match self {
            Self::Client(file) => file.try_into_std().await.map_err(Self::Client),
            Self::Direct(file) => file.try_into_std().await.map_err(Self::Direct),
        }
    }
}

pub enum AnyOpenOptions<'a> {
    Client(client::OpenOptions<'a>),
    Direct(DirectOpenOptions),
}

impl OpenOptions for AnyOpenOptions<'_> {
    type File = AnyFile;

    fn read(&mut self, read: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.read(read);
            }
            Self::Direct(opts) => {
                opts.read(read);
            }
        }
        self
    }

    fn write(&mut self, write: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.write(write);
            }
            Self::Direct(opts) => {
                opts.write(write);
            }
        }
        self
    }

    fn append(&mut self, append: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.append(append);
            }
            Self::Direct(opts) => {
                opts.append(append);
            }
        }
        self
    }

    fn create(&mut self, create: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.create(create);
            }
            Self::Direct(opts) => {
                opts.create(create);
            }
        }
        self
    }

    fn create_new(&mut self, create_new: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.create_new(create_new);
            }
            Self::Direct(opts) => {
                opts.create_new(create_new);
            }
        }
        self
    }

    fn truncate(&mut self, truncate: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.truncate(truncate);
            }
            Self::Direct(opts) => {
                opts.truncate(truncate);
            }
        }
        self
    }

    fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.no_follow(no_follow);
            }
            Self::Direct(opts) => {
                opts.no_follow(no_follow);
            }
        }
        self
    }

    async fn open(&self, path: Utf8TypedPath<'_>) -> crate::Result<AnyFile> {
        match self {
            Self::Client(opts) => OpenOptions::open(opts, path).await.map(AnyFile::Client),
            Self::Direct(opts) => OpenOptions::open(opts, path).await.map(AnyFile::Direct),
        }
    }
}

pub enum AnyCommand<'a> {
    Client(client::CommandBuilder<'a>),
    Direct(direct::DirectCommand<'a>),
}

pub enum AnyChild {
    Client(client::ClientChild),
    Direct(Box<direct::DirectChild>),
}

impl Child for AnyChild {
    async fn wait(&mut self) -> crate::Result<ProcessStatus> {
        match self {
            Self::Client(child) => child.wait().await,
            Self::Direct(child) => child.wait().await,
        }
    }

    async fn terminate(self) -> crate::Result<ProcessStatus> {
        match self {
            Self::Client(child) => child.terminate().await,
            Self::Direct(child) => (*child).terminate().await,
        }
    }
}

impl<'a> Command for AnyCommand<'a> {
    type Child = AnyChild;
    type StdioSend = StdioSend;
    type StdioRecv = StdioRecv;

    fn arg(&mut self, arg: &str) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.arg(arg);
            }
            Self::Direct(builder) => {
                builder.arg(arg);
            }
        }
        self
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.env(key, val);
            }
            Self::Direct(builder) => {
                builder.env(key, val);
            }
        }
        self
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.env_remove(key);
            }
            Self::Direct(builder) => {
                builder.env_remove(key);
            }
        }
        self
    }

    fn current_dir(&mut self, dir: Utf8TypedPath<'_>) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.current_dir(dir);
            }
            Self::Direct(builder) => {
                builder.current_dir(dir);
            }
        }
        self
    }

    fn stdin(&mut self, stdio: StdioRecv) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stdin(stdio)?;
            }
            Self::Direct(builder) => {
                builder.stdin(stdio)?;
            }
        }
        Ok(self)
    }

    fn stdout(&mut self, stdio: StdioSend) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stdout(stdio)?;
            }
            Self::Direct(builder) => {
                builder.stdout(stdio)?;
            }
        }
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stdin_inherit()?;
            }
            Self::Direct(builder) => {
                builder.stdin_inherit()?;
            }
        }
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stdout_inherit()?;
            }
            Self::Direct(builder) => {
                builder.stdout_inherit()?;
            }
        }
        Ok(self)
    }

    fn stdin_null(&mut self) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stdin_null();
            }
            Self::Direct(builder) => {
                builder.stdin_null();
            }
        }
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stdout_null();
            }
            Self::Direct(builder) => {
                builder.stdout_null();
            }
        }
        self
    }

    fn stderr(&mut self, stdio: StdioSend) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stderr(stdio)?;
            }
            Self::Direct(builder) => {
                builder.stderr(stdio)?;
            }
        }
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stderr_inherit()?;
            }
            Self::Direct(builder) => {
                builder.stderr_inherit()?;
            }
        }
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stderr_inherit_stdout()?;
            }
            Self::Direct(builder) => {
                builder.stderr_inherit_stdout()?;
            }
        }
        Ok(self)
    }

    fn stderr_null(&mut self) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stderr_null();
            }
            Self::Direct(builder) => {
                builder.stderr_null();
            }
        }
        self
    }

    async fn spawn(self) -> crate::Result<Self::Child> {
        match self {
            Self::Client(builder) => builder.spawn().await.map(AnyChild::Client),
            Self::Direct(builder) => builder.spawn().await.map(Box::new).map(AnyChild::Direct),
        }
    }
}

#[derive(Clone)]
pub enum AnyVfs {
    Client(client::Client),
    Direct(Direct),
}

impl Default for AnyVfs {
    fn default() -> Self {
        Self::Direct(Direct::default())
    }
}

impl From<client::Client> for AnyVfs {
    fn from(value: client::Client) -> Self {
        Self::Client(value)
    }
}

impl From<Direct> for AnyVfs {
    fn from(value: Direct) -> Self {
        Self::Direct(value)
    }
}

impl AnyVfs {
    pub fn as_client(&self) -> Option<&client::Client> {
        match self {
            Self::Client(client) => Some(client),
            Self::Direct(_) => None,
        }
    }

    pub fn into_client(self) -> Option<client::Client> {
        match self {
            Self::Client(client) => Some(client),
            Self::Direct(_) => None,
        }
    }
}

impl Vfs for AnyVfs {
    type File = AnyFile;
    type StdioSend = StdioSend;
    type StdioRecv = StdioRecv;
    type OpenOptions<'a>
        = AnyOpenOptions<'a>
    where
        Self: 'a;
    type Command<'a>
        = AnyCommand<'a>
    where
        Self: 'a;

    fn open_options(&self) -> Self::OpenOptions<'_> {
        match self {
            Self::Client(client) => AnyOpenOptions::Client(client.open_options()),
            Self::Direct(direct) => AnyOpenOptions::Direct(direct.open_options()),
        }
    }

    fn command(&self, program: Utf8TypedPath<'_>) -> Self::Command<'_> {
        match self {
            Self::Client(client) => AnyCommand::Client(client.command(program)),
            Self::Direct(direct) => AnyCommand::Direct(direct.command(program)),
        }
    }

    async fn pipe(&self) -> crate::Result<(StdioSend, StdioRecv)> {
        match self {
            Self::Client(client) => client.pipe().await,
            Self::Direct(direct) => direct.pipe().await,
        }
    }

    async fn query(&self) -> crate::Result<Query> {
        match self {
            Self::Client(client) => client.query().await,
            Self::Direct(direct) => direct.query().await,
        }
    }

    async fn user_name(&self, uid: u32) -> crate::Result<String> {
        match self {
            Self::Client(client) => client.user_name(uid).await,
            Self::Direct(direct) => direct.user_name(uid).await,
        }
    }

    async fn user_id(&self, name: &str) -> crate::Result<u32> {
        match self {
            Self::Client(client) => client.user_id(name).await,
            Self::Direct(direct) => direct.user_id(name).await,
        }
    }

    async fn group_name(&self, gid: u32) -> crate::Result<String> {
        match self {
            Self::Client(client) => client.group_name(gid).await,
            Self::Direct(direct) => direct.group_name(gid).await,
        }
    }

    async fn group_id(&self, name: &str) -> crate::Result<u32> {
        match self {
            Self::Client(client) => client.group_id(name).await,
            Self::Direct(direct) => direct.group_id(name).await,
        }
    }

    async fn sid_name(&self, sid: &Sid) -> crate::Result<SidName> {
        match self {
            Self::Client(client) => client.sid_name(sid).await,
            Self::Direct(direct) => direct.sid_name(sid).await,
        }
    }

    async fn account_name(&self, name: &str) -> crate::Result<SidName> {
        match self {
            Self::Client(client) => client.account_name(name).await,
            Self::Direct(direct) => direct.account_name(name).await,
        }
    }

    async fn read_dir(&self, path: Utf8TypedPath<'_>) -> crate::Result<ReadDir> {
        match self {
            Self::Client(client) => client.read_dir(path).await,
            Self::Direct(direct) => direct.read_dir(path).await,
        }
    }

    async fn which(
        &self,
        program: Utf8TypedPath<'_>,
        path: Option<&str>,
        cwd: Option<Utf8TypedPath<'_>>,
    ) -> crate::Result<Option<Utf8TypedPathBuf>> {
        match self {
            Self::Client(client) => Vfs::which(client, program, path, cwd).await,
            Self::Direct(direct) => Vfs::which(direct, program, path, cwd).await,
        }
    }

    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> crate::Result<Utf8TypedPathBuf> {
        match self {
            Self::Client(client) => Vfs::well_known_path(client, key, env).await,
            Self::Direct(direct) => Vfs::well_known_path(direct, key, env).await,
        }
    }

    async fn clear_cache(&self) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.clear_cache().await,
            Self::Direct(direct) => direct.clear_cache().await,
        }
    }

    async fn xattrs(
        &self,
        path: Utf8TypedPath<'_>,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> crate::Result<Vec<XattrEntry>> {
        match self {
            Self::Client(client) => client.xattrs(path, namespace, follow).await,
            Self::Direct(direct) => direct.xattrs(path, namespace, follow).await,
        }
    }

    async fn streams(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> crate::Result<Vec<StreamEntry>> {
        match self {
            Self::Client(client) => client.streams(path, follow).await,
            Self::Direct(direct) => direct.streams(path, follow).await,
        }
    }

    async fn xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> crate::Result<Vec<u8>> {
        match self {
            Self::Client(client) => client.xattr(path, name, namespace, follow).await,
            Self::Direct(direct) => direct.xattr(path, name, namespace, follow).await,
        }
    }

    async fn set_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.set_xattr(path, name, namespace, value, follow).await,
            Self::Direct(direct) => direct.set_xattr(path, name, namespace, value, follow).await,
        }
    }

    async fn remove_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.remove_xattr(path, name, namespace, follow).await,
            Self::Direct(direct) => direct.remove_xattr(path, name, namespace, follow).await,
        }
    }

    async fn remove(&self, path: Utf8TypedPath<'_>, all: bool, ignore: bool) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.remove(path, all, ignore).await,
            Self::Direct(direct) => direct.remove(path, all, ignore).await,
        }
    }

    async fn metadata(&self, path: Utf8TypedPath<'_>) -> crate::Result<Metadata> {
        match self {
            Self::Client(client) => client.metadata(path).await,
            Self::Direct(direct) => direct.metadata(path).await,
        }
    }

    async fn fs_metadata(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> crate::Result<FsMetadata> {
        match self {
            Self::Client(client) => client.fs_metadata(path, follow).await,
            Self::Direct(direct) => direct.fs_metadata(path, follow).await,
        }
    }

    async fn sec_desc(
        &self,
        path: Utf8TypedPath<'_>,
        mask: u32,
        follow: bool,
    ) -> crate::Result<SecDesc> {
        match self {
            Self::Client(client) => client.sec_desc(path, mask, follow).await,
            Self::Direct(direct) => direct.sec_desc(path, mask, follow).await,
        }
    }

    async fn set_sec_desc(
        &self,
        path: Utf8TypedPath<'_>,
        sec_desc: &SecDesc,
        follow: bool,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.set_sec_desc(path, sec_desc, follow).await,
            Self::Direct(direct) => direct.set_sec_desc(path, sec_desc, follow).await,
        }
    }

    async fn create_dir(&self, path: Utf8TypedPath<'_>, all: bool) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.create_dir(path, all).await,
            Self::Direct(direct) => direct.create_dir(path, all).await,
        }
    }

    async fn remove_dir(
        &self,
        path: Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.remove_dir(path, all, ignore).await,
            Self::Direct(direct) => direct.remove_dir(path, all, ignore).await,
        }
    }

    async fn copy(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.copy(from, to, all).await,
            Self::Direct(direct) => direct.copy(from, to, all).await,
        }
    }

    async fn rename(&self, from: Utf8TypedPath<'_>, to: Utf8TypedPath<'_>) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.rename(from, to).await,
            Self::Direct(direct) => direct.rename(from, to).await,
        }
    }

    async fn move_(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.move_(from, to, all).await,
            Self::Direct(direct) => direct.move_(from, to, all).await,
        }
    }

    async fn symlink(
        &self,
        cwd: Utf8TypedPath<'_>,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.symlink(cwd, src, dst).await,
            Self::Direct(direct) => direct.symlink(cwd, src, dst).await,
        }
    }

    async fn hard_link(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.hard_link(src, dst).await,
            Self::Direct(direct) => direct.hard_link(src, dst).await,
        }
    }

    async fn symlink_dir(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.symlink_dir(src, dst).await,
            Self::Direct(direct) => direct.symlink_dir(src, dst).await,
        }
    }

    async fn symlink_file(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.symlink_file(src, dst).await,
            Self::Direct(direct) => direct.symlink_file(src, dst).await,
        }
    }

    async fn symlink_metadata(&self, path: Utf8TypedPath<'_>) -> crate::Result<Metadata> {
        match self {
            Self::Client(client) => client.symlink_metadata(path).await,
            Self::Direct(direct) => direct.symlink_metadata(path).await,
        }
    }

    async fn attrs(&self, path: Utf8TypedPath<'_>, follow: bool) -> crate::Result<Attrs> {
        match self {
            Self::Client(client) => client.attrs(path, follow).await,
            Self::Direct(direct) => direct.attrs(path, follow).await,
        }
    }

    async fn set_attrs(&self, path: Utf8TypedPath<'_>, attrs: Attrs) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.set_attrs(path, attrs).await,
            Self::Direct(direct) => direct.set_attrs(path, attrs).await,
        }
    }

    async fn canonicalize(&self, path: Utf8TypedPath<'_>) -> crate::Result<Utf8TypedPathBuf> {
        match self {
            Self::Client(client) => client.canonicalize(path).await,
            Self::Direct(direct) => direct.canonicalize(path).await,
        }
    }

    async fn read_link(&self, path: Utf8TypedPath<'_>) -> crate::Result<Utf8TypedPathBuf> {
        match self {
            Self::Client(client) => client.read_link(path).await,
            Self::Direct(direct) => direct.read_link(path).await,
        }
    }

    async fn glob(
        &self,
        pattern: impl Into<String>,
        root: Utf8TypedPath<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> crate::Result<Vec<Utf8TypedPathBuf>> {
        let pattern = pattern.into();

        match self {
            Self::Client(client) => client.glob(pattern, root, follow_symlinks, max_depth).await,
            Self::Direct(direct) => direct.glob(pattern, root, follow_symlinks, max_depth).await,
        }
    }

    async fn set_permissions(
        &self,
        path: Utf8TypedPath<'_>,
        perm: Permissions,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.set_permissions(path, perm).await,
            Self::Direct(direct) => direct.set_permissions(path, perm).await,
        }
    }

    async fn set_times(
        &self,
        path: Utf8TypedPath<'_>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.set_times(path, accessed, modified, created).await,
            Self::Direct(direct) => direct.set_times(path, accessed, modified, created).await,
        }
    }

    async fn chown(
        &self,
        path: Utf8TypedPath<'_>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> crate::Result<()> {
        match self {
            Self::Client(client) => client.chown(path, user, group, follow).await,
            Self::Direct(direct) => direct.chown(path, user, group, follow).await,
        }
    }
}

/// Client for connecting to the agent daemon and spawning processes.
pub use client::Client;
/// Builder for constructing spawn requests.
pub use client::CommandBuilder;
/// Agent server for VFS RPC connections.
pub use server::Server;

/// Runs one VFS server session over standard input and output.
pub fn serve_stdio() -> io::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            Server::new_split(tokio::io::stdin(), tokio::io::stdout())
                .serve()
                .await
        })
}

#[cfg(unix)]
mod unix {
    /// Daemonization errors.
    pub use crate::service::ServiceError;
    /// Run the agent server in foreground mode (no daemonization).
    pub use crate::service::foreground;
    /// Access permission flags for the `access` method.
    pub use nix::unistd::AccessFlags;
}

#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
pub use windows::{WindowsSession, serve_named_pipe};
