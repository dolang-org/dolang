use dolang::runtime::{
    Arg, Error, Output, Result, Slot, State, Strand, call, method, unpack, vm::Builder,
};
use std::{io, io::ErrorKind, path::PathBuf, time};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};

use rand::RngExt;
use rand::distr::Alphanumeric;

pub(crate) mod file;
pub(crate) mod glob;
pub(crate) mod path;
pub(crate) mod readdir;

use crate::{
    error::{ErrorExt as _, ResultExt as _},
    fs::{
        file::File,
        path::{Path, PathAnnex, PathOrStr, normalize_path},
        readdir::{DirEntryIter, DirEntryIterAnnex},
    },
    global::Global,
    time::{create_datetime_with_global, datetime_to_system_time},
};

use glob::{GlobIter, GlobIterAnnex};

/// Trait abstracting over OpenOptions implementations.
pub(crate) trait OpenOptionsLike {
    fn read(&mut self, read: bool) -> &mut Self;
    fn write(&mut self, write: bool) -> &mut Self;
    fn append(&mut self, append: bool) -> &mut Self;
    fn create(&mut self, create: bool) -> &mut Self;
    fn truncate(&mut self, truncate: bool) -> &mut Self;
    async fn open(&mut self, path: &std::path::Path) -> io::Result<fs::File>;
}

/// Trait abstracting over file type types.
pub(crate) trait FileTypeLike {
    fn is_file(&self) -> bool;
    fn is_dir(&self) -> bool;
    fn is_symlink(&self) -> bool;
}

/// Trait abstracting over metadata types.
pub(crate) trait MetadataLike {
    fn len(&self) -> u64;
    fn file_type(&self) -> impl FileTypeLike;
    fn modified(&self) -> io::Result<time::SystemTime>;
    fn accessed(&self) -> io::Result<time::SystemTime>;
    fn created(&self) -> io::Result<time::SystemTime>;
    fn is_file(&self) -> bool {
        self.file_type().is_file()
    }
    fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }
    fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }
}

impl OpenOptionsLike for fs::OpenOptions {
    fn read(&mut self, read: bool) -> &mut Self {
        fs::OpenOptions::read(self, read);
        self
    }

    fn write(&mut self, write: bool) -> &mut Self {
        fs::OpenOptions::write(self, write);
        self
    }

    fn append(&mut self, append: bool) -> &mut Self {
        fs::OpenOptions::append(self, append);
        self
    }

    fn create(&mut self, create: bool) -> &mut Self {
        fs::OpenOptions::create(self, create);
        self
    }

    fn truncate(&mut self, truncate: bool) -> &mut Self {
        fs::OpenOptions::truncate(self, truncate);
        self
    }

    async fn open(&mut self, path: &std::path::Path) -> io::Result<fs::File> {
        fs::OpenOptions::open(self, path).await
    }
}

#[cfg(unix)]
pub(crate) mod unix {
    use super::*;

    use dolang::runtime::Value;
    use dolang_shell_vfs as agent;
    use nix::{
        errno::Errno,
        fcntl::AT_FDCWD,
        sys::{
            stat::{UtimensatFlags, utimensat},
            time::{TimeSpec, TimeValLike},
        },
        unistd::{Gid, Group, Uid, User, chown},
    };
    use std::os::unix::fs::MetadataExt;
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    fn system_time_from_unix_parts(secs: i64, nanos: i64) -> io::Result<time::SystemTime> {
        let carry = nanos.div_euclid(1_000_000_000);
        let nanos = nanos.rem_euclid(1_000_000_000);
        let secs = secs
            .checked_add(carry)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid timestamp"))?;

        if secs >= 0 {
            time::UNIX_EPOCH
                .checked_add(time::Duration::new(secs as u64, nanos as u32))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid timestamp"))
        } else {
            let secs_abs = secs.unsigned_abs();
            let duration = if nanos == 0 {
                time::Duration::new(secs_abs, 0)
            } else {
                time::Duration::new(secs_abs - 1, 1_000_000_000u32 - nanos as u32)
            };
            time::UNIX_EPOCH
                .checked_sub(duration)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid timestamp"))
        }
    }

    fn unix_timespec(time: Option<(i64, u32)>) -> io::Result<TimeSpec> {
        match time {
            Some((secs, nanos)) => secs
                .checked_mul(1_000_000_000)
                .and_then(|secs| secs.checked_add(i64::from(nanos)))
                .map(TimeSpec::nanoseconds)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid timestamp")),
            None => Ok(TimeSpec::UTIME_OMIT),
        }
    }

    /// Unix-specific metadata extension trait.
    #[cfg(unix)]
    pub(crate) trait MetadataExtLike {
        fn mode(&self) -> u32;
        fn dev(&self) -> u64;
        fn ino(&self) -> u64;
        fn nlink(&self) -> u64;
        fn uid(&self) -> u32;
        fn gid(&self) -> u32;
        fn rdev(&self) -> u64;
        fn blksize(&self) -> u64;
        fn blocks(&self) -> u64;
        #[expect(dead_code)]
        fn atime(&self) -> i64;
        #[expect(dead_code)]
        fn atime_nsec(&self) -> i64;
        #[expect(dead_code)]
        fn mtime(&self) -> i64;
        #[expect(dead_code)]
        fn mtime_nsec(&self) -> i64;
        #[expect(dead_code)]
        fn ctime(&self) -> i64;
        #[expect(dead_code)]
        fn ctime_nsec(&self) -> i64;
    }

    impl FileTypeLike for agent::FileType {
        fn is_file(&self) -> bool {
            matches!(self, agent::FileType::File)
        }

        fn is_dir(&self) -> bool {
            matches!(self, agent::FileType::Dir)
        }

        fn is_symlink(&self) -> bool {
            matches!(self, agent::FileType::Symlink)
        }
    }

