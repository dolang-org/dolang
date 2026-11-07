//! Recursive `remove` and `copy`, and the check `move` makes before falling
//! back to them. A walk never follows a symbolic link, and descends into any
//! other crossing point only when it lies below `stop` on the [`Resolve`]
//! scale.
//!
//! The walks run on the blocking pool, one directory at a time.

use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
};

use super::Direct;
use crate::{
    error::{Error, ErrorKind, Result},
    file::FileId,
    metadata::Resolve,
};

#[cfg(unix)]
use unix::{WalkDir, remove_root, root_entry};
#[cfg(windows)]
use windows::{WalkDir, copy_stopped, remove_root, root_entry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Dir,
    File,
    Symlink,
    /// A FIFO, socket or device, which Windows never lists.
    #[cfg_attr(windows, allow(dead_code))]
    Other,
}

/// An entry found by a walk, classified without following it.
#[derive(Debug)]
pub(super) struct Entry {
    pub(super) name: OsString,
    pub(super) kind: Kind,
    /// The lowest `stop` that stops at this entry, if any does.
    pub(super) level: Option<Resolve>,
    /// Whether the OS removes the entry as a directory: on Windows, a
    /// directory symlink is one.
    pub(super) dir: bool,
    /// Windows file attributes.
    #[cfg(windows)]
    pub(super) attrs: u32,
}

impl Entry {
    fn stops(&self, stop: Resolve) -> bool {
        self.level.is_some_and(|level| level <= stop)
    }

    fn descends(&self, stop: Resolve) -> bool {
        self.kind == Kind::Dir && !self.stops(stop)
    }
}

/// Checks a `stop` passed to a recursive operation.
pub(super) fn check_stop(stop: Resolve) -> Result<Resolve> {
    if stop == Resolve::Target {
        Err(Error::new(
            ErrorKind::InvalidInput,
            "stop: :TARGET: is not supported; recursion never follows symbolic links",
        ))
    } else {
        Ok(stop)
    }
}

fn cycle_error(path: &Path) -> Error {
    Error::new(
        ErrorKind::Other,
        format!("{}: directory cycle", path.display()),
    )
}

/// The directories open on the current path of a walk, for finding cycles
/// through a crossing point that leads back to one of them.
struct Ancestors(HashSet<FileId>);

impl Ancestors {
    fn enter(&mut self, dir: &WalkDir) -> Result<()> {
        if self.0.insert(dir.id()) {
            Ok(())
        } else {
            Err(cycle_error(dir.path()))
        }
    }

    fn leave(&mut self, dir: &WalkDir) {
        self.0.remove(&dir.id());
    }
}

struct Frame<T> {
    dir: WalkDir,
    entries: std::vec::IntoIter<Entry>,
    data: T,
}

