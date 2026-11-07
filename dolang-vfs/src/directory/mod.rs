use crate::{client, direct, error::Result, metadata::FileType};
use serde::{Deserialize, Serialize};

/// An entry returned by [`ReadDir`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    file_name: String,
    file_type: FileType,
    family: DirEntryFamily,
}

/// Platform-specific fields carried by a directory entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DirEntryFamily {
    /// Unix-specific entry information.
    Unix { ino: u64 },
    /// Windows entry information.
    Windows {
        /// The reparse tag, if the entry is a reparse point.
        reparse_tag: Option<u32>,
    },
}

impl DirEntry {
    pub(crate) fn new(file_name: String, file_type: FileType, family: DirEntryFamily) -> Self {
        Self {
            file_name,
            file_type,
            family,
        }
    }

    /// Returns the entry name without its parent path.
    pub fn file_name(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new(&self.file_name)
    }

    /// Returns the inode number when the target is Unix-like.
    pub fn ino(&self) -> Option<u64> {
        match self.family {
            DirEntryFamily::Unix { ino } => Some(ino),
            DirEntryFamily::Windows { .. } => None,
        }
    }

    /// Returns the reparse tag when the entry is a Windows reparse point.
    pub fn reparse_tag(&self) -> Option<u32> {
        match self.family {
            DirEntryFamily::Windows { reparse_tag } => reparse_tag,
            DirEntryFamily::Unix { .. } => None,
        }
    }

    /// Returns the entry's file type.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }
}

#[derive(Debug)]
enum ReadDirInner {
    Client(client::ReadDir),
    Direct(direct::ReadDir),
}

/// An asynchronous directory iterator.
#[derive(Debug)]
pub struct ReadDir {
    inner: ReadDirInner,
}

impl ReadDir {
    pub(crate) fn client(read_dir: client::ReadDir) -> Self {
        Self {
            inner: ReadDirInner::Client(read_dir),
        }
    }

    pub(crate) fn direct(read_dir: direct::ReadDir) -> Self {
        Self {
            inner: ReadDirInner::Direct(read_dir),
        }
    }

    /// Returns the next directory entry, or `None` after the iterator is exhausted.
    pub async fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        Ok(match &mut self.inner {
            ReadDirInner::Client(read_dir) => read_dir.next_entry().await?,
            ReadDirInner::Direct(read_dir) => read_dir.next_entry().await?,
        })
    }
}
