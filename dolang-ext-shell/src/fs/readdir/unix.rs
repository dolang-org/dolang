//! Unix implementation of async directory enumeration with fdopendir support.

use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::OwnedFd;
use std::path::{Path, PathBuf};

use nix::dir::Dir as NixDir;
use nix::dir::OwningIter;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use tokio::task;

/// Reads the entries in a directory.
///
/// This is a tokio-compatible async wrapper around the `nix` crate's directory
/// operations, with the ability to open a directory from a file descriptor.
#[derive(Debug)]
pub(crate) struct ReadDir {
    /// The owning iterator that holds the Dir. None when exhausted.
    iter: Option<OwningIter>,
    path: PathBuf,
}

impl ReadDir {
    /// Open a directory from an existing file descriptor.
    ///
    /// This corresponds to the fdopendir() functionality.
    pub(crate) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let nix_dir = NixDir::from_fd(fd).map_err(io::Error::other)?;
        let iter = nix_dir.into_iter();

        Ok(Self {
            iter: Some(iter),
            path: PathBuf::new(),
        })
    }

    /// Returns the next entry in the directory stream.
    pub(crate) async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        let path = self.path.clone();

        // Take the iterator out of self to move it into spawn_blocking
        let mut iter = match self.iter.take() {
            Some(iter) => iter,
            None => return Ok(None), // Already exhausted
        };

        let (result, iter) = task::spawn_blocking(move || {
            // Keep getting entries until we find one that's not . or .., or run out
            loop {
                match iter.next() {
                    Some(Ok(entry)) => {
                        let name = entry.file_name().to_bytes();
                        // Skip . and .. entries
                        if name != b"." && name != b".." {
                            return (Ok(Some(DirEntry::from_nix(entry, path))), Some(iter));
                        }
                        // Otherwise continue to next entry
                    }
                    Some(Err(e)) => return (Err(io::Error::other(e)), Some(iter)),
                    None => return (Ok(None), None),
                }
            }
        })
        .await
        .map_err(io::Error::other)?;

        // Put the iterator back if we got it back
        self.iter = iter;

        result
    }
}

/// Entries returned by the ReadDir stream.
#[derive(Debug)]
pub(crate) struct DirEntry {
    entry: nix::dir::Entry,
    dir_path: PathBuf,
}

impl DirEntry {
    fn from_nix(entry: nix::dir::Entry, dir_path: PathBuf) -> Self {
        Self { entry, dir_path }
    }

    /// Returns the bare file name of this directory entry without any other
    /// leading path component.
    pub(crate) fn file_name(&self) -> OsString {
        OsString::from_vec(self.entry.file_name().to_bytes().to_vec())
    }

    /// Returns the full path to the file that this entry represents.
    pub(crate) fn path(&self) -> PathBuf {
        self.dir_path.join(self.file_name())
    }

    /// Returns the underlying d_ino field in the contained dirent structure.
    pub(crate) fn ino(&self) -> u64 {
        self.entry.ino()
    }

    /// Returns the underlying d_type field in the contained dirent structure.
    pub(crate) fn unix_file_type(&self) -> Option<nix::dir::Type> {
        self.entry.file_type()
    }

    /// Returns the metadata for the file that this entry points at.
    ///
    /// This function will not traverse symlinks if this entry points at a
    /// symlink.
    pub(crate) async fn metadata(&self) -> io::Result<Metadata> {
        let path = self.path();
        task::spawn_blocking(move || fs::symlink_metadata(&path))
            .await
            .map_err(io::Error::other)?
    }

    /// Returns the file type for the file that this entry points at.
    ///
    /// This function will not traverse symlinks if this entry points at a
    /// symlink.
    pub(crate) async fn file_type(&self) -> io::Result<fs::FileType> {
        let metadata = self.metadata().await?;
        Ok(metadata.file_type())
    }
}

/// Reads the entries in a directory.
///
/// This is the standard way to open a directory for reading by path.
pub(crate) async fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
    let path = path.as_ref().to_path_buf();

    task::spawn_blocking(move || {
        let nix_dir =
            NixDir::open(&path, OFlag::O_DIRECTORY, Mode::empty()).map_err(io::Error::other)?;
        let iter = nix_dir.into_iter();

        Ok(ReadDir {
            iter: Some(iter),
            path,
        })
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[tokio::test]
    async fn read_dir_from_fd() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_path = temp_dir.path();

        // Create some test files
        std::fs::write(test_path.join("file1.txt"), "content1").unwrap();
        std::fs::write(test_path.join("file2.txt"), "content2").unwrap();

        // Open the directory and get an fd
        let dir_file = std::fs::File::open(test_path).unwrap();

        let mut entries = Vec::new();
        let mut dir = ReadDir::from_fd(dir_file.into()).unwrap();

        while let Some(entry) = dir.next_entry().await.unwrap() {
            entries.push(entry.file_name());
        }

        assert_eq!(entries.len(), 2);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"file1.txt".to_string()));
        assert!(names.contains(&"file2.txt".to_string()));
    }

    #[tokio::test]
    async fn dir_entry_ino() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_path = temp_dir.path();
        let file_path = test_path.join("test.txt");
        std::fs::write(&file_path, "content").unwrap();

        let mut dir = super::read_dir(test_path).await.unwrap();

        // Find the test.txt entry
        let mut found = false;
        while let Some(entry) = dir.next_entry().await.unwrap() {
            if entry.file_name().to_string_lossy() == "test.txt" {
                let ino = entry.ino();
                assert!(ino > 0);

                // Verify it matches the actual file inode
                let metadata = std::fs::metadata(&file_path).unwrap();
                assert_eq!(ino, metadata.ino());
                found = true;
                break;
            }
        }
        assert!(found, "test.txt not found in directory");
    }
}