/// Walks the tree below the directory `root`, calling `leaf` for each entry
/// it doesn't descend into, `enter` before descending into a directory, and
/// `leave` after, with the directory's parent.
fn walk<T>(
    root: WalkDir,
    data: T,
    stop: Resolve,
    mut leaf: impl FnMut(&WalkDir, &Entry, &T) -> Result<()>,
    mut enter: impl FnMut(&WalkDir, &Entry, &T) -> Result<Option<T>>,
    mut leave: impl FnMut(&WalkDir, Entry, T) -> Result<()>,
) -> Result<()> {
    let mut ancestors = Ancestors(HashSet::new());
    ancestors.enter(&root)?;
    let mut root = root;
    let entries = root.entries()?.into_iter();
    let mut stack = vec![(
        Frame {
            dir: root,
            entries,
            data,
        },
        None::<Entry>,
    )];
    while let Some((frame, _)) = stack.last_mut() {
        let Some(entry) = frame.entries.next() else {
            let (frame, entry) = stack.pop().expect("stack is not empty");
            ancestors.leave(&frame.dir);
            let Frame { dir, data, .. } = frame;
            drop(dir);
            if let (Some(entry), Some((parent, _))) = (entry, stack.last()) {
                leave(&parent.dir, entry, data)?;
            }
            continue;
        };
        if !entry.descends(stop) {
            leaf(&frame.dir, &entry, &frame.data)?;
            continue;
        }
        let Some(data) = enter(&frame.dir, &entry, &frame.data)? else {
            continue;
        };
        let mut dir = match frame.dir.open_child(&entry, stop) {
            Ok(dir) => dir,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        ancestors.enter(&dir)?;
        let entries = dir.entries()?.into_iter();
        stack.push((Frame { dir, entries, data }, Some(entry)));
    }
    Ok(())
}

/// Ignores an entry that vanished while a walk was removing it.
fn ignore_vanished(result: Result<()>) -> Result<()> {
    match result {
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Removes `path` and, if it is a directory `stop` doesn't stop at, everything
/// below it. A stopping point is removed as itself, which fails for a mount
/// point or a non-empty directory. The first error ends the walk.
pub(super) fn remove(path: &Path, stop: Resolve) -> Result<()> {
    let root = root_entry(path)?;
    if root.descends(stop) {
        let dir = WalkDir::open(path, stop)?;
        walk(
            dir,
            (),
            stop,
            |dir, entry, ()| ignore_vanished(dir.remove(entry)),
            |_, _, ()| Ok(Some(())),
            |dir, entry, ()| ignore_vanished(dir.remove(&entry)),
        )?;
    }
    remove_root(path, &root)
}

/// Removes `path` without descending into it, which fails for a directory a
/// walk would have to descend into. A Windows junction or directory symlink is
/// removed like any other link.
pub(super) fn remove_entry(path: &Path) -> Result<()> {
    let root = root_entry(path)?;
    if root.kind == Kind::Dir && !root.stops(Resolve::Mount) {
        return Err(Direct::directory_requires_all_error());
    }
    remove_root(path, &root)
}

/// Copies `entry`, at `src`, to `dst` without descending into it.
fn copy_leaf(src: &Path, dst: &Path, entry: &Entry, stop: Resolve) -> Result<()> {
    if entry.stops(stop) {
        return copy_stopped(src, dst, entry);
    }
    match entry.kind {
        Kind::File => {
            std::fs::copy(src, dst)?;
            Ok(())
        }
        Kind::Dir | Kind::Symlink | Kind::Other => Err(Error::new(
            ErrorKind::Unsupported,
            format!("{}: unsupported file type", src.display()),
        )),
    }
}

/// Copies a stopping point as itself.
#[cfg(unix)]
fn copy_stopped(src: &Path, dst: &Path, entry: &Entry) -> Result<()> {
    match entry.kind {
        Kind::Symlink => Ok(std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?),
        // The mount point, without what is mounted there, as `cp -x` does
        _ => Ok(std::fs::create_dir(dst)?),
    }
}

/// Copies `from` to `to`. A directory needs `all`, and is copied with
/// everything below it, descending into crossing points below `stop` and
/// copying the ones at or above it as themselves: a link as a link, and a
/// mount point or other directory as an empty directory.
pub(super) fn copy(from: &Path, to: &Path, all: bool, stop: Resolve) -> Result<()> {
    let root = root_entry(from)?;
    if !root.descends(stop) {
        return copy_leaf(from, to, &root, stop);
    }
    if !all {
        return Err(Direct::directory_requires_all_error());
    }
    std::fs::create_dir(to)?;
    let dir = WalkDir::open(from, stop)?;
    walk(
        dir,
        to.to_path_buf(),
        stop,
        |dir, entry, dst: &PathBuf| {
            copy_leaf(
                &dir.path().join(&entry.name),
                &dst.join(&entry.name),
                entry,
                stop,
            )
        },
        |_, entry, dst| {
            let dst = dst.join(&entry.name);
            std::fs::create_dir(&dst)?;
            Ok(Some(dst))
        },
        |_, _, _| Ok(()),
    )
}

/// Checks that `remove` with `stop` would be able to remove everything below
/// `path`, as far as can be seen without removing anything: that it holds no
/// mount point, and no non-empty directory it would stop at.
pub(super) fn check_removable(path: &Path, stop: Resolve) -> Result<()> {
    let check = |path: &Path, entry: &Entry| -> Result<()> {
        if entry.kind != Kind::Dir || !entry.stops(stop) {
            return Ok(());
        }
        // A Windows junction or volume mount point is removed like a link
        if cfg!(unix) && entry.level == Some(Resolve::Mount) {
            return Err(Error::new(
                ErrorKind::ResourceBusy,
                format!("{}: is a mount point", path.display()),
            ));
        }
        if std::fs::read_dir(path)?.next().is_some() {
            return Err(Error::new(
                ErrorKind::DirectoryNotEmpty,
                format!(
                    "{}: can't remove a non-empty directory that `stop` stops at",
                    path.display()
                ),
            ));
        }
        Ok(())
    };
    let root = root_entry(path)?;
    if !root.descends(stop) {
        return check(path, &root);
    }
    let dir = WalkDir::open(path, stop)?;
    walk(
        dir,
        (),
        stop,
        |dir, entry, ()| check(&dir.path().join(&entry.name), entry),
        |_, _, ()| Ok(Some(())),
        |_, _, ()| Ok(()),
    )
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::{CString, OsString},
        os::{
            fd::{AsFd, OwnedFd},
            unix::ffi::{OsStrExt, OsStringExt},
        },
        path::{Path, PathBuf},
    };

    use nix::{
        dir::Dir,
        fcntl::{OFlag, open, openat},
        sys::stat::{FileStat, Mode, fstat, fstatat},
        unistd::{UnlinkatFlags, unlinkat},
    };

    use super::{Entry, Kind};
    use crate::{
        direct::mount,
        error::{Error, Result},
        file::FileId,
        metadata::Resolve,
    };

    const DIR_FLAGS: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_DIRECTORY)
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC);

    // `dev_t` and `ino_t` are signed or narrower on some targets, as std's
    // `MetadataExt` also casts
    #[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
    fn dev_ino(stat: &FileStat) -> (u64, u64) {
        (stat.st_dev as u64, stat.st_ino as u64)
    }

    fn kind(stat: &FileStat) -> Kind {
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Kind::Dir,
            libc::S_IFREG => Kind::File,
            libc::S_IFLNK => Kind::Symlink,
            _ => Kind::Other,
        }
    }

    /// Classifies the entry `name` in `dir` (the working directory when
    /// `None`), at `path`, whose containing directory is on `parent_dev`.
    fn classify(
        dir: Option<std::os::fd::BorrowedFd<'_>>,
        name: OsString,
        path: &Path,
        stat: &FileStat,
        parent_dev: Option<u64>,
    ) -> Result<Entry> {
        let kind = kind(stat);
        let (dev, _) = dev_ino(stat);
        let level = match kind {
            Kind::Symlink => Some(Resolve::Link),
            Kind::Dir => {
                let cname = CString::new(name.as_bytes())?;
                if mount::is_mount_root(dir, &cname, path, dev, parent_dev) {
                    Some(Resolve::Mount)
                } else if parent_dev.is_some_and(|parent_dev| parent_dev != dev) {
                    Some(Resolve::Cross)
                } else {
                    None
                }
            }
            Kind::File | Kind::Other => None,
        };
        Ok(Entry {
            name,
            kind,
            level,
            dir: kind == Kind::Dir,
        })
    }

    pub(super) fn root_entry(path: &Path) -> Result<Entry> {
        let stat = nix::sys::stat::lstat(path)?;
        // The root of the filesystem is a mount point, with no parent to
        // compare with
        let parent_dev = match path.parent() {
            Some(parent) => {
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                Some(dev_ino(&nix::sys::stat::stat(parent)?).0)
            }
            None => None,
        };
        let mut entry = classify(None, path.as_os_str().to_owned(), path, &stat, parent_dev)?;
        if parent_dev.is_none() && entry.kind == Kind::Dir {
            entry.level = Some(Resolve::Mount);
        }
        Ok(entry)
    }

    pub(super) fn remove_root(path: &Path, entry: &Entry) -> Result<()> {
        if entry.dir {
            Ok(std::fs::remove_dir(path)?)
        } else {
            Ok(std::fs::remove_file(path)?)
        }
    }

    /// A directory open for walking, by descriptor, so no component of its
    /// path is looked up again once it is open.
    pub(super) struct WalkDir {
        fd: OwnedFd,
        path: PathBuf,
        dev: u64,
        id: FileId,
    }

    impl WalkDir {
        fn new(fd: OwnedFd, path: PathBuf) -> Result<Self> {
            let (dev, ino) = dev_ino(&fstat(&fd)?);
            Ok(Self {
                fd,
                path,
                dev,
                id: FileId {
                    volume: dev,
                    index: ino,
                },
            })
        }

        pub(super) fn open(path: &Path, _stop: Resolve) -> Result<Self> {
            let fd = open(path, DIR_FLAGS, Mode::empty())?;
            Self::new(fd, path.to_path_buf())
        }

        pub(super) fn open_child(&self, entry: &Entry, _stop: Resolve) -> Result<Self> {
            let fd = openat(&self.fd, entry.name.as_os_str(), DIR_FLAGS, Mode::empty())?;
            Self::new(fd, self.path.join(&entry.name))
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        pub(super) fn id(&self) -> FileId {
            self.id
        }

        pub(super) fn entries(&mut self) -> Result<Vec<Entry>> {
            // A fresh open file description, so listing doesn't move the
            // offset of the one `self.fd` shares with nothing else
            let fd = openat(&self.fd, ".", DIR_FLAGS, Mode::empty())?;
            let mut dir = Dir::from_fd(fd)?;
            let mut entries = Vec::new();
            for entry in dir.iter() {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let name = OsString::from_vec(name.to_vec());
                let stat = match fstatat(
                    &self.fd,
                    name.as_os_str(),
                    nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
                ) {
                    Ok(stat) => stat,
                    // Removed since it was listed
                    Err(nix::errno::Errno::ENOENT) => continue,
                    Err(err) => return Err(err.into()),
                };
                let path = self.path.join(&name);
                entries.push(classify(
                    Some(self.fd.as_fd()),
                    name,
                    &path,
                    &stat,
                    Some(self.dev),
                )?);
            }
            Ok(entries)
        }

        pub(super) fn remove(&self, entry: &Entry) -> Result<()> {
            let flag = if entry.dir {
                UnlinkatFlags::RemoveDir
            } else {
                UnlinkatFlags::NoRemoveDir
            };
            unlinkat(&self.fd, entry.name.as_os_str(), flag).map_err(Error::from)
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::{
        ffi::OsString,
        os::windows::io::{AsHandle, OwnedHandle},
        path::{Path, PathBuf},
    };

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    };

    use super::{Entry, Kind};
    use crate::{
        direct::{
            Direct,
            win_dir::{self, DirIter, RawEntry},
        },
        error::{Error, ErrorKind, Result},
        file::FileId,
        metadata::Resolve,
    };

    fn classify(name: OsString, attrs: u32, tag: Option<u32>) -> Entry {
        let raw = RawEntry { name, attrs, tag };
        let kind = match raw.file_type() {
            crate::metadata::FileType::Symlink => Kind::Symlink,
            crate::metadata::FileType::Dir => Kind::Dir,
            _ => Kind::File,
        };
        let dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        // A reparse point that isn't a name surrogate only stops a walk when
        // it is a directory: there is nothing to descend into in a file
        let level = tag
            .map(Resolve::of_reparse_tag)
            .filter(|&level| dir || level < Resolve::Cross);
        Entry {
            name: raw.name,
            kind,
            level,
            dir,
            attrs,
        }
    }

    pub(super) fn root_entry(path: &Path) -> Result<Entry> {
        let (attrs, tag) = Direct::probe_entry(path)?;
        Ok(classify(path.as_os_str().to_owned(), attrs, tag))
    }

    fn remove_path(path: &Path, entry: &Entry) -> Result<()> {
        if entry.dir {
            return Ok(std::fs::remove_dir(path)?);
        }
        if entry.attrs & FILE_ATTRIBUTE_READONLY != 0 {
            let mut permissions = std::fs::symlink_metadata(path)?.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            std::fs::set_permissions(path, permissions)?;
        }
        Ok(std::fs::remove_file(path)?)
    }

    pub(super) fn remove_root(path: &Path, entry: &Entry) -> Result<()> {
        remove_path(path, entry)
    }

    /// Copies a stopping point as itself: a link or mount point as a reparse
    /// point, byte for byte, and any other directory, such as a cloud folder,
    /// as an empty directory.
    pub(super) fn copy_stopped(src: &Path, dst: &Path, entry: &Entry) -> Result<()> {
        match entry.level {
            Some(Resolve::Link | Resolve::Mount) => Direct::copy_reparse_point_sync(src, dst),
            _ => Ok(std::fs::create_dir(dst)?),
        }
    }

    /// A directory open for walking. Its handle doesn't share delete access,
    /// so it can't be renamed, or replaced with a link, while the walk is
    /// inside it.
    pub(super) struct WalkDir {
        iter: DirIter,
        path: PathBuf,
        id: FileId,
    }

    impl WalkDir {
        pub(super) fn open(path: &Path, stop: Resolve) -> Result<Self> {
            let handle = win_dir::open_dir(path, true, false)?;
            let handle: OwnedHandle = match Direct::reparse_tag(handle.as_handle())? {
                None => handle,
                Some(tag) if Resolve::of_reparse_tag(tag) > stop => {
                    drop(handle);
                    win_dir::open_dir(path, false, false)?
                }
                Some(_) => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("{}: changed while being walked", path.display()),
                    ));
                }
            };
            let id = crate::direct::File::handle_id(handle.as_handle())?;
            Ok(Self {
                iter: DirIter::new(handle),
                path: path.to_path_buf(),
                id,
            })
        }

        pub(super) fn open_child(&self, entry: &Entry, stop: Resolve) -> Result<Self> {
            Self::open(&self.path.join(&entry.name), stop)
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        pub(super) fn id(&self) -> FileId {
            self.id
        }

        pub(super) fn entries(&mut self) -> Result<Vec<Entry>> {
            let mut entries = Vec::new();
            while let Some(raw) = self.iter.next_entry()? {
                entries.push(classify(raw.name, raw.attrs, raw.tag));
            }
            Ok(entries)
        }

        pub(super) fn remove(&self, entry: &Entry) -> Result<()> {
            remove_path(&self.path.join(&entry.name), entry)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::Path;

    use super::{Kind, WalkDir, check_removable, root_entry};
    use crate::{error::ErrorKind, metadata::Resolve};

    #[test]
    fn classifies_mount_points() {
        for path in ["/", "/proc"] {
            let entry = root_entry(Path::new(path)).unwrap();
            assert_eq!(entry.kind, Kind::Dir, "{path}");
            assert_eq!(entry.level, Some(Resolve::Mount), "{path}");
        }
        let mut root = WalkDir::open(Path::new("/"), Resolve::Cross).unwrap();
        let proc = root
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == "proc")
            .unwrap();
        assert_eq!(proc.level, Some(Resolve::Mount));
    }

    #[test]
    fn classifies_ordinary_entries() {
        let dir = crate::test_temp::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dir")).unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        std::os::unix::fs::symlink("dir", dir.path().join("link")).unwrap();
        let mut walk = WalkDir::open(dir.path(), Resolve::Cross).unwrap();
        let mut entries: Vec<_> = walk
            .entries()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.name.into_string().unwrap(), entry.kind, entry.level))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            entries,
            [
                ("dir".to_owned(), Kind::Dir, None),
                ("file".to_owned(), Kind::File, None),
                ("link".to_owned(), Kind::Symlink, Some(Resolve::Link)),
            ]
        );
    }

    #[test]
    fn mount_point_is_not_removable() {
        let err = check_removable(Path::new("/proc"), Resolve::Cross).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ResourceBusy);
        let dir = crate::test_temp::tempdir().unwrap();
        check_removable(dir.path(), Resolve::Cross).unwrap();
    }
}