    impl<'a> OpenOptionsLike for agent::OpenOptions<'a> {
        fn read(&mut self, read: bool) -> &mut Self {
            agent::OpenOptions::read(self, read);
            self
        }

        fn write(&mut self, write: bool) -> &mut Self {
            agent::OpenOptions::write(self, write);
            self
        }

        fn append(&mut self, append: bool) -> &mut Self {
            agent::OpenOptions::append(self, append);
            self
        }

        fn create(&mut self, create: bool) -> &mut Self {
            agent::OpenOptions::create(self, create);
            self
        }

        fn truncate(&mut self, truncate: bool) -> &mut Self {
            agent::OpenOptions::truncate(self, truncate);
            self
        }

        async fn open(&mut self, path: &std::path::Path) -> io::Result<fs::File> {
            agent::OpenOptions::open(self, path).await
        }
    }
    impl MetadataLike for agent::Metadata {
        fn len(&self) -> u64 {
            self.len
        }

        fn file_type(&self) -> impl FileTypeLike {
            self.file_type
        }

        fn modified(&self) -> io::Result<time::SystemTime> {
            system_time_from_unix_parts(self.mtime, self.mtime_nsec)
        }

        fn accessed(&self) -> io::Result<time::SystemTime> {
            system_time_from_unix_parts(self.atime, self.atime_nsec)
        }

        fn created(&self) -> io::Result<time::SystemTime> {
            system_time_from_unix_parts(self.ctime, self.ctime_nsec)
        }

        fn is_file(&self) -> bool {
            self.file_type.is_file()
        }

        fn is_dir(&self) -> bool {
            self.file_type.is_dir()
        }

        fn is_symlink(&self) -> bool {
            self.file_type.is_symlink()
        }
    }

    impl MetadataExtLike for std::fs::Metadata {
        fn mode(&self) -> u32 {
            MetadataExt::mode(self)
        }

        fn dev(&self) -> u64 {
            MetadataExt::dev(self)
        }

        fn ino(&self) -> u64 {
            MetadataExt::ino(self)
        }

        fn nlink(&self) -> u64 {
            MetadataExt::nlink(self)
        }

        fn uid(&self) -> u32 {
            MetadataExt::uid(self)
        }

        fn gid(&self) -> u32 {
            MetadataExt::gid(self)
        }

        fn rdev(&self) -> u64 {
            MetadataExt::rdev(self)
        }

        fn blksize(&self) -> u64 {
            MetadataExt::blksize(self)
        }

        fn blocks(&self) -> u64 {
            MetadataExt::blocks(self)
        }

        fn atime(&self) -> i64 {
            MetadataExt::atime(self)
        }

        fn atime_nsec(&self) -> i64 {
            MetadataExt::atime_nsec(self)
        }

        fn mtime(&self) -> i64 {
            MetadataExt::mtime(self)
        }

        fn mtime_nsec(&self) -> i64 {
            MetadataExt::mtime_nsec(self)
        }

        fn ctime(&self) -> i64 {
            MetadataExt::ctime(self)
        }

        fn ctime_nsec(&self) -> i64 {
            MetadataExt::ctime_nsec(self)
        }
    }

    impl MetadataExtLike for agent::Metadata {
        fn mode(&self) -> u32 {
            self.mode
        }

        fn dev(&self) -> u64 {
            self.dev
        }

        fn ino(&self) -> u64 {
            self.ino
        }

        fn nlink(&self) -> u64 {
            self.nlink
        }

        fn uid(&self) -> u32 {
            self.uid
        }

        fn gid(&self) -> u32 {
            self.gid
        }

        fn rdev(&self) -> u64 {
            self.rdev
        }

        fn blksize(&self) -> u64 {
            self.blksize
        }

        fn blocks(&self) -> u64 {
            self.blocks
        }

        fn atime(&self) -> i64 {
            self.atime
        }

        fn atime_nsec(&self) -> i64 {
            self.atime_nsec
        }

        fn mtime(&self) -> i64 {
            self.mtime
        }

        fn mtime_nsec(&self) -> i64 {
            self.mtime_nsec
        }

        fn ctime(&self) -> i64 {
            self.ctime
        }

        fn ctime_nsec(&self) -> i64 {
            self.ctime_nsec
        }
    }

    pub(crate) async fn unix_metadata_to_record<'v, 's>(
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        record: &Value<'v>,
        metadata: &impl MetadataExtLike,
    ) -> Result<'v, 's, ()> {
        use libc::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK};

