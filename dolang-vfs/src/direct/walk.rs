//! Recursive `remove` and `copy`, and the check `move` makes before falling
//! back to them. A walk's [`Bounds`] say where it stops descending: at
//! crossing points [`Stop`] covers, and at symbolic links unless it follows
//! them. A followed link is classified by what it leads to, so a walk that
//! follows links still stops at a mount point one of them leads to.
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
    metadata::{Resolve, Stop},
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

/// Where a walk stops descending.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bounds {
    /// The crossing points the walk stops at, or `None` for none of them.
    pub(super) stop: Option<Stop>,
    /// Whether the walk follows symbolic links instead of stopping at them.
    pub(super) follow: bool,
}

impl Bounds {
    /// The bounds of a removal, which never follows links.
    pub(super) fn remove(stop: Stop) -> Self {
        Self {
            stop: Some(stop),
            follow: false,
        }
    }

    fn stops(self, level: Resolve) -> bool {
        match level {
            Resolve::Target => false,
            Resolve::Link => !self.follow,
            level => self.stop.is_some_and(|stop| level <= stop.level()),
        }
    }
}

/// An entry found by a walk. A symbolic link the walk follows is classified
/// by what it leads to, and any other entry as itself.
#[derive(Debug)]
pub(super) struct Entry {
    pub(super) name: OsString,
    pub(super) kind: Kind,
    /// The lowest [`Resolve`] level that opens this entry as itself, if any
    /// does.
    pub(super) level: Option<Resolve>,
    /// Whether the OS removes the entry as a directory: on Windows, a
    /// directory symlink is one.
    pub(super) dir: bool,
    /// Windows file attributes.
    #[cfg(windows)]
    pub(super) attrs: u32,
    /// Where a followed link leads.
    #[cfg(windows)]
    pub(super) target: Option<PathBuf>,
}

impl Entry {
    fn stops(&self, bounds: Bounds) -> bool {
        self.level.is_some_and(|level| bounds.stops(level))
    }

    fn descends(&self, bounds: Bounds) -> bool {
        self.kind == Kind::Dir && !self.stops(bounds)
    }
}

