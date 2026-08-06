use std::{io, path::Path};

use crate::{DirEntry, DirEntryFamily, FileType};
#[cfg(unix)]
use nix::{
    dir::{Dir as NixDir, OwningIter, Type},
    fcntl::OFlag,
    sys::stat::Mode,
};

#[derive(Debug)]
pub struct ReadDir {
    inner: ReadDirInner,
}

#[derive(Debug)]
enum ReadDirInner {
    #[cfg(unix)]
    Unix(Option<OwningIter>),
    #[cfg(windows)]
    Windows(Box<tokio::fs::ReadDir>),
    Remote(std::vec::IntoIter<DirEntry>),
}

impl ReadDir {
    #[cfg(unix)]
    pub(crate) async fn open(path: &Path) -> io::Result<Self> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let nix_dir =
                NixDir::open(&path, OFlag::O_DIRECTORY, Mode::empty()).map_err(io::Error::other)?;
            Ok(Self {
                inner: ReadDirInner::Unix(Some(nix_dir.into_iter())),
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    #[cfg(windows)]
    pub(crate) async fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: ReadDirInner::Windows(Box::new(tokio::fs::read_dir(path).await?)),
        })
    }

    pub(crate) fn from_entries(entries: Vec<DirEntry>) -> Self {
        Self {
            inner: ReadDirInner::Remote(entries.into_iter()),
        }
    }

    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        match &mut self.inner {
            #[cfg(unix)]
            ReadDirInner::Unix(iter) => Self::next_unix(iter).await,
            #[cfg(windows)]
            ReadDirInner::Windows(inner) => Self::next_windows(inner).await,
            ReadDirInner::Remote(entries) => Ok(entries.next()),
        }
    }

    #[cfg(unix)]
    async fn next_unix(iter: &mut Option<OwningIter>) -> io::Result<Option<DirEntry>> {
        let mut owned_iter = match iter.take() {
            Some(iter) => iter,
            None => return Ok(None),
        };
        let (result, next_iter) = tokio::task::spawn_blocking(move || {
            loop {
                match owned_iter.next() {
                    Some(Ok(entry)) => {
                        let name = entry.file_name().to_bytes();
                        if name == b"." || name == b".." {
                            continue;
                        }
                        let file_name = match String::from_utf8(name.to_vec()) {
                            Ok(name) => name,
                            Err(error) => {
                                return (
                                    Err(io::Error::new(io::ErrorKind::InvalidData, error)),
                                    Some(owned_iter),
                                );
                            }
                        };
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
                                file_name,
                                file_type,
                                family: DirEntryFamily::Unix { ino: entry.ino() },
                            })),
                            Some(owned_iter),
                        );
                    }
                    Some(Err(error)) => {
                        return (Err(io::Error::other(error)), Some(owned_iter));
                    }
                    None => return (Ok(None), None),
                }
            }
        })
        .await
        .map_err(io::Error::other)?;
        *iter = next_iter;
        result
    }

    #[cfg(windows)]
    async fn next_windows(inner: &mut tokio::fs::ReadDir) -> io::Result<Option<DirEntry>> {
        let Some(entry) = inner.next_entry().await? else {
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
        let file_name = entry.file_name().into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory entry is not valid UTF-8",
            )
        })?;
        Ok(Some(DirEntry {
            file_name,
            file_type,
            family: DirEntryFamily::Windows,
        }))
    }
}