        let mode = metadata.mode() as libc::mode_t;
        let file_type = match mode & S_IFMT {
            S_IFREG => global.syms.file,
            S_IFDIR => global.syms.dir,
            S_IFLNK => global.syms.symlink,
            S_IFBLK => global.syms.block_device,
            S_IFCHR => global.syms.char_device,
            S_IFIFO => global.syms.fifo,
            S_IFSOCK => global.syms.socket,
            _ => global.syms.unknown,
        };
        record.set(strand, global.syms.ty, file_type)?;
        record.set(strand, global.syms.mode, mode as i64)?;
        record.set(strand, global.syms.dev, metadata.dev() as i64)?;
        record.set(strand, global.syms.ino, metadata.ino() as i64)?;
        record.set(strand, global.syms.nlink, metadata.nlink() as i64)?;
        record.set(strand, global.syms.uid, metadata.uid() as i64)?;
        record.set(strand, global.syms.gid, metadata.gid() as i64)?;
        record.set(strand, global.syms.rdev, metadata.rdev() as i64)?;
        record.set(strand, global.syms.blksize, metadata.blksize() as i64)?;
        record.set(strand, global.syms.blocks, metadata.blocks() as i64)?;
        Ok(())
    }

    fn resolve_user(user: Option<agent::ChownIdentity>) -> std::result::Result<Option<Uid>, Errno> {
        match user {
            None => Ok(None),
            Some(agent::ChownIdentity::Id(id)) => Ok(Some(Uid::from_raw(id))),
            Some(agent::ChownIdentity::Name(name)) => match User::from_name(&name)? {
                Some(user) => Ok(Some(user.uid)),
                None => Err(Errno::ENOENT),
            },
        }
    }

    fn resolve_group(
        group: Option<agent::ChownIdentity>,
    ) -> std::result::Result<Option<Gid>, Errno> {
        match group {
            None => Ok(None),
            Some(agent::ChownIdentity::Id(id)) => Ok(Some(Gid::from_raw(id))),
            Some(agent::ChownIdentity::Name(name)) => match Group::from_name(&name)? {
                Some(group) => Ok(Some(group.gid)),
                None => Err(Errno::ENOENT),
            },
        }
    }

    fn lchown_path(
        path: &std::path::Path,
        user: Option<Uid>,
        group: Option<Gid>,
    ) -> std::result::Result<(), Errno> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)?;
        Errno::result(unsafe {
            libc::lchown(
                path.as_ptr(),
                user.map_or(!0, |user| user.as_raw()) as libc::uid_t,
                group.map_or(!0, |group| group.as_raw()) as libc::gid_t,
            )
        })
        .map(drop)
    }

    pub(crate) async fn chown_path(
        path: PathBuf,
        user: Option<agent::ChownIdentity>,
        group: Option<agent::ChownIdentity>,
        follow: bool,
    ) -> io::Result<()> {
        tokio::task::spawn_blocking(move || {
            let user =
                resolve_user(user).map_err(|err| io::Error::from_raw_os_error(err as i32))?;
            let group =
                resolve_group(group).map_err(|err| io::Error::from_raw_os_error(err as i32))?;
            let result = if follow {
                chown(&path, user, group)
            } else {
                lchown_path(&path, user, group)
            };
            result.map_err(|err| io::Error::from_raw_os_error(err as i32))
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(crate) async fn utime_path(
        path: PathBuf,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
    ) -> io::Result<()> {
        tokio::task::spawn_blocking(move || {
            let atime = unix_timespec(accessed)?;
            let mtime = unix_timespec(modified)?;
            utimensat(
                AT_FDCWD,
                &path,
                &atime,
                &mtime,
                UtimensatFlags::FollowSymlink,
            )
            .map_err(io::Error::from)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }
}

#[cfg(windows)]
pub(crate) mod windows {
    use super::*;

    use std::{fs::File, fs::FileTimes, os::windows::fs::FileTimesExt};

    pub(crate) async fn set_times_path(
        path: PathBuf,
        accessed: Option<time::SystemTime>,
        modified: Option<time::SystemTime>,
        created: Option<time::SystemTime>,
    ) -> io::Result<()> {
        tokio::task::spawn_blocking(move || {
            let file = File::open(path)?;
            let mut times = FileTimes::new();
            if let Some(accessed) = accessed {
                times = times.set_accessed(accessed);
            }
            if let Some(modified) = modified {
                times = times.set_modified(modified);
            }
            if let Some(created) = created {
                times = times.set_created(created);
            }
            file.set_times(times)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("failed to join timestamp update task")))
    }
}

impl FileTypeLike for std::fs::FileType {
    fn is_file(&self) -> bool {
        std::fs::FileType::is_file(self)
    }

    fn is_dir(&self) -> bool {
        std::fs::FileType::is_dir(self)
    }

    fn is_symlink(&self) -> bool {
        std::fs::FileType::is_symlink(self)
    }
}

impl MetadataLike for std::fs::Metadata {
    fn len(&self) -> u64 {
        std::fs::Metadata::len(self)
    }

    fn file_type(&self) -> impl FileTypeLike {
        std::fs::Metadata::file_type(self)
    }

    fn modified(&self) -> io::Result<time::SystemTime> {
        std::fs::Metadata::modified(self)
    }

    fn accessed(&self) -> io::Result<time::SystemTime> {
        std::fs::Metadata::accessed(self)
    }

    fn created(&self) -> io::Result<time::SystemTime> {
        std::fs::Metadata::created(self)
    }

    fn is_file(&self) -> bool {
        std::fs::Metadata::is_file(self)
    }

    fn is_dir(&self) -> bool {
        std::fs::Metadata::is_dir(self)
    }

    fn is_symlink(&self) -> bool {
        std::fs::Metadata::is_symlink(self)
    }
}

async fn metadata_to_record<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    metadata: &impl MetadataLike,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async |strand, [mut std_mod, mut record, mut tmp]| {
            strand.import("std", &mut std_mod).await?;

            let file_type = if metadata.is_file() {
                global.syms.file
            } else if metadata.is_dir() {
                global.syms.dir
            } else if metadata.is_symlink() {
                global.syms.symlink
            } else {
                global.syms.unknown
            };

            let len = global.syms.len;
            let ty = global.syms.ty;
            let record_sym = global.syms.record;

            method!(
                strand, std_mod, record_sym, &mut record,
                len: metadata.len() as i64,
                ty: file_type
            )
            .await?;

            let modified = global.syms.modified;
            if let Ok(time) = metadata.modified() {
                create_datetime_with_global(strand, global, time, &mut tmp)?;
                record.set(strand, modified, &mut tmp)?;
            }

            let accessed = global.syms.accessed;
            if let Ok(time) = metadata.accessed() {
                create_datetime_with_global(strand, global, time, &mut tmp)?;
                record.set(strand, accessed, &mut tmp)?;
            }

            let created = global.syms.created;
            if let Ok(time) = metadata.created() {
                create_datetime_with_global(strand, global, time, &mut tmp)?;
                record.set(strand, created, &mut tmp)?;
            }

            Output::set(strand, out, record);
            Ok(())
        })
        .await
}

async fn metadata<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    follow: bool,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut record]| {
            let local = global.local.get(strand);
            let path = local.cwd().as_ref().join(&path);
            let container = local.container();
            if let Some(context) = container.as_ref() {
                let client = context.client();
                let metadata = if follow {
                    client.metadata(&path).await
                } else {
                    client.symlink_metadata(&path).await
                }
                .into_sys(strand)?;
                drop(container);
                metadata_to_record(strand, global, &metadata, &mut record).await?;

                #[cfg(unix)]
                unix::unix_metadata_to_record(strand, global, &record, &metadata).await?;
            } else {
                drop(container);
                let metadata = if follow {
                    fs::metadata(&path).await
                } else {
                    fs::symlink_metadata(&path).await
                }
                .into_sys(strand)?;
                metadata_to_record(strand, global, &metadata, &mut record).await?;

                #[cfg(unix)]
                unix::unix_metadata_to_record(strand, global, &record, &metadata).await?;
            }

            Output::set(strand, out, record);
            Ok(())
        })
        .await
}

