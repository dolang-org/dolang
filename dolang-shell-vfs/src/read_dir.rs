use std::{io, path::Path};

#[cfg(unix)]
use nix::{
    dir::{Dir as NixDir, OwningIter, Type},
    fcntl::OFlag,
    sys::stat::Mode,
};
#[cfg(unix)]
use std::{
    ffi::OsString,
    os::unix::{ffi::OsStringExt, io::OwnedFd},
};

use crate::{DirEntry, FileType};

#[cfg(unix)]
#[derive(Debug)]
pub struct ReadDir {
    iter: Option<OwningIter>,
}

#[cfg(windows)]
#[derive(Debug)]
pub struct ReadDir {
    inner: tokio::fs::ReadDir,
}

#[cfg(unix)]
impl ReadDir {
    pub(crate) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let nix_dir = NixDir::from_fd(fd).map_err(io::Error::other)?;
        Ok(Self {
            iter: Some(nix_dir.into_iter()),
        })
    }

    pub(crate) async fn open(path: &Path) -> io::Result<Self> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let nix_dir =
                NixDir::open(&path, OFlag::O_DIRECTORY, Mode::empty()).map_err(io::Error::other)?;
            Ok(Self {
                iter: Some(nix_dir.into_iter()),
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        let mut iter: OwningIter = match self.iter.take() {
            Some(iter) => iter,
            None => return Ok(None),
        };

        let (result, iter): (io::Result<Option<DirEntry>>, Option<OwningIter>) =
            tokio::task::spawn_blocking(move || {
                loop {
                    match iter.next() {
                        Some(Ok(entry)) => {
                            let name = entry.file_name().to_bytes();
                            if name != b"." && name != b".." {
                                let file_type = entry
                                    .file_type()
                                    .map(|ty| match ty {
                                        Type::File => FileType::File,
                                        Type::Directory => FileType::Dir,
                                        Type::Symlink => FileType::Symlink,
                                        Type::Fifo => FileType::Fifo,
                                        Type::CharacterDevice => FileType::CharacterDevice,
                                        Type::BlockDevice => FileType::BlockDevice,
                                        Type::Socket => FileType::Socket,
                                    })
                                    .unwrap_or(FileType::Unknown);

                                return (
                                    Ok(Some(DirEntry {
                                        file_name: OsString::from_vec(name.to_vec()),
                                        ino: entry.ino(),
                                        file_type,
                                    })),
                                    Some(iter),
                                );
                            }
                        }
                        Some(Err(e)) => return (Err(io::Error::other(e)), Some(iter)),
                        None => return (Ok(None), None),
                    }
                }
            })
            .await
            .map_err(io::Error::other)?;

        self.iter = iter;
        result
    }
}

#[cfg(windows)]
impl ReadDir {
    pub(crate) async fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: tokio::fs::read_dir(path).await?,
        })
    }

    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        let Some(entry) = self.inner.next_entry().await? else {
            return Ok(None);
        };
        let file_type = entry.file_type().await?;
        let file_type = if file_type.is_file() {
            FileType::File
        } else if file_type.is_dir() {
            FileType::Dir
        } else if file_type.is_symlink() {
            FileType::Symlink
        } else {
            FileType::Unknown
        };
        Ok(Some(DirEntry {
            file_name: entry.file_name(),
            ino: 0,
            file_type,
        }))
    }
}
