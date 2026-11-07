//! File and filesystem metadata returned by a [`crate::Vfs`].

use dolang_winterop::security::Sid;
use serde::{Deserialize, Serialize};

use crate::security::{OwnershipIdentity, Permission};

/// How far path resolution goes before stopping at the final component.
///
/// The levels are ordered: each one opens the final component as itself for
/// everything the previous level covers, and more. As a recursion boundary
/// (`stop`), a level means "don't descend into anything this level would open
/// as itself".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum Resolve {
    /// Follow every link, including at the final component.
    #[default]
    Target,
    /// Open a final symbolic link as itself.
    Link,
    /// Also open an explicit mount point as itself: a junction or volume mount
    /// point on Windows, a mount root on Unix.
    Mount,
    /// Also open any crossing point as itself: any reparse point on Windows, or
    /// a directory on a different device from its parent on Unix.
    Cross,
}

impl Resolve {
    /// Returns whether a final symbolic link is followed.
    pub fn follows(self) -> bool {
        self == Self::Target
    }

    /// Returns the first level that opens a Windows reparse point with `tag`
    /// as itself.
    #[cfg(windows)]
    pub(crate) fn of_reparse_tag(tag: u32) -> Self {
        const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
        const IO_REPARSE_TAG_LX_SYMLINK: u32 = 0xA000_001D;
        // `IsReparseTagNameSurrogate`: the tag names another entity in the
        // namespace, as a junction or volume mount point does
        const NAME_SURROGATE: u32 = 0x2000_0000;
        match tag {
            IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_LX_SYMLINK => Self::Link,
            tag if tag & NAME_SURROGATE != 0 => Self::Mount,
            _ => Self::Cross,
        }
    }
}

/// The kind of filesystem object described by [`Metadata`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileType {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link.
    Symlink,
    /// FIFO or named pipe.
    Fifo,
    /// Character device.
    CharacterDevice,
    /// Block device.
    BlockDevice,
    /// Unix-domain socket.
    Socket,
    /// Windows junction or volume mount point, opened as itself.
    Junction,
    /// Any other Windows reparse point, opened as itself.
    Reparse,
    /// Unrecognized file type.
    Unknown,
}

bitflags::bitflags! {
    /// Portable Unix-style mode bits: permissions, the setuid/setgid/sticky
    /// bits, and the `S_IFMT` file-type nibble.
    ///
    /// The file-type bits are retained for inspection/round-tripping but are
    /// not decoded structurally here -- [`FileType`] is the source of truth
    /// for a filesystem object's kind, since the type nibble is fragile and
    /// platform-dependent.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct Mode: u32 {
        const SET_UID = 0o4000;
        const SET_GID = 0o2000;
        const STICKY = 0o1000;
        const OWNER_READ = 0o400;
        const OWNER_WRITE = 0o200;
        const OWNER_EXECUTE = 0o100;
        const GROUP_READ = 0o40;
        const GROUP_WRITE = 0o20;
        const GROUP_EXECUTE = 0o10;
        const OTHER_READ = 0o4;
        const OTHER_WRITE = 0o2;
        const OTHER_EXECUTE = 0o1;
        const IFIFO = 0o010000;
        const IFCHR = 0o020000;
        const IFDIR = 0o040000;
        const IFBLK = 0o060000;
        const IFREG = 0o100000;
        const IFLNK = 0o120000;
        const IFSOCK = 0o140000;
    }
}

impl Mode {
    /// Projects the owning user's read/write/execute bits.
    pub fn owner(self) -> Permission {
        Permission::from_bits_truncate((self.bits() >> 6) as u8 & 0o7)
    }
    /// Projects the owning group's read/write/execute bits.
    pub fn group(self) -> Permission {
        Permission::from_bits_truncate((self.bits() >> 3) as u8 & 0o7)
    }
    /// Projects other users' read/write/execute bits.
    pub fn other(self) -> Permission {
        Permission::from_bits_truncate(self.bits() as u8 & 0o7)
    }
}