async fn remove<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    all: bool,
    ignore: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(&path);

    let result = if all {
        let container = local.container();
        if let Some(context) = container.as_ref() {
            match context.client().symlink_metadata(&path).await {
                Ok(metadata) if metadata.is_dir() => {
                    context.client().remove(&path, true, ignore).await
                }
                Ok(_) => context
                    .client()
                    .remove(&path, false, ignore)
                    .await
                    .map(|_| ()),
                Err(e) => Err(e),
            }
        } else {
            drop(container);
            match fs::symlink_metadata(&path).await {
                Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path).await,
                Ok(_) => fs::remove_file(&path).await,
                Err(e) => Err(e),
            }
        }
    } else {
        let container = local.container();
        if let Some(context) = container.as_ref() {
            context
                .client()
                .remove(&path, false, ignore)
                .await
                .map(|_| ())
        } else {
            drop(container);
            fs::remove_file(&path).await
        }
    };

    match result {
        Ok(()) => Ok(()),
        Err(e) if ignore && e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into_sys(strand)),
    }
}

async fn exists<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(&path);
    let container = local.container();
    let res = if let Some(context) = container.as_ref() {
        context.client().metadata(&path).await.map(|_| ())
    } else {
        drop(container);
        fs::metadata(&path).await.map(|_| ())
    };
    Output::set(
        strand,
        out,
        match res {
            Ok(()) => true,
            Err(e) if e.kind() == ErrorKind::NotFound => false,
            Err(e) => return Err(e.into_sys(strand)),
        },
    );
    Ok(())
}

async fn entries<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    #[cfg(unix)]
    let read_dir = {
        use crate::fs::readdir::ReadDir;

        let local = global.local.get(strand);
        let path = local.cwd().as_ref().join(&path);
        let container = local.container();
        let file = if let Some(context) = container.as_ref() {
            context
                .client()
                .open_options()
                .read(true)
                .open(&path)
                .await
                .into_sys(strand)?
        } else {
            fs::File::open(&path).await.into_sys(strand)?
        };

        ReadDir::from_fd(file.into_std().await.into()).into_sys(strand)?
    };

    #[cfg(not(unix))]
    let read_dir = fs::read_dir(&path).await.into_sys(strand)?;

    global.types.dir_entry_iter.create_with_annex(
        strand,
        DirEntryIter {
            read_dir,
            path: path.as_ref().to_owned(),
        },
        DirEntryIterAnnex { global },
        out,
    );
    Ok(())
}

async fn read<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    mode: Option<Slot<'v, '_>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let mode = match mode {
        None => "r",
        Some(mode) => match mode.as_str(strand) {
            Some("b") => "rb",
            Some(_) => return Err(Error::type_error(strand, "fs.read: mode must be `b`")),
            None => return Err(Error::type_error(strand, "fs.read: mode must be a string")),
        },
    };
    let is_binary = mode == "rb";
    let mut file = file::open(strand, global, path.as_ref(), mode)
        .await
        .into_sys(strand)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.into_sys(strand)?;
    if is_binary {
        Output::set(strand, out, buf.as_slice());
    } else {
        let text =
            std::str::from_utf8(&buf).map_err(|_| Error::runtime(strand, "invalid UTF-8 data"))?;
        Output::set(strand, out, text);
    }
    Ok(())
}

async fn write<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    data: Slot<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let mut file = file::open(strand, global, path.as_ref(), "w")
        .await
        .into_sys(strand)?;
    let bytes_written = if let Some(slice) = data.as_u8_slice(strand) {
        file.write_all(slice).await.into_sys(strand)?;
        slice.len()
    } else {
        let text = data.to_string(strand)?;
        file.write_all(text.as_bytes()).await.into_sys(strand)?;
        text.len()
    };
    file.flush().await.into_sys(strand)?;
    Output::set(strand, out, bytes_written as i64);
    Ok(())
}

async fn copy<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: PathOrStr<'v, '_>,
    to: PathOrStr<'v, '_>,
    all: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let from_path = local.cwd().as_ref().join(&from);
    let to_path = local.cwd().as_ref().join(&to);
    let container = local.container();
    if let Some(context) = container.as_ref() {
        context
            .client()
            .copy(&from_path, &to_path, all)
            .await
            .into_sys(strand)?;
    } else {
        dolang_shell_vfs::copy_local(&from_path, &to_path, all)
            .await
            .into_sys(strand)?;
    }
    Ok(())
}

