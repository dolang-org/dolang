//! Async directory enumeration API with fdopendir support.
//!
//! On Unix platforms, this provides a tokio-compatible ReadDir implementation
//! that can be instantiated from an OwnedFd (fdopendir functionality).
//! On Windows, this re-exports tokio::fs::{ReadDir, DirEntry, read_dir}.

#![allow(dead_code)]

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::ReadDir;
#[cfg(windows)]
pub(crate) use windows::ReadDir;

#[cfg(unix)]
pub(crate) use unix::DirEntry;
#[cfg(windows)]
pub(crate) use windows::DirEntry;

use std::path::PathBuf;

use dolang::runtime::{
    Instance, Object, Output, Result, Slot, State, Strand, method, object::TypeBuilder,
    value::TypeObject,
};

use crate::error::ErrorExt as ShellErrorExt;
use crate::global::Global;

use crate::fs::path::{Path, PathAnnex};

pub(crate) struct DirEntryIter {
    pub(crate) read_dir: ReadDir,
    pub(crate) path: PathBuf,
}

pub(crate) struct DirEntryIterAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for DirEntryIter {
    const NAME: &'v str = "DirEntryIter";
    const MODULE: &'v str = "fs";
    type Annex = DirEntryIterAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let annex = this.annex();
        let global = annex.global;

        match borrow.read_dir.next_entry().await {
            Ok(Some(entry)) => {
                entry_to_record(&entry, &borrow.path, global, strand, out).await?;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(e.into_sys(strand)),
        }
    }
}

async fn entry_to_record<'v, 's>(
    entry: &DirEntry,
    dir_path: &std::path::Path,
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async |strand, [mut std_mod, mut record, mut tmp]| {
            strand.import("std", &mut std_mod).await?;

            let file_name = entry.file_name();
            let entry_path = dir_path.join(&file_name);

            let path_sym = global.syms.path;
            let record_sym = global.syms.record;

            global.types.path.create_with_annex(
                strand,
                Path,
                PathAnnex::new(entry_path, global),
                &mut tmp,
            );

            method!(
                strand, std_mod, record_sym, &mut record,
                path_sym: &mut tmp,
            )
            .await?;

            #[cfg(unix)]
            {
                use nix::dir::Type;
                record.set(strand, global.syms.ino, entry.ino() as i64)?;
                if let Some(ty) = entry.unix_file_type() {
                    record.set(
                        strand,
                        global.syms.ty,
                        match ty {
                            Type::File => global.syms.file,
                            Type::Directory => global.syms.dir,
                            Type::Symlink => global.syms.symlink,
                            Type::Fifo => global.syms.fifo,
                            Type::CharacterDevice => global.syms.char_device,
                            Type::BlockDevice => global.syms.block_device,
                            Type::Socket => global.syms.socket,
                        },
                    )?;
                } else {
                    record.set(strand, global.syms.ty, global.syms.unknown)?;
                }
            }

            Output::set(strand, out, record);
            Ok(())
        })
        .await
}

#[cfg(test)]
mod test_common {
    // Re-export the platform-specific implementations for tests
    #[cfg(unix)]
    pub(crate) use super::unix::{ReadDir, read_dir};
    #[cfg(windows)]
    pub(crate) use super::windows::{ReadDir, read_dir};
}

#[cfg(test)]
pub(crate) mod tests {
    pub(crate) use super::test_common::*;

    #[tokio::test]
    async fn read_dir_from_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_path = temp_dir.path();

        // Create some test files
        std::fs::write(test_path.join("file1.txt"), "content1").unwrap();
        std::fs::write(test_path.join("file2.txt"), "content2").unwrap();
        std::fs::create_dir(test_path.join("subdir")).unwrap();

        let mut entries = Vec::new();
        let mut dir = read_dir(test_path).await.unwrap();

        while let Some(entry) = dir.next_entry().await.unwrap() {
            entries.push(entry.file_name());
        }

        assert_eq!(entries.len(), 3);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"file1.txt".to_string()));
        assert!(names.contains(&"file2.txt".to_string()));
        assert!(names.contains(&"subdir".to_string()));
    }

    #[tokio::test]
    async fn read_dir_empty_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_path = temp_dir.path();

        let mut dir = read_dir(test_path).await.unwrap();
        let entry = dir.next_entry().await.unwrap();
        assert!(entry.is_none());
    }

    #[tokio::test]
    async fn dir_entry_metadata() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_path = temp_dir.path();
        let file_path = test_path.join("test.txt");
        std::fs::write(&file_path, "test content").unwrap();

        let mut dir = read_dir(test_path).await.unwrap();

        // Find the test.txt entry
        let mut found = false;
        while let Some(entry) = dir.next_entry().await.unwrap() {
            if entry.file_name().to_string_lossy() == "test.txt" {
                let metadata = entry.metadata().await.unwrap();
                assert!(metadata.is_file());
                assert_eq!(metadata.len(), 12); // "test content" = 12 bytes
                found = true;
                break;
            }
        }
        assert!(found, "test.txt not found in directory");
    }

    #[tokio::test]
    async fn dir_entry_file_type() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_path = temp_dir.path();
        std::fs::write(test_path.join("file.txt"), "content").unwrap();
        std::fs::create_dir(test_path.join("dir")).unwrap();

        let mut dir = read_dir(test_path).await.unwrap();

        let mut found_file = false;
        let mut found_dir = false;

        while let Some(entry) = dir.next_entry().await.unwrap() {
            let file_type = entry.file_type().await.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();

            if name == "file.txt" {
                assert!(file_type.is_file());
                found_file = true;
            } else if name == "dir" {
                assert!(file_type.is_dir());
                found_dir = true;
            }
        }

        assert!(found_file);
        assert!(found_dir);
    }

    #[tokio::test]
    async fn read_dir_nonexistent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let nonexistent = temp_dir.path().join("does_not_exist");

        let result: std::io::Result<ReadDir> = read_dir(&nonexistent).await;
        assert!(result.is_err());
    }
}