pub(super) fn cycle_error(path: &Path) -> Error {
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
    bounds: Bounds,
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
        if !entry.descends(bounds) {
            leaf(&frame.dir, &entry, &frame.data)?;
            continue;
        }
        let Some(data) = enter(&frame.dir, &entry, &frame.data)? else {
            continue;
        };
        let mut dir = match frame.dir.open_child(&entry, bounds) {
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
pub(super) fn remove(path: &Path, stop: Stop) -> Result<()> {
    let bounds = Bounds::remove(stop);
    let root = root_entry(path, false)?;
    if root.descends(bounds) {
        let dir = WalkDir::open(path, bounds)?;
        walk(
            dir,
            (),
            bounds,
            |dir, entry, ()| ignore_vanished(dir.remove(entry)),
            |_, _, ()| Ok(Some(())),
            |dir, entry, ()| ignore_vanished(dir.remove(&entry)),
        )?;
    }
    remove_root(path, &root)
}

/// Returns whether a walk with `bounds` stops at `path`.
pub(super) fn stops_at(path: &Path, bounds: Bounds) -> Result<bool> {
    Ok(root_entry(path, bounds.follow)?.stops(bounds))
}

/// Removes `path` without descending into it, which fails for a directory a
/// walk would have to descend into. A Windows junction or directory symlink is
/// removed like any other link.
pub(super) fn remove_entry(path: &Path) -> Result<()> {
    let root = root_entry(path, false)?;
    if root.descends(Bounds::remove(Stop::Mount)) {
        return Err(Direct::directory_requires_all_error());
    }
    remove_root(path, &root)
}

/// Copies `entry`, at `src`, to `dst` without descending into it.
fn copy_leaf(src: &Path, dst: &Path, entry: &Entry, bounds: Bounds) -> Result<()> {
    if entry.stops(bounds) {
        return copy_stopped(src, dst, entry);
    }
    match entry.kind {
        Kind::File => {
            std::fs::copy(src, dst)?;
            Ok(())
        }
        // Only a walk that follows links reaches a link it doesn't stop at,
        // and only when it leads nowhere
        Kind::Symlink => Err(Error::new(
            ErrorKind::NotFound,
            format!("{}: dangling symbolic link", src.display()),
        )),
        Kind::Dir | Kind::Other => Err(Error::new(
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
/// everything below it, copying the points `bounds` stops at as themselves: a
/// link as a link, and a mount point or other directory as an empty
/// directory. When `bounds` follows links, `from` is followed too, and what
/// links lead to is copied.
pub(super) fn copy(from: &Path, to: &Path, all: bool, bounds: Bounds) -> Result<()> {
    let root = root_entry(from, bounds.follow)?;
    if !root.descends(bounds) {
        return copy_leaf(from, to, &root, bounds);
    }
    if !all {
        return Err(Direct::directory_requires_all_error());
    }
    std::fs::create_dir(to)?;
    let dir = WalkDir::open(from, bounds)?;
    walk(
        dir,
        to.to_path_buf(),
        bounds,
        |dir, entry, dst: &PathBuf| {
            copy_leaf(
                &dir.path().join(&entry.name),
                &dst.join(&entry.name),
                entry,
                bounds,
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
pub(super) fn check_removable(path: &Path, stop: Stop) -> Result<()> {
    let bounds = Bounds::remove(stop);
    let check = |path: &Path, entry: &Entry| -> Result<()> {
        if entry.kind != Kind::Dir || !entry.stops(bounds) {
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
    let root = root_entry(path, false)?;
    if !root.descends(bounds) {
        return check(path, &root);
    }
    let dir = WalkDir::open(path, bounds)?;
    walk(
        dir,
        (),
        bounds,
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

    use super::{Bounds, Entry, Kind};
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
    /// With `follow`, `name` is a link and `stat` describes what it leads to.
    fn classify(
        dir: Option<std::os::fd::BorrowedFd<'_>>,
        name: OsString,
        path: &Path,
        stat: &FileStat,
        parent_dev: Option<u64>,
        follow: bool,
    ) -> Result<Entry> {
        let kind = kind(stat);
        let (dev, _) = dev_ino(stat);
        let level = match kind {
            Kind::Symlink => Some(Resolve::Link),
            Kind::Dir => {
                let cname = CString::new(name.as_bytes())?;
                if mount::is_mount_root(dir, &cname, path, dev, parent_dev, follow) {
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

    /// Classifies `path`, following it with `follow` unless it is a dangling
    /// link.
    pub(super) fn root_entry(path: &Path, follow: bool) -> Result<Entry> {
        let mut stat = nix::sys::stat::lstat(path)?;
        let mut followed = false;
        if follow
            && kind(&stat) == Kind::Symlink
            && let Ok(target) = nix::sys::stat::stat(path)
        {
            stat = target;
            followed = true;
        }
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
        let mut entry = classify(
            None,
            path.as_os_str().to_owned(),
            path,
            &stat,
            parent_dev,
            followed,
        )?;
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
        /// Whether the walk follows symbolic links.
        follow: bool,
    }

    fn dir_flags(follow: bool) -> OFlag {
        if follow {
            DIR_FLAGS.difference(OFlag::O_NOFOLLOW)
        } else {
            DIR_FLAGS
        }
    }

    impl WalkDir {
        fn new(fd: OwnedFd, path: PathBuf, follow: bool) -> Result<Self> {
            let (dev, ino) = dev_ino(&fstat(&fd)?);
            Ok(Self {
                fd,
                path,
                dev,
                id: FileId {
                    volume: dev,
                    index: ino,
                },
                follow,
            })
        }

        pub(super) fn open(path: &Path, bounds: Bounds) -> Result<Self> {
            let fd = open(path, dir_flags(bounds.follow), Mode::empty())?;
            Self::new(fd, path.to_path_buf(), bounds.follow)
        }

        pub(super) fn open_child(&self, entry: &Entry, _bounds: Bounds) -> Result<Self> {
            let fd = openat(
                &self.fd,
                entry.name.as_os_str(),
                dir_flags(self.follow),
                Mode::empty(),
            )?;
            Self::new(fd, self.path.join(&entry.name), self.follow)
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
                let mut stat = match fstatat(
                    &self.fd,
                    name.as_os_str(),
                    nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
                ) {
                    Ok(stat) => stat,
                    // Removed since it was listed
                    Err(nix::errno::Errno::ENOENT) => continue,
                    Err(err) => return Err(err.into()),
                };
                let mut followed = false;
                if self.follow && kind(&stat) == Kind::Symlink {
                    match fstatat(&self.fd, name.as_os_str(), nix::fcntl::AtFlags::empty()) {
                        Ok(target) => {
                            stat = target;
                            followed = true;
                        }
                        // Dangling, or a loop of links
                        Err(nix::errno::Errno::ENOENT | nix::errno::Errno::ELOOP) => {}
                        Err(err) => return Err(err.into()),
                    }
                }
                let path = self.path.join(&name);
                entries.push(classify(
                    Some(self.fd.as_fd()),
                    name,
                    &path,
                    &stat,
                    Some(self.dev),
                    followed,
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

    use super::{Bounds, Entry, Kind};
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
            target: None,
        }
    }

    /// Classifies `entry`, at `path`, by what it leads to if it is a link,
    /// leaving it as it is if the link leads nowhere. Only symbolic links are
    /// followed, so a link to a junction leads to the junction, which the
    /// OS would go through.
    fn follow_link(path: &Path, entry: Entry) -> Result<Entry> {
        // As many links as Windows itself follows in one path
        const MAX_LINKS: usize = 63;
        if entry.kind != Kind::Symlink {
            return Ok(entry);
        }
        let mut target = path.to_path_buf();
        for _ in 0..MAX_LINKS {
            let link = match std::fs::read_link(&target) {
                Ok(link) => link,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(entry),
                Err(err) => return Err(err.into()),
            };
            target = match target.parent() {
                Some(parent) => parent.join(link),
                None => link,
            };
            let (attrs, tag) = match Direct::probe_entry(&target) {
                Ok(probe) => probe,
                Err(err) if err.kind() == ErrorKind::NotFound => return Ok(entry),
                Err(err) => return Err(err),
            };
            let followed = classify(entry.name.clone(), attrs, tag);
            if followed.kind != Kind::Symlink {
                return Ok(Entry {
                    target: Some(target),
                    ..followed
                });
            }
        }
        // A loop of links, which leads nowhere
        Ok(entry)
    }

    /// Classifies `path`, following it with `follow` unless it is a dangling
    /// link.
    pub(super) fn root_entry(path: &Path, follow: bool) -> Result<Entry> {
        let (attrs, tag) = Direct::probe_entry(path)?;
        let entry = classify(path.as_os_str().to_owned(), attrs, tag);
        if follow {
            follow_link(path, entry)
        } else {
            Ok(entry)
        }
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
    /// as an empty directory. A followed link's stopping point is what it
    /// leads to.
    pub(super) fn copy_stopped(src: &Path, dst: &Path, entry: &Entry) -> Result<()> {
        let src = entry.target.as_deref().unwrap_or(src);
        match entry.level {
            Some(Resolve::Link | Resolve::Mount) => {
                Direct::copy_reparse_point_sync(src, dst, entry.dir)
            }
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
        /// Whether the walk follows symbolic links.
        follow: bool,
    }

    impl WalkDir {
        pub(super) fn open(path: &Path, bounds: Bounds) -> Result<Self> {
            let handle = win_dir::open_dir(path, true, false)?;
            let handle: OwnedHandle = match Direct::reparse_tag(handle.as_handle())? {
                None => handle,
                Some(tag) if !bounds.stops(Resolve::of_reparse_tag(tag)) => {
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
                follow: bounds.follow,
            })
        }

        pub(super) fn open_child(&self, entry: &Entry, bounds: Bounds) -> Result<Self> {
            Self::open(&self.path.join(&entry.name), bounds)
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
                let mut entry = classify(raw.name, raw.attrs, raw.tag);
                if self.follow {
                    entry = follow_link(&self.path.join(&entry.name), entry)?;
                }
                entries.push(entry);
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

    use super::{Bounds, Kind, WalkDir, check_removable, root_entry};
    use crate::{
        error::ErrorKind,
        metadata::{Resolve, Stop},
    };

    #[test]
    fn classifies_mount_points() {
        for path in ["/", "/proc"] {
            let entry = root_entry(Path::new(path), false).unwrap();
            assert_eq!(entry.kind, Kind::Dir, "{path}");
            assert_eq!(entry.level, Some(Resolve::Mount), "{path}");
        }
        let mut root = WalkDir::open(Path::new("/"), Bounds::remove(Stop::Cross)).unwrap();
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
        let mut walk = WalkDir::open(dir.path(), Bounds::remove(Stop::Cross)).unwrap();
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
        let err = check_removable(Path::new("/proc"), Stop::Cross).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ResourceBusy);
        let dir = crate::test_temp::tempdir().unwrap();
        check_removable(dir.path(), Stop::Cross).unwrap();
    }
}