async fn move_<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: PathOrStr<'v, '_>,
    to: PathOrStr<'v, '_>,
    all: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let from_path = local.cwd().as_ref().join(&from);
    let to_path = local.cwd().as_ref().join(&to);
    let container = local.container();
    if let Some(context) = container.as_ref() {
        context
            .client()
            .move_(&from_path, &to_path, all)
            .await
            .into_sys(strand)?;
    } else {
        dolang_shell_vfs::move_local(&from_path, &to_path, all)
            .await
            .into_sys(strand)?;
    }
    Ok(())
}

async fn rename<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: PathOrStr<'v, '_>,
    to: PathOrStr<'v, '_>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let from_path = local.cwd().as_ref().join(&from);
    let to_path = local.cwd().as_ref().join(&to);
    let container = local.container();
    if let Some(context) = container.as_ref() {
        context
            .client()
            .rename(&from_path, &to_path)
            .await
            .into_sys(strand)?;
    } else {
        fs::rename(&from_path, &to_path).await.into_sys(strand)?;
    }
    Ok(())
}

async fn symlink<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: PathOrStr<'v, '_>,
    dst: PathOrStr<'v, '_>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let src_path = local.cwd().as_ref().join(&src);
    let dst_path = local.cwd().as_ref().join(&dst);
    let container = local.container();
    if let Some(context) = container.as_ref() {
        context
            .client()
            .symlink(&src_path, &dst_path)
            .await
            .into_sys(strand)?;
    } else {
        #[cfg(unix)]
        fs::symlink(&src_path, &dst_path).await.into_sys(strand)?;
        #[cfg(windows)]
        {
            use std::io::{self, ErrorKind};
            let metadata = fs::symlink_metadata(&src_path).await.into_sys(strand)?;
            if metadata.is_dir() {
                fs::symlink_dir(&src_path, &dst_path)
                    .await
                    .into_sys(strand)?;
            } else if metadata.is_file() {
                fs::symlink_file(&src_path, &dst_path)
                    .await
                    .into_sys(strand)?;
            } else {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "cannot determine if symlink target is a file or directory",
                )
                .into_sys(strand));
            }
        }
    }
    Ok(())
}

async fn symlink_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: PathOrStr<'v, '_>,
    dst: PathOrStr<'v, '_>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let src_path = local.cwd().as_ref().join(&src);
    let dst_path = local.cwd().as_ref().join(&dst);
    let container = local.container();
    if let Some(context) = container.as_ref() {
        context
            .client()
            .symlink(&src_path, &dst_path)
            .await
            .into_sys(strand)?;
    } else {
        #[cfg(unix)]
        fs::symlink(&src_path, &dst_path).await.into_sys(strand)?;
        #[cfg(windows)]
        fs::symlink_dir(&src_path, &dst_path)
            .await
            .into_sys(strand)?;
    }
    Ok(())
}

async fn symlink_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: PathOrStr<'v, '_>,
    dst: PathOrStr<'v, '_>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let src_path = local.cwd().as_ref().join(&src);
    let dst_path = local.cwd().as_ref().join(&dst);
    let container = local.container();
    if let Some(context) = container.as_ref() {
        context
            .client()
            .symlink(&src_path, &dst_path)
            .await
            .into_sys(strand)?;
    } else {
        #[cfg(unix)]
        fs::symlink(&src_path, &dst_path).await.into_sys(strand)?;
        #[cfg(windows)]
        fs::symlink_file(&src_path, &dst_path)
            .await
            .into_sys(strand)?;
    }
    Ok(())
}

async fn create_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    all: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(&path);
    let container = local.container();
    if let Some(context) = container.as_ref() {
        let client = context.client();
        client.create_dir(&path, all).await.into_sys(strand)?;
    } else if all {
        fs::create_dir_all(&path).await.into_sys(strand)?;
    } else {
        fs::create_dir(&path).await.into_sys(strand)?;
    }
    Ok(())
}

async fn remove_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    all: bool,
    ignore: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(&path);
    let container = local.container();
    let result = if let Some(context) = container.as_ref() {
        let client = context.client();
        client.remove_dir(&path, all, ignore).await
    } else if all {
        dolang_shell_vfs::remove_dir_empty_tree_local(&path, ignore)
            .await
            .map(|_| ())
    } else {
        fs::remove_dir(&path).await
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if ignore && e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into_sys(strand)),
    }
}

async fn chmod<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    mode: u32,
) -> Result<'v, 's, ()> {
    #[cfg(unix)]
    {
        let local = global.local.get(strand);
        let path = local.cwd().as_ref().join(&path);
        let container = local.container();
        if let Some(context) = container.as_ref() {
            let client = context.client();
            let perm = dolang_shell_vfs::Permissions::from_mode(mode);
            client.set_permissions(&path, perm).await.into_sys(strand)?;
        } else {
            drop(container);
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(mode);
            fs::set_permissions(&path, permissions)
                .await
                .into_sys(strand)?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (global, path, mode);
        Err(Error::runtime(
            strand,
            "chmod is not supported on this platform",
        ))
    }
}

fn parse_timestamp_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Option<Slot<'v, '_>>,
    name: &str,
) -> Result<'v, 's, Option<time::SystemTime>> {
    let Some(value) = value else {
        return Ok(None);
    };
    datetime_to_system_time(strand, global.types.date_time, &value)
        .map(Some)
        .map_err(|_| Error::type_error(strand, format!("{name}: expected DateTime")))
}

#[cfg(unix)]
fn system_time_to_unix_timestamp<'v, 's>(
    strand: &mut Strand<'v, 's>,
    time: Option<time::SystemTime>,
) -> Result<'v, 's, Option<(i64, u32)>> {
    let Some(time) = time else {
        return Ok(None);
    };
    match time.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(duration) => {
            let secs = i64::try_from(duration.as_secs()).map_err(|_| Error::overflow(strand))?;
            Ok(Some((secs, duration.subsec_nanos())))
        }
        Err(err) => {
            let duration = err.duration();
            let secs = i64::try_from(duration.as_secs()).map_err(|_| Error::overflow(strand))?;
            if duration.subsec_nanos() == 0 {
                Ok(Some((
                    0i64.checked_sub(secs)
                        .ok_or_else(|| Error::overflow(strand))?,
                    0,
                )))
            } else {
                Ok(Some((
                    0i64.checked_sub(secs)
                        .and_then(|v| v.checked_sub(1))
                        .ok_or_else(|| Error::overflow(strand))?,
                    1_000_000_000u32 - duration.subsec_nanos(),
                )))
            }
        }
    }
}