/// Metadata for one filesystem object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// File length in bytes.
    pub(crate) len: u64,
    /// File kind.
    pub(crate) file_type: FileType,
    /// Last-access time in seconds since the Unix epoch.
    pub(crate) atime: i64,
    /// Nanosecond component of [`atime`](Self::atime).
    pub(crate) atime_nsec: i64,
    /// Last-modification time in seconds since the Unix epoch.
    pub(crate) mtime: i64,
    /// Nanosecond component of [`mtime`](Self::mtime).
    pub(crate) mtime_nsec: i64,
    /// Metadata-change time in seconds since the Unix epoch.
    pub(crate) ctime: i64,
    /// Nanosecond component of [`ctime`](Self::ctime).
    pub(crate) ctime_nsec: i64,
    /// Whether the object is a mount point opened as itself.
    pub(crate) is_mount: bool,
    /// Platform-specific metadata.
    pub(crate) family: MetadataFamily,
}

/// Platform-specific metadata payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum MetadataFamily {
    /// Unix metadata.
    Unix(UnixMetadata),
    /// Windows metadata.
    Windows(WindowsMetadata),
}

/// Metadata specific to Unix targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnixMetadata {
    /// File mode bits.
    pub(crate) mode: Mode,
    /// Device ID containing the file.
    pub(crate) dev: u64,
    /// File inode number.
    pub(crate) ino: u64,
    /// Number of hard links.
    pub(crate) nlink: u64,
    /// Owning user ID.
    pub(crate) uid: u32,
    /// Owning group ID.
    pub(crate) gid: u32,
    /// Device ID for special files.
    pub(crate) rdev: u64,
    /// Preferred I/O block size.
    pub(crate) blksize: u64,
    /// Number of allocated blocks.
    pub(crate) blocks: u64,
    pub(crate) platform: UnixMetadataPlatform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum UnixMetadataPlatform {
    FreeBsd { attrs: u32 },
    Linux { attrs: Option<u32> },
    Macos { attrs: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Windows-specific file metadata.
pub struct WindowsMetadata {
    pub(crate) attrs: u32,
    pub(crate) reparse_tag: Option<u32>,
    pub(crate) user: Option<Sid>,
    pub(crate) group: Option<Sid>,
}

impl Metadata {
    /// Returns the file length in bytes.
    pub const fn len(&self) -> u64 {
        self.len
    }
    /// Returns whether the file is empty.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Returns the type of the filesystem object.
    pub const fn file_type(&self) -> FileType {
        self.file_type
    }
    /// Returns whether the object is a mount point that was opened as itself,
    /// rather than the root of the filesystem mounted there.
    pub const fn is_mount(&self) -> bool {
        self.is_mount
    }
    /// Returns the access time in seconds since the Unix epoch.
    pub const fn atime(&self) -> i64 {
        self.atime
    }
    /// Returns the nanosecond component of the access time.
    pub const fn atime_nsec(&self) -> i64 {
        self.atime_nsec
    }
    /// Returns the modification time in seconds since the Unix epoch.
    pub const fn mtime(&self) -> i64 {
        self.mtime
    }
    /// Returns the nanosecond component of the modification time.
    pub const fn mtime_nsec(&self) -> i64 {
        self.mtime_nsec
    }
    /// Returns the metadata-change time in seconds since the Unix epoch.
    pub const fn ctime(&self) -> i64 {
        self.ctime
    }
    /// Returns the nanosecond component of the metadata-change time.
    pub const fn ctime_nsec(&self) -> i64 {
        self.ctime_nsec
    }
    /// Returns the Unix-specific metadata, if present.
    pub fn unix(&self) -> Option<&UnixMetadata> {
        if let MetadataFamily::Unix(metadata) = &self.family {
            Some(metadata)
        } else {
            None
        }
    }
    /// Returns the Windows-specific metadata, if present.
    pub fn windows(&self) -> Option<&WindowsMetadata> {
        if let MetadataFamily::Windows(metadata) = &self.family {
            Some(metadata)
        } else {
            None
        }
    }
    /// Returns the Linux inode attributes, if present and available.
    pub const fn linux_attrs(&self) -> Option<u32> {
        match &self.family {
            MetadataFamily::Unix(UnixMetadata {
                platform: UnixMetadataPlatform::Linux { attrs },
                ..
            }) => *attrs,
            _ => None,
        }
    }
    /// Returns the FreeBSD file flags, if present.
    pub const fn freebsd_attrs(&self) -> Option<u32> {
        match &self.family {
            MetadataFamily::Unix(UnixMetadata {
                platform: UnixMetadataPlatform::FreeBsd { attrs },
                ..
            }) => Some(*attrs),
            _ => None,
        }
    }
    /// Returns the macOS file flags, if present.
    pub const fn macos_attrs(&self) -> Option<u32> {
        match &self.family {
            MetadataFamily::Unix(UnixMetadata {
                platform: UnixMetadataPlatform::Macos { attrs },
                ..
            }) => Some(*attrs),
            _ => None,
        }
    }
    /// Returns the Windows file attributes, if present.
    pub const fn win_attrs(&self) -> Option<u32> {
        match &self.family {
            MetadataFamily::Windows(metadata) => Some(metadata.attrs),
            _ => None,
        }
    }
}

impl UnixMetadata {
    /// Returns the Unix mode bits.
    pub const fn mode(&self) -> Mode {
        self.mode
    }
    /// Returns the device ID.
    pub const fn dev(&self) -> u64 {
        self.dev
    }
    /// Returns the inode number.
    pub const fn ino(&self) -> u64 {
        self.ino
    }
    /// Returns the number of hard links.
    pub const fn nlink(&self) -> u64 {
        self.nlink
    }
    /// Returns the owning user ID.
    pub const fn uid(&self) -> u32 {
        self.uid
    }
    /// Returns the owning group ID.
    pub const fn gid(&self) -> u32 {
        self.gid
    }
    /// Returns the device ID for a special file.
    pub const fn rdev(&self) -> u64 {
        self.rdev
    }
    /// Returns the preferred I/O block size.
    pub const fn block_size(&self) -> u64 {
        self.blksize
    }
    /// Returns the number of allocated blocks.
    pub const fn blocks(&self) -> u64 {
        self.blocks
    }
    /// Returns the Linux inode attributes, if present and available.
    pub const fn linux_attrs(&self) -> Option<u32> {
        match self.platform {
            UnixMetadataPlatform::Linux { attrs } => attrs,
            _ => None,
        }
    }
    /// Returns the FreeBSD file flags, if present.
    pub const fn freebsd_attrs(&self) -> Option<u32> {
        match self.platform {
            UnixMetadataPlatform::FreeBsd { attrs } => Some(attrs),
            _ => None,
        }
    }
    /// Returns the macOS file flags, if present.
    pub const fn macos_attrs(&self) -> Option<u32> {
        match self.platform {
            UnixMetadataPlatform::Macos { attrs } => Some(attrs),
            _ => None,
        }
    }
}

impl WindowsMetadata {
    /// Returns the Windows file attributes.
    pub const fn attrs(&self) -> u32 {
        self.attrs
    }
    /// Returns the reparse tag, if the object is a reparse point.
    pub const fn reparse_tag(&self) -> Option<u32> {
        self.reparse_tag
    }
    /// Returns the owner SID, if it was requested and available.
    pub fn user(&self) -> Option<&Sid> {
        self.user.as_ref()
    }
    /// Returns the group SID, if it was requested and available.
    pub fn group(&self) -> Option<&Sid> {
        self.group.as_ref()
    }
}

/// Capacity and allocation information for a filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsMetadata {
    pub(crate) capacity: u64,
    pub(crate) free: u64,
    pub(crate) available: u64,
    pub(crate) block_size: u32,
    pub(crate) fs_type: Option<String>,
    pub(crate) family: FsMetadataFamily,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum FsMetadataFamily {
    Unix(UnixFsMetadata),
    Windows(WindowsFsMetadata),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Unix-specific filesystem metadata.
pub struct UnixFsMetadata {
    pub(crate) blocks: u64,
    pub(crate) blocks_free: u64,
    pub(crate) blocks_available: u64,
    pub(crate) files: u64,
    pub(crate) files_free: u64,
    pub(crate) files_available: u64,
    pub(crate) fragment_size: u32,
    pub(crate) fsid: Option<u64>,
    pub(crate) name_max: u32,
    pub(crate) platform: UnixFsMetadataPlatform,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum UnixFsMetadataPlatform {
    Linux { flags: u64 },
    Macos { flags: u64 },
    FreeBsd { flags: u64 },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Windows-specific filesystem metadata.
pub struct WindowsFsMetadata {
    pub(crate) flags: u32,
    pub(crate) volume_serial_number: u32,
    pub(crate) component_length_max: u32,
}

impl FsMetadata {
    /// Returns the total filesystem capacity in bytes.
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }
    /// Returns the total free space in bytes.
    pub const fn free(&self) -> u64 {
        self.free
    }
    /// Returns the space available to an unprivileged caller in bytes.
    pub const fn available(&self) -> u64 {
        self.available
    }
    /// Returns the fundamental filesystem block size in bytes.
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }
    /// Returns the filesystem type name as reported by the platform, if known.
    pub fn fs_type(&self) -> Option<&str> {
        self.fs_type.as_deref()
    }
    /// Returns the Unix-specific filesystem metadata, if present.
    pub fn unix(&self) -> Option<&UnixFsMetadata> {
        if let FsMetadataFamily::Unix(metadata) = &self.family {
            Some(metadata)
        } else {
            None
        }
    }
    /// Returns the Windows-specific filesystem metadata, if present.
    pub fn windows(&self) -> Option<&WindowsFsMetadata> {
        if let FsMetadataFamily::Windows(metadata) = &self.family {
            Some(metadata)
        } else {
            None
        }
    }
    /// Returns whether the filesystem is read-only.
    #[allow(clippy::unnecessary_cast)]
    pub fn read_only(&self) -> bool {
        match &self.family {
            FsMetadataFamily::Unix(metadata) => metadata.platform.flags() & 1 != 0,
            FsMetadataFamily::Windows(metadata) => metadata.flags & 0x0008_0000 != 0,
        }
    }
    /// Returns whether set-user-ID and set-group-ID bits are disabled, if known.
    #[allow(clippy::unnecessary_cast)]
    pub fn no_suid(&self) -> Option<bool> {
        match &self.family {
            FsMetadataFamily::Unix(metadata) => Some(metadata.platform.flags() & 2 != 0),
            FsMetadataFamily::Windows(_) => None,
        }
    }
    /// Returns whether execution is disabled, if known.
    #[allow(clippy::unnecessary_cast)]
    pub fn no_exec(&self) -> Option<bool> {
        self.linux_flag(8)
    }
    /// Returns whether writes are synchronous, if known.
    #[allow(clippy::unnecessary_cast)]
    pub fn synchronous(&self) -> Option<bool> {
        self.linux_flag(16)
    }
    /// Returns whether device files are disabled, if known.
    #[allow(clippy::unnecessary_cast)]
    pub fn no_dev(&self) -> Option<bool> {
        self.linux_flag(4)
    }
    /// Returns whether access-time updates are disabled, if known.
    #[allow(clippy::unnecessary_cast)]
    pub fn no_atime(&self) -> Option<bool> {
        self.linux_flag(1024)
    }
    /// Returns whether directory access-time updates are disabled, if known.
    #[allow(clippy::unnecessary_cast)]
    pub fn no_dir_atime(&self) -> Option<bool> {
        self.linux_flag(2048)
    }
    /// Returns whether relative access-time updates are enabled, if known.
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
impl UnixFsMetadata {
    /// Returns the total number of filesystem blocks.
    pub const fn blocks(&self) -> u64 {
        self.blocks
    }
    /// Returns the number of free filesystem blocks.
    pub const fn blocks_free(&self) -> u64 {
        self.blocks_free
    }
    /// Returns the number of blocks available to an unprivileged caller.
    pub const fn blocks_available(&self) -> u64 {
        self.blocks_available
    }
    /// Returns the total number of file nodes.
    pub const fn files(&self) -> u64 {
        self.files
    }
    /// Returns the number of free file nodes.
    pub const fn files_free(&self) -> u64 {
        self.files_free
    }
    /// Returns the number of file nodes available to an unprivileged caller.
    pub const fn files_available(&self) -> u64 {
        self.files_available
    }
    /// Returns the fragment size in bytes.
    pub const fn fragment_size(&self) -> u32 {
        self.fragment_size
    }
    /// Returns the filesystem ID, if available.
    pub const fn fsid(&self) -> Option<u64> {
        self.fsid
    }
    /// Returns the maximum filename length.
    pub const fn name_max(&self) -> u32 {
        self.name_max
    }
    /// Returns the Linux mount flags, if present.
    pub const fn linux_flags(&self) -> Option<u64> {
        match self.platform {
            UnixFsMetadataPlatform::Linux { flags } => Some(flags),
            _ => None,
        }
    }
    /// Returns the FreeBSD mount flags, if present.
    pub const fn freebsd_flags(&self) -> Option<u64> {
        match self.platform {
            UnixFsMetadataPlatform::FreeBsd { flags } => Some(flags),
            _ => None,
        }
    }
    /// Returns the macOS mount flags, if present.
    pub const fn macos_flags(&self) -> Option<u64> {
        match self.platform {
            UnixFsMetadataPlatform::Macos { flags } => Some(flags),
            _ => None,
        }
    }
}
impl WindowsFsMetadata {
    /// Returns the filesystem flags reported by Windows.
    pub const fn flags(&self) -> u32 {
        self.flags
    }
    /// Returns the volume serial number.
    pub const fn volume_serial_number(&self) -> u32 {
        self.volume_serial_number
    }
    /// Returns the maximum filesystem path-component length.
    pub const fn component_length_max(&self) -> u32 {
        self.component_length_max
    }
}
impl UnixFsMetadataPlatform {
    pub fn flags(&self) -> u64 {
        match self {
            Self::FreeBsd { flags } | Self::Linux { flags } | Self::Macos { flags } => *flags,
        }
    }
}

/// Portable filesystem attribute flags.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttrFlags(u64);
impl AttrFlags {
    /// Read-only.
    pub const READONLY: Self = Self(1 << 0);
    /// Hidden from ordinary directory listings.
    pub const HIDDEN: Self = Self(1 << 1);
    /// Used by the operating system.
    pub const SYSTEM: Self = Self(1 << 2);
    /// Marked for archival.
    pub const ARCHIVE: Self = Self(1 << 3);
    /// Stored in compressed form.
    pub const COMPRESSED: Self = Self(1 << 4);
    /// Intended for temporary storage.
    pub const TEMPORARY: Self = Self(1 << 5);
    /// Data is not immediately available.
    pub const OFFLINE: Self = Self(1 << 6);
    /// Excluded from content indexing.
    pub const NOT_CONTENT_INDEXED: Self = Self(1 << 7);
    /// Cannot be modified.
    pub const IMMUTABLE: Self = Self(1 << 8);
    /// May only be appended to.
    pub const APPEND_ONLY: Self = Self(1 << 9);
    /// Excluded from dump-style backups.
    pub const NO_DUMP: Self = Self(1 << 10);
    /// Does not update access time.
    pub const NO_ATIME: Self = Self(1 << 11);
    /// Disables copy-on-write behavior.
    pub const NO_COPY_ON_WRITE: Self = Self(1 << 12);
    /// Directory changes are written synchronously.
    pub const DIR_SYNC: Self = Self(1 << 13);
    /// Directory uses case-insensitive lookup.
    pub const CASEFOLD: Self = Self(1 << 14);
    /// File data is journaled.
    pub const DATA_JOURNALING: Self = Self(1 << 15);
    /// File must not be compressed.
    pub const NO_COMPRESS: Self = Self(1 << 16);
    /// New children inherit the project ID.
    pub const PROJECT_INHERIT: Self = Self(1 << 17);
    /// Requests secure deletion.
    pub const SECURE_DELETE: Self = Self(1 << 18);
    /// Changes are written synchronously.
    pub const SYNC: Self = Self(1 << 19);
    /// Disables tail merging.
    pub const NO_TAIL_MERGE: Self = Self(1 << 20);
    /// Directory is the top of a hierarchy.
    pub const TOP_DIR: Self = Self(1 << 21);
    /// File can be recovered after deletion.
    pub const UNDELETE: Self = Self(1 << 22);
    /// Supports direct-access storage.
    pub const DIRECT_ACCESS: Self = Self(1 << 23);
    /// Uses extent-based storage.
    pub const EXTENT_FORMAT: Self = Self(1 << 24);
    /// Directory is opaque to union mounts.
    pub const OPAQUE: Self = Self(1 << 25);
    /// File uses sparse allocation.
    pub const SPARSE: Self = Self(1 << 26);
    /// Returns an empty set of flags.
    pub const fn empty() -> Self {
        Self(0)
    }
    /// Returns whether all bits in `flag` are set.
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
    /// Returns whether any bits in `other` are set.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
    /// Returns the union of two flag sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// Returns these flags with the bits in `other` removed.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
    /// Returns whether no flags are set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttrsPatch {
    pub(crate) set: AttrFlags,
    pub(crate) clear: AttrFlags,
}
impl AttrsPatch {
    pub fn update(&mut self, flag: AttrFlags, value: Option<bool>) {
        match value {
            Some(true) => {
                self.set = self.set.union(flag);
                self.clear = self.clear.difference(flag);
            }
            Some(false) => {
                self.clear = self.clear.union(flag);
                self.set = self.set.difference(flag);
            }
            None => {}
        }
    }
    pub const fn requested(self) -> AttrFlags {
        self.set.union(self.clear)
    }
    pub const fn is_empty(self) -> bool {
        self.set.is_empty() && self.clear.is_empty()
    }
}

/// Requested changes to a filesystem object's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataPatch {
    pub(crate) mode: Option<Mode>,
    pub(crate) user: Option<OwnershipIdentity>,
    pub(crate) group: Option<OwnershipIdentity>,
    pub(crate) accessed: Option<i128>,
    pub(crate) modified: Option<i128>,
    pub(crate) created: Option<i128>,
    pub(crate) attrs: AttrsPatch,
    pub(crate) resolve: Resolve,
}
impl Default for MetadataPatch {
    fn default() -> Self {
        Self {
            mode: None,
            user: None,
            group: None,
            accessed: None,
            modified: None,
            created: None,
            attrs: AttrsPatch::default(),
            resolve: Resolve::Target,
        }
    }
}
impl MetadataPatch {
    /// Creates an empty metadata patch that follows symbolic links.
    pub fn new() -> Self {
        Self::default()
    }
    /// Sets the replacement Unix mode.
    pub fn mode(&mut self, mode: Mode) -> &mut Self {
        self.mode = Some(mode);
        self
    }
    /// Sets the replacement owner.
    pub fn user(&mut self, user: OwnershipIdentity) -> &mut Self {
        self.user = Some(user);
        self
    }
    /// Sets the replacement group.
    pub fn group(&mut self, group: OwnershipIdentity) -> &mut Self {
        self.group = Some(group);
        self
    }
    /// Sets the access time in nanoseconds since the Unix epoch.
    pub fn accessed(&mut self, accessed: i128) -> &mut Self {
        self.accessed = Some(accessed);
        self
    }
    /// Sets the modification time in nanoseconds since the Unix epoch.
    pub fn modified(&mut self, modified: i128) -> &mut Self {
        self.modified = Some(modified);
        self
    }
    /// Sets the creation time in nanoseconds since the Unix epoch.
    pub fn created(&mut self, created: i128) -> &mut Self {
        self.created = Some(created);
        self
    }
    /// Requests that an attribute be set, cleared, or left unchanged.
    pub fn attribute(&mut self, flag: AttrFlags, value: Option<bool>) -> &mut Self {
        self.attrs.update(flag, value);
        self
    }
    /// Selects how the final path component is resolved.
    pub fn resolve(&mut self, resolve: Resolve) -> &mut Self {
        self.resolve = resolve;
        self
    }
    /// Sets the replacement Unix mode and returns the patch.
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode(mode);
        self
    }
    /// Sets the replacement owner and returns the patch.
    pub fn with_user(mut self, user: OwnershipIdentity) -> Self {
        self.user(user);
        self
    }
    /// Sets the replacement group and returns the patch.
    pub fn with_group(mut self, group: OwnershipIdentity) -> Self {
        self.group(group);
        self
    }
    /// Sets the access time and returns the patch.
    pub fn with_accessed(mut self, accessed: i128) -> Self {
        self.accessed(accessed);
        self
    }
    /// Sets the modification time and returns the patch.
    pub fn with_modified(mut self, modified: i128) -> Self {
        self.modified(modified);
        self
    }
    /// Sets the creation time and returns the patch.
    pub fn with_created(mut self, created: i128) -> Self {
        self.created(created);
        self
    }
    /// Requests an attribute change and returns the patch.
    pub fn with_attribute(mut self, flag: AttrFlags, value: Option<bool>) -> Self {
        self.attribute(flag, value);
        self
    }
    /// Selects how the final path component is resolved and returns the
    /// patch.
    pub fn with_resolve(mut self, resolve: Resolve) -> Self {
        self.resolve(resolve);
        self
    }
    /// Returns whether the patch requests no metadata changes.
    pub fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.user.is_none()
            && self.group.is_none()
            && self.accessed.is_none()
            && self.modified.is_none()
            && self.created.is_none()
            && self.attrs.is_empty()
    }
}

pub(crate) fn metadata_from_std(metadata: std::fs::Metadata) -> Metadata {
    #[cfg(unix)]
    {
        use nix::sys::stat::{SFlag, mode_t};
        #[cfg(target_os = "macos")]
        use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
        #[cfg(target_os = "freebsd")]
        use std::os::freebsd::fs::MetadataExt as FreeBsdMetadataExt;
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode();
        let file_type = match SFlag::from_bits_truncate(mode as mode_t) & SFlag::S_IFMT {
            SFlag::S_IFREG => FileType::File,
            SFlag::S_IFDIR => FileType::Dir,
            SFlag::S_IFLNK => FileType::Symlink,
            SFlag::S_IFIFO => FileType::Fifo,
            SFlag::S_IFCHR => FileType::CharacterDevice,
            SFlag::S_IFBLK => FileType::BlockDevice,
            SFlag::S_IFSOCK => FileType::Socket,
            _ => FileType::Unknown,
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
            is_mount: false,
            family: MetadataFamily::Unix(UnixMetadata {
                mode: Mode::from_bits_retain(mode),
                dev: metadata.dev(),
                ino: metadata.ino(),
                nlink: metadata.nlink(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                rdev: metadata.rdev(),
                blksize: metadata.blksize(),
                blocks: metadata.blocks(),
                #[cfg(target_os = "linux")]
                platform: UnixMetadataPlatform::Linux { attrs: None },
                #[cfg(target_os = "freebsd")]
                platform: UnixMetadataPlatform::FreeBsd {
                    attrs: metadata.st_flags(),
                },
                #[cfg(target_os = "macos")]
                platform: UnixMetadataPlatform::Macos {
                    attrs: metadata.st_flags(),
                },
            }),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let file_type = if metadata.is_file() {
            FileType::File
        } else if metadata.is_dir() {
            FileType::Dir
        } else if metadata.file_type().is_symlink() {
            FileType::Symlink
        } else {
            FileType::Unknown
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
            is_mount: false,
            family: MetadataFamily::Windows(WindowsMetadata {
                attrs: metadata.file_attributes(),
                reparse_tag: None,
                user: None,
                group: None,
            }),
        }
    }
}
/// Records the reparse tag of the object `metadata` describes, which was
/// opened as itself if it has one, and derives the file type from it rather
/// than from std, which reports every name surrogate, junctions included, as a
/// symlink.
#[cfg(windows)]
pub(crate) fn metadata_with_reparse_tag(mut metadata: Metadata, tag: Option<u32>) -> Metadata {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    let MetadataFamily::Windows(windows) = &mut metadata.family else {
        unreachable!()
    };
    windows.reparse_tag = tag;
    let level = tag.map(Resolve::of_reparse_tag);
    metadata.file_type = match (tag, level) {
        (_, Some(Resolve::Link)) => FileType::Symlink,
        (Some(IO_REPARSE_TAG_MOUNT_POINT), _) => FileType::Junction,
        (Some(_), _) => FileType::Reparse,
        (None, _) if windows.attrs & FILE_ATTRIBUTE_DIRECTORY != 0 => FileType::Dir,
        (None, _) => FileType::File,
    };
    metadata.is_mount = level == Some(Resolve::Mount);
    metadata
}
#[cfg(windows)]
pub(crate) fn metadata_with_sids(
    mut metadata: Metadata,
    user: Option<Sid>,
    group: Option<Sid>,
) -> Metadata {
    let MetadataFamily::Windows(windows) = &mut metadata.family else {
        unreachable!()
    };
    windows.user = user;
    windows.group = group;
    metadata
}
#[cfg(windows)]
fn system_time_to_parts(time: Option<std::time::SystemTime>) -> (i64, u32) {
    use std::time::UNIX_EPOCH;
    let Some(time) = time else { return (0, 0) };
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

#[cfg(test)]
mod tests {
    use super::{AttrFlags, MetadataPatch, Mode, Resolve};
    use crate::security::OwnershipIdentity;

    #[test]
    fn metadata_patch_builder_tracks_requested_changes() {
        let patch = MetadataPatch::new()
            .with_mode(Mode::OWNER_READ)
            .with_user(OwnershipIdentity::Id(1))
            .with_group(OwnershipIdentity::Name("staff".to_owned()))
            .with_accessed(10)
            .with_modified(20)
            .with_created(30)
            .with_attribute(AttrFlags::HIDDEN, Some(true))
            .with_resolve(Resolve::Link);

        assert!(!patch.is_empty());
        assert_eq!(patch.mode, Some(Mode::OWNER_READ));
        assert!(patch.attrs.set.contains(AttrFlags::HIDDEN));
        assert_eq!(patch.resolve, Resolve::Link);
        assert_eq!(
            postcard::from_bytes::<MetadataPatch>(&postcard::to_stdvec(&patch).unwrap()).unwrap(),
            patch
        );
    }

    #[test]
    fn metadata_patch_attribute_updates_are_disjoint() {
        let mut patch = MetadataPatch::new();
        patch.attribute(AttrFlags::READONLY, Some(true));
        patch.attribute(AttrFlags::READONLY, Some(false));
        assert!(!patch.attrs.set.contains(AttrFlags::READONLY));
        assert!(patch.attrs.clear.contains(AttrFlags::READONLY));
    }
}