async fn set_timestamps<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathOrStr<'v, '_>,
    modified: Option<Slot<'v, '_>>,
    accessed: Option<Slot<'v, '_>>,
    created: Option<Slot<'v, '_>>,
) -> Result<'v, 's, ()> {
    #[cfg(unix)]
    {
        let modified = parse_timestamp_arg(strand, global, modified, "modified")?;
        let accessed = parse_timestamp_arg(strand, global, accessed, "accessed")?;
        let created = parse_timestamp_arg(strand, global, created, "created")?;
        let modified = system_time_to_unix_timestamp(strand, modified)?;
        let accessed = system_time_to_unix_timestamp(strand, accessed)?;
        if created.is_some() {
            return Err(Error::runtime(
                strand,
                "created: unsupported on this platform",
            ));
        }
        let local = global.local.get(strand);
        let path = local.cwd().as_ref().join(&path);
        let container = local.container();
        if let Some(context) = container.as_ref() {
            context
                .client()
                .utime(&path, accessed, modified)
                .await
                .into_sys(strand)?;
        } else {
            drop(container);
            unix::utime_path(path, accessed, modified)
                .await
                .into_sys(strand)?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let modified = parse_timestamp_arg(strand, global, modified, "modified")?;
        let accessed = parse_timestamp_arg(strand, global, accessed, "accessed")?;
        let created = parse_timestamp_arg(strand, global, created, "created")?;
        let local = global.local.get(strand);
        let path = local.cwd().as_ref().join(&path);
        windows::set_times_path(path, accessed, modified, created)
            .await
            .into_sys(strand)?;
        Ok(())
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (global, path, modified, accessed, created);
        Err(Error::runtime(
            strand,
            "set_timestamps is not supported on this platform",
        ))
    }
}

#[cfg(unix)]
fn parse_chown_identity<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &dolang::runtime::Value<'v>,
    field: &'static str,
) -> Result<'v, 's, dolang_shell_vfs::ChownIdentity> {
    if let Some(value) = value.as_i64(strand) {
        let value = u32::try_from(value)
            .map_err(|_| Error::type_error(strand, "expected non-negative int or str"))?;
        Ok(dolang_shell_vfs::ChownIdentity::Id(value))
    } else if let Some(value) = value.as_str(strand) {
        Ok(dolang_shell_vfs::ChownIdentity::Name(value.to_string()))
    } else {
        Err(Error::type_error(
            strand,
            match field {
                "user" => "user: expected int or str",
                "group" => "group: expected int or str",
                _ => "expected int or str",
            },
        ))
    }
}

#[cfg(unix)]
fn parse_chown_common<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    args: dolang::runtime::Args<'v, 'a>,
    path: Option<PathBuf>,
) -> Result<
    'v,
    's,
    (
        PathBuf,
        Option<dolang_shell_vfs::ChownIdentity>,
        Option<dolang_shell_vfs::ChownIdentity>,
        bool,
    ),
> {
    let mut positional_index = usize::from(path.is_some());
    let mut path = path;
    let mut user = None;
    let mut group = None;
    let mut follow = true;

    for arg in args {
        match arg {
            Arg::Pos(slot) => {
                if path.is_none() {
                    path = Some(PathOrStr::new(strand, global, &slot)?.to_path_buf());
                } else if user.is_none() {
                    user = Some(parse_chown_identity(strand, &slot, "user")?);
                } else {
                    return Err(Error::unexpected_positional(strand, positional_index));
                }
                positional_index += 1;
            }
            Arg::Key(sym, slot) if sym == global.syms.group => {
                group = Some(parse_chown_identity(strand, &slot, "group")?);
            }
            Arg::Key(sym, slot) if sym == global.syms.follow => {
                follow = slot
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "follow: expected bool"))?;
            }
            Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
        }
    }

    let path = path.ok_or_else(|| Error::missing_positional(strand, 0))?;
    if user.is_none() && group.is_none() {
        return Err(Error::runtime(
            strand,
            "chown requires at least one of user or group",
        ));
    }
    Ok((path, user, group, follow))
}

#[cfg(unix)]
async fn chown<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathBuf,
    user: Option<dolang_shell_vfs::ChownIdentity>,
    group: Option<dolang_shell_vfs::ChownIdentity>,
    follow: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let container = local.container();
    if let Some(context) = container.as_ref() {
        context
            .client()
            .chown(&path, user, group, follow)
            .await
            .into_sys(strand)?;
    } else {
        drop(container);
        unix::chown_path(path, user, group, follow)
            .await
            .into_sys(strand)?;
    }
    Ok(())
}

/// Shared implementation for `fs.absolute` and `Path.absolute`.
pub(crate) fn path_absolute<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        local.cwd().as_ref().join(path)
    };
    global
        .types
        .path
        .create_with_annex(strand, Path, PathAnnex::new(absolute, global), out);
    Ok(())
}

/// Shared implementation for `fs.relative` and `Path.relative`.
pub(crate) fn path_relative<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    base: Option<Slot<'v, '_>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let relative = match base {
        Some(b) => path.strip_prefix(&PathOrStr::new(strand, global, &b)?),
        None => path.strip_prefix(global.local.get(strand).cwd().as_ref()),
    };
    global.types.path.create_with_annex(
        strand,
        Path,
        PathAnnex::new(
            relative
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| path.to_path_buf()),
            global,
        ),
        out,
    );
    Ok(())
}

/// Shared implementation for `fs.canonical` and `Path.canonical`.
pub(crate) async fn path_canonical<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let absolute = local.cwd().as_ref().join(path);
    let container = local.container();
    let canonical = if let Some(context) = container.as_ref() {
        context.client().canonicalize(&absolute).await
    } else {
        #[cfg(target_os = "windows")]
        {
            use dolang::runtime::error::ResultExt;
            tokio::task::spawn_blocking(move || dunce::canonicalize(&absolute))
                .await
                .into_do(strand)?
        }
        #[cfg(not(target_os = "windows"))]
        {
            fs::canonicalize(&absolute).await
        }
    }
    .into_sys(strand)?;
    global
        .types
        .path
        .create_with_annex(strand, Path, PathAnnex::new(canonical, global), out);
    Ok(())
}

async fn glob<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    root: Option<&std::path::Path>,
    pattern: Slot<'v, '_>,
    max_depth: Option<Slot<'v, '_>>,
    follow: Option<Slot<'v, '_>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let pattern = pattern
        .as_str(strand)
        .ok_or_else(|| Error::type_error(strand, "pattern: expected str"))?;
    let max_depth = match max_depth {
        Some(v) => Some(
            v.as_i64(strand)
                .ok_or_else(|| Error::type_error(strand, "max_depth: expected int"))?
                .try_into()
                .map_err(|_| Error::overflow(strand))?,
        ),
        None => None,
    };
    let follow = match follow {
        Some(v) => v
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
        None => false,
    };

    let local = global.local.get(strand);
    let cwd = local.cwd();
    let container = local.container();

    let paths = if let Some(context) = container.as_ref() {
        context
            .client()
            .glob(
                pattern,
                root.unwrap_or_else(|| cwd.as_ref()),
                follow,
                max_depth,
            )
            .await
            .into_sys(strand)?
    } else {
        dolang_shell_vfs::glob_local(
            pattern,
            root.unwrap_or_else(|| cwd.as_ref()),
            follow,
            max_depth,
        )
        .await
        .into_sys(strand)?
    };

    global.types.glob_iter.create_with_annex(
        strand,
        GlobIter {
            paths: paths.into(),
        },
        GlobIterAnnex {
            global,
            prefix: root.map(|p| p.to_owned()).unwrap_or_default(),
        },
        out,
    );
    Ok(())
}

async fn create_temp_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    parent: PathBuf,
) -> std::result::Result<PathBuf, io::Error> {
    let mut rng = rand::rng();
    let container = global.local.get(strand).container();
    for attempt in 0..1000 {
        let random_suffix: String = (0..16)
            .map(|_| rng.sample(Alphanumeric))
            .map(char::from)
            .collect();
        let temp_path = parent.join(format!("tmp_{}", random_suffix));
        let result = if let Some(context) = container.as_ref() {
            context.client().create_dir(&temp_path, false).await
        } else {
            fs::create_dir(&temp_path).await
        };
        match result {
            Ok(()) => return Ok(temp_path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 999 => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to create temporary directory after many attempts",
    ))
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let parent = builder.sym("parent");
    let all = builder.sym("all");
    let ignore = builder.sym("ignore");
    let max_depth = builder.sym("max_depth");
    let follow = builder.sym("follow");
    let modified = builder.sym("modified");
    let accessed = builder.sym("accessed");
    let created = builder.sym("created");
    let module = builder
        .module("fs")
        .function("open", async move |strand, args, out| {
            let ([path], [opt1, opt2]) = unpack!(strand, args, 1, 2)?;
            let path = PathOrStr::new(strand, global, &path)?;
            File::open(strand, global, path, opt1, opt2, out).await
        })
        .function("remove", async move |strand, args, _out| {
            let ([], [all, ignore], paths) =
                unpack!(strand, args, 0, 0, all = None, ignore = None, ...)?;
            let all = match all {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            let ignore = match ignore {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            for path in paths {
                match path {
                    Arg::Pos(path) => {
                        let path = PathOrStr::new(strand, global, &path)?;
                        remove(strand, global, path, all, ignore).await?;
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }
            Ok(())
        })
        .function("metadata", async move |strand, args, out| {
            let ([path], [follow]) = unpack!(strand, args, 1, 1)?;
            let path = PathOrStr::new(strand, global, &path)?;
            let follow = match follow {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => true,
            };
            metadata(strand, global, path, follow, out).await
        })
        .function("exists", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = PathOrStr::new(strand, global, &path)?;
            exists(strand, global, path, out).await
        })
        .function("read", async move |strand, args, out| {
            let ([path], [mode]) = unpack!(strand, args, 1, 1)?;
            let path = PathOrStr::new(strand, global, &path)?;
            read(strand, global, path, mode, out).await
        })
        .function("write", async move |strand, args, out| {
            let ([path, data], []) = unpack!(strand, args, 2, 0)?;
            let path = PathOrStr::new(strand, global, &path)?;
            write(strand, global, path, data, out).await
        })
        .function("is_absolute", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = PathOrStr::new(strand, global, &path)?;
            Output::set(strand, out, path.is_absolute());
            Ok(())
        })
        .function("copy", async move |strand, args, out| {
            let ([from, to], [all]) = unpack!(strand, args, 2, 0, all = None)?;
            let from = PathOrStr::new(strand, global, &from)?;
            let to = PathOrStr::new(strand, global, &to)?;
            let all = match all {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            let _ = out;
            copy(strand, global, from, to, all).await
        })
        .function("rename", async move |strand, args, _out| {
            let ([from, to], []) = unpack!(strand, args, 2, 0)?;
            let from = PathOrStr::new(strand, global, &from)?;
            let to = PathOrStr::new(strand, global, &to)?;
            rename(strand, global, from, to).await
        })
        .function("move", async move |strand, args, _out| {
            let ([from, to], [all]) = unpack!(strand, args, 2, 0, all = None)?;
            let from = PathOrStr::new(strand, global, &from)?;
            let to = PathOrStr::new(strand, global, &to)?;
            let all = match all {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            move_(strand, global, from, to, all).await
        })
        .function("symlink", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = PathOrStr::new(strand, global, &src)?;
            let dst = PathOrStr::new(strand, global, &dst)?;
            symlink(strand, global, src, dst).await
        })
        .function("symlink_dir", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = PathOrStr::new(strand, global, &src)?;
            let dst = PathOrStr::new(strand, global, &dst)?;
            symlink_dir(strand, global, src, dst).await
        })
        .function("symlink_file", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = PathOrStr::new(strand, global, &src)?;
            let dst = PathOrStr::new(strand, global, &dst)?;
            symlink_file(strand, global, src, dst).await
        })
        .function("entries", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = PathOrStr::new(strand, global, &path)?;
            entries(strand, global, path, out).await
        })
        .function("glob", async move |strand, args, out| {
            let ([pattern], [max_depth, follow]) =
                unpack!(strand, args, 1, 0, max_depth = None, follow = None)?;
            glob(strand, global, None, pattern, max_depth, follow, out).await
        })
        .function("create_dir", async move |strand, args, _out| {
            let ([path], [all]) = unpack!(strand, args, 1, 0, all = None)?;
            let path = PathOrStr::new(strand, global, &path)?;
            let all = match all {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            create_dir(strand, global, path, all).await
        })
        .function("remove_dir", async move |strand, args, _out| {
            let ([], [all, ignore], paths) =
                unpack!(strand, args, 0, 0, all = None, ignore = None, ...)?;
            let all = match all {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            let ignore = match ignore {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            for path in paths {
                match path {
                    Arg::Pos(path) => {
                        let path = PathOrStr::new(strand, global, &path)?;
                        remove_dir(strand, global, path, all, ignore).await?;
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }
            Ok(())
        })
        .function("chmod", async move |strand, args, _out| {
            let ([path, mode], []) = unpack!(strand, args, 2, 0)?;
            let path = PathOrStr::new(strand, global, &path)?;
            let mode = mode
                .as_i64(strand)
                .ok_or_else(|| Error::type_error(strand, "expected int"))?
                as u32;
            chmod(strand, global, path, mode).await
        })
        .function("set_timestamps", async move |strand, args, _out| {
            let ([path], [modified, accessed, created]) = unpack!(
                strand,
                args,
                1,
                0,
                modified = None,
                accessed = None,
                created = None
            )?;
            let path = PathOrStr::new(strand, global, &path)?;
            set_timestamps(strand, global, path, modified, accessed, created).await
        });
    #[cfg(unix)]
    let module = module.function("chown", async move |strand, args, _out| {
        let (path, user, group, follow) = parse_chown_common(strand, global, args, None)?;
        chown(strand, global, path, user, group, follow).await
    });
    module
        .function("normal", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = PathOrStr::new(strand, global, &path)?;
            let normalized = normalize_path(&path);
            global.types.path.create_with_annex(
                strand,
                Path,
                PathAnnex::new(normalized, global),
                out,
            );
            Ok(())
        })
        .function("absolute", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = PathOrStr::new(strand, global, &path)?;
            path_absolute(strand, global, &path, out)
        })
        .function("relative", async move |strand, args, out| {
            let ([path], [base]) = unpack!(strand, args, 1, 1)?;
            let path = PathOrStr::new(strand, global, &path)?;
            let base_path = match base {
                Some(slot) => PathOrStr::new(strand, global, &slot)?.to_path_buf(),
                None => {
                    let local = global.local.get(strand);
                    local.cwd().as_ref().to_path_buf()
                }
            };
            let relative = path
                .strip_prefix(&base_path)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| path.to_path_buf());
            global.types.path.create_with_annex(
                strand,
                Path,
                PathAnnex::new(relative, global),
                out,
            );
            Ok(())
        })
        .function("canonical", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = PathOrStr::new(strand, global, &path)?;
            path_canonical(strand, global, &path, out).await
        })
        .function_with_slots(
            "with_temp_dir",
            async move |strand, args, out, [mut path]| {
                let ([callable], [parent]) = unpack!(strand, args, 1, 0, parent = None)?;
                let parent = match parent {
                    Some(p) => {
                        let p = PathOrStr::new(strand, global, &p)?;
                        let local = global.local.get(strand);
                        local.cwd().as_ref().join(&p)
                    }
                    None => {
                        let local = global.local.get(strand);
                        if cfg!(windows) {
                            std::env::temp_dir()
                        } else {
                            match local.env().get("TMPDIR") {
                                Some(dir) => local.cwd().as_ref().join(dir.as_ref()),
                                None => PathBuf::from("/tmp"),
                            }
                        }
                    }
                };
                let temp_path = create_temp_dir(strand, global, parent)
                    .await
                    .into_sys(strand)?;
                global.types.path.create_with_annex(
                    strand,
                    Path,
                    PathAnnex::new(temp_path.clone(), global),
                    &mut path,
                );
                let result = call!(strand, callable, out, &path).await;
                let _ = strand
                    .with_cancel_mask(true, async move |strand| {
                        let local = global.local.get(strand);
                        let container = local.container();
                        if let Some(context) = container.as_ref() {
                            let client = context.client();
                            client.remove(&temp_path, true, false).await
                        } else {
                            fs::remove_dir_all(&temp_path).await
                        }
                    })
                    .await;
                result
            },
        )
        .value("Path", global.types.path)
        .commit();
}
