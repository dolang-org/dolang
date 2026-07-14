use std::{
    io::{self, SeekFrom},
    mem, result, str,
};

use bstr::ByteSlice;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, call, method,
    object::TypeBuilder,
    unpack,
    value::{BinEmbryo, TypeObject, View},
};
use dolang_shell_vfs::{AnyFile, FileHandle, OpenOptions, Utf8TypedPath, Vfs};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::{
    error::{ErrorExt as _, ResultExt as _},
    fs::{
        fs_metadata::create_fs_metadata, metadata::create_metadata, read_all, read_into_spare,
        stream, xattr,
    },
    global::Global,
    util,
};

const CHUNK_SIZE: usize = 8192;

/// Configure OpenOptions based on mode string (supports 'b' suffix for binary mode).
fn configure_options(opts: &mut impl OpenOptions, mode: &str) {
    // Strip 'b' suffix for binary mode - it doesn't affect file opening
    match mode.strip_suffix('b').unwrap_or(mode) {
        "r" => {
            opts.read(true);
        }
        "w" => {
            opts.write(true).truncate(true).create(true);
        }
        "a" => {
            opts.write(true).append(true).create(true);
        }
        "r+" => {
            opts.read(true).write(true);
        }
        "w+" => {
            opts.read(true).write(true).truncate(true).create(true);
        }
        "a+" => {
            opts.read(true).write(true).append(true).create(true);
        }
        _ => {
            // Invalid mode - will be handled by the caller
        }
    }
}

fn maximal_utf8_prefix(bytes: &[u8]) -> result::Result<&str, ()> {
    match str::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => {
            if e.error_len().is_none() {
                Ok(unsafe { str::from_utf8_unchecked(&bytes[0..e.valid_up_to()]) })
            } else {
                Err(())
            }
        }
    }
}

/// A handle to an open file.
pub(crate) struct File<'v> {
    file: Option<AnyFile>,
    buf: BinEmbryo<'v>,
}

pub(crate) struct FileAnnex<'v> {
    global: State<'v, Global<'v>>,
    is_binary: bool,
}

pub(crate) async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    mode: &str,
) -> Result<'v, 's, AnyFile> {
    let path = super::prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let mut opts = vfs.open_options();
    configure_options(&mut opts, mode);
    opts.open(path.to_path()).await.into_sys(strand)
}

pub(crate) async fn open_native<'v>(
    strand: &Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    mode: &str,
) -> io::Result<AnyFile> {
    let local = global.local.get(strand);
    let path = local.cwd().join(path.as_str());
    let vfs = local.vfs();
    let mut opts = vfs.open_options();
    configure_options(&mut opts, mode);
    opts.open(path.to_path())
        .await
        .map_err(dolang_shell_vfs::Error::into_io_error)
}

impl<'v> File<'v> {
    pub(crate) fn create(
        global: State<'v, Global<'v>>,
        file: AnyFile,
        is_binary: bool,
    ) -> (Self, FileAnnex<'v>) {
        (
            File {
                file: Some(file),
                buf: BinEmbryo::new(),
            },
            FileAnnex { global, is_binary },
        )
    }

    pub(crate) async fn command_handle<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Option<AnyFile>> {
        let borrow = this.borrow(strand)?;
        if !borrow.buf.is_empty() {
            return Ok(None);
        }
        let file_ref = borrow
            .file
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        file_ref.try_clone().await.map(Some).into_sys(strand)
    }

    async fn logical_position<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, u64> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let pos = file_ref.stream_position().await.into_sys(strand)?;
        pos.checked_sub(self.buf.len() as u64)
            .ok_or_else(|| Error::runtime(strand, "file cursor is before buffered data"))
    }

    async fn seek_to<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        seek_from: SeekFrom,
    ) -> Result<'v, 's, u64> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let pos = file_ref.seek(seek_from).await.into_sys(strand)?;
        self.buf.truncate(0);
        Ok(pos)
    }

    pub(crate) async fn open<'s>(
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        path: Utf8TypedPath<'_>,
        opt1: Option<Slot<'v, '_>>,
        opt2: Option<Slot<'v, '_>>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        // Determine mode and block
        let (mode, block) = match (&opt1, &opt2) {
            (None, None) => ("r".to_string(), None),
            (Some(slot), None) => {
                // Single arg: check if it's a mode string or block callable
                if let Some(mode) = slot.as_str(strand) {
                    (mode.to_string(), None)
                } else {
                    ("r".to_string(), Some(slot))
                }
            }

            (Some(slot1), Some(slot2)) => {
                // Two args: first must be mode, second is block
                let mode = slot1
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "mode must be a string"))?
                    .to_string();
                (mode, Some(slot2))
            }
            (None, Some(_)) => unreachable!(),
        };

        // Validate mode string (strip 'b' suffix for validation)
        let base_mode = mode.strip_suffix('b').unwrap_or(&mode);
        match base_mode {
            "r" | "w" | "a" | "r+" | "w+" | "a+" => {}
            _ => {
                return Err(Error::value(strand, format!("invalid mode: {}", mode)));
            }
        }

        let file = open(strand, global, path, &mode).await?;

        if let Some(block) = block {
            strand
                .with_slots(async move |strand, [mut handle, mut tmp]| {
                    // Block scope mode: create handle, call block with auto-close
                    let (file, annex) = File::create(global, file, mode.contains('b'));
                    global
                        .types
                        .file
                        .create_with_annex(strand, file, annex, &mut handle);

                    // Call the block with the handle as argument
                    let result = call!(strand, block, out, &handle).await;

                    // Always close the file, even on error
                    let _ = method!(strand, &handle, global.syms.close, &mut tmp).await;

                    result
                })
                .await
        } else {
            // No block: just return the handle in the slot
            let (file, annex) = File::create(global, file, mode.contains('b'));
            global
                .types
                .file
                .create_with_annex(strand, file, annex, out);
            Ok(())
        }
    }

    async fn fill_buf<'s>(&mut self, strand: &mut Strand<'v, 's>, n: usize) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let buf = &mut self.buf;
        if buf.len() >= n {
            return Ok(());
        }
        if n > buf.capacity() {
            buf.reserve(strand, n - buf.len())
        }
        let read = read_into_spare(file_ref, buf.spare_capacity_mut())
            .await
            .into_sys(strand)?;
        unsafe { buf.advance(read) };
        Ok(())
    }

    async fn read_binary<'s>(
        &mut self,
        n: usize,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.fill_buf(strand, n).await?;
        let buf = mem::take(&mut self.buf);
        buf.finish(strand, out);
        Ok(())
    }

    async fn read_binary_all<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut buf = mem::take(&mut self.buf);
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        read_all(strand, file, &mut buf).await?;
        buf.finish(strand, out);
        Ok(())
    }

    async fn read_text<'s>(
        &mut self,
        n: usize,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.fill_buf(strand, n).await?;
        match maximal_utf8_prefix(self.buf.as_slice()) {
            Ok(s) => {
                let consumed = s.len();
                let rem = self.buf.len() - consumed;
                let mut buf =
                    mem::replace(&mut self.buf, BinEmbryo::new_with_capacity(strand, rem));
                self.buf.extend(strand, &buf.as_slice()[consumed..]);
                buf.truncate(consumed);
                unsafe { buf.finish_str_unchecked(strand, out) };
                Ok(())
            }
            Err(()) => Err(Error::runtime(strand, "invalid UTF-8 data")),
        }
    }

    async fn read_text_all<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut buf = mem::take(&mut self.buf);
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        read_all(strand, file, &mut buf).await?;
        buf.finish_str(strand, out)
            .map_err(|_| Error::runtime(strand, "invalid UTF-8 data"))
    }

    async fn write<'a, 's>(
        &mut self,
        data: Slot<'v, 'a>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

        let bytes_written = match data.view(strand) {
            View::Str(s) => {
                let s = s.pin();
                file.write_all(s.as_bytes()).await.map(|_| s.len())
            }
            View::Bin(b) => {
                let b = b.pin();
                file.write_all(&b).await.map(|_| b.len())
            }
            _ => return Err(Error::type_error(strand, "expected `str` or `bin`")),
        }
        .into_sys(strand)?;

        Output::set(strand, out, bytes_written);
        Ok(())
    }

    async fn metadata<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

        let metadata = file_ref.metadata().await.into_sys(strand)?;
        create_metadata(strand, global, metadata, out);
        Ok(())
    }

    async fn fs_metadata<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

        let metadata = file_ref.fs_metadata().await.into_sys(strand)?;
        create_fs_metadata(strand, global, metadata, out);
        Ok(())
    }
}

impl<'v> Object<'v> for File<'v> {
    const NAME: &'v str = "File";
    const MODULE: &'v str = "fs";
    type Annex = FileAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

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
        let is_binary = this.annex().is_binary;

        if is_binary {
            // Binary mode: read a chunk of data
            let mut buf = mem::take(&mut borrow.buf);
            buf.reserve(strand, CHUNK_SIZE.saturating_sub(buf.len()));

            let file_ref = borrow
                .file
                .as_mut()
                .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

            match read_into_spare(file_ref, buf.spare_capacity_mut()).await {
                Ok(0) => {
                    borrow.buf = buf;
                    Ok(false)
                }
                Ok(read) => {
                    unsafe { buf.advance(read) };
                    buf.finish(strand, out);
                    Ok(true)
                }
                Err(e) => {
                    borrow.buf = buf;
                    Err(e.into_sys(strand))
                }
            }
        } else {
            // Text mode: read a line using buffered approach
            // Take ownership of the buffer temporarily
            let mut buf = mem::take(&mut borrow.buf);

            loop {
                // Check if we already have a complete line in the buffer
                if let Some((line, _rest)) = buf.as_slice().split_once_str(b"\n") {
                    let line_len = line.len();
                    borrow.buf = BinEmbryo::new_with_capacity(strand, buf.len() - (line_len + 1));
                    borrow.buf.extend(strand, &buf.as_slice()[line_len + 1..]);
                    buf.truncate(line_len - line.ends_with(b"\r") as usize);
                    buf.finish_str(strand, out)
                        .map_err(|_| Error::runtime(strand, "invalid UTF-8"))?;
                    return Ok(true);
                }

                // Need to read more data
                buf.reserve(strand, CHUNK_SIZE);

                let file_ref = borrow
                    .file
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

                match read_into_spare(file_ref, buf.spare_capacity_mut()).await {
                    Ok(0) => {
                        // EOF reached
                        if buf.is_empty() {
                            borrow.buf = buf;
                            return Ok(false);
                        } else {
                            buf.finish_str(strand, out)
                                .map_err(|_| Error::runtime(strand, "invalid UTF-8"))?;
                            return Ok(true);
                        }
                    }
                    Ok(read) => {
                        unsafe { buf.advance(read) };
                        // Continue loop to check for newline
                        continue;
                    }
                    Err(e) => {
                        borrow.buf = buf;
                        return Err(e.into_sys(strand));
                    }
                }
            }
        }
    }

    async fn output<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let is_binary = this.annex().is_binary;
        let file = borrow
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

        if is_binary {
            // Binary mode: write bytes directly
            if let Some(s) = value.as_str(strand) {
                file.write_all(s.pin().as_bytes()).await.into_sys(strand)?;
            } else {
                let s = value.to_string(strand)?;
                file.write_all(s.as_bytes()).await.into_sys(strand)?;
            }
        } else {
            // Text mode: check if value is binary
            if let Some(b) = value.as_bin(strand) {
                // It's binary data, write as-is
                file.write_all(&b.pin()).await.into_sys(strand)?;
            } else {
                // Not binary, convert to string and add newline
                let s = value.to_string(strand)?;
                file.write_all(s.as_bytes()).await.into_sys(strand)?;
                file.write_all(b"\n").await.into_sys(strand)?;
            }
        }
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let start_sym = builder.sym("start");
        let end_sym = builder.sym("end");
        let namespace = builder.sym("namespace");
        let any = builder.sym("ANY");
        builder
            .supertype(TypeObject::Iter)
            .supertype(TypeObject::Sink)
            .method("close", async move |this, strand, _args, _out| {
                let mut borrow = this.borrow_mut(strand)?;
                if let Some(file) = borrow.file.take() {
                    file.close().await.into_sys(strand)?
                }
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([], [size]) = unpack!(strand, args, 0, 1)?;
                let size: Option<usize> = size
                    .map(|s| {
                        s.to_i64(strand)
                            .ok()
                            .and_then(|n| usize::try_from(n).ok())
                            .ok_or_else(|| {
                                Error::type_error(strand, "size must be a non-negative integer")
                            })
                    })
                    .transpose()?;

                let mut borrow = this.borrow_mut(strand)?;
                let is_binary = this.annex().is_binary;

                match (is_binary, size) {
                    (true, Some(n)) => borrow.read_binary(n, strand, out).await,
                    (true, None) => borrow.read_binary_all(strand, out).await,
                    (false, Some(n)) => borrow.read_text(n, strand, out).await,
                    (false, None) => borrow.read_text_all(strand, out).await,
                }
            })
            .method("write", async move |this, strand, args, out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                this.borrow_mut(strand)?.write(data, strand, out).await
            })
            .method("set_len", async move |this, strand, args, _out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = size.to_i64(strand).map_err(|_| {
                    Error::type_error(strand, "size must be a non-negative integer")
                })?;
                let size = u64::try_from(size).map_err(|_| {
                    Error::type_error(strand, "size must be a non-negative integer")
                })?;

                let mut borrow = this.borrow_mut(strand)?;
                let pos = borrow.logical_position(strand).await?;
                {
                    let file = borrow
                        .file
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                    file.set_len(size).await.into_sys(strand)?;
                }
                borrow.seek_to(strand, SeekFrom::Start(pos)).await?;
                Ok(())
            })
            .method("metadata", async move |this, strand, _args, out| {
                this.borrow_mut(strand)?
                    .metadata(strand, this.annex().global, out)
                    .await
            })
            .method("fs_metadata", async move |this, strand, _args, out| {
                this.borrow_mut(strand)?
                    .fs_metadata(strand, this.annex().global, out)
                    .await
            })
            .method("xattrs", async move |this, strand, args, out| {
                let ([], [namespace]) = unpack!(strand, args, 0, 0, namespace = None)?;
                let global = this.annex().global;
                let (namespace, any) = match namespace {
                    None => (None, false),
                    Some(namespace) => {
                        if let Some(sym) = namespace.as_sym(strand) {
                            if sym == any {
                                (None, true)
                            } else {
                                return Err(Error::value(
                                    strand,
                                    "namespace: expected str or :ANY:",
                                ));
                            }
                        } else if let Some(namespace) = namespace.as_str(strand) {
                            (Some(namespace.to_string()), false)
                        } else {
                            return Err(Error::type_error(
                                strand,
                                "namespace: expected str or sym",
                            ));
                        }
                    }
                };
                let entries = {
                    let mut borrow = this.borrow_mut(strand)?;
                    let file = borrow
                        .file
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                    file.xattrs(if any {
                        dolang_shell_vfs::XattrNamespace::Any
                    } else if let Some(ref namespace) = namespace {
                        dolang_shell_vfs::XattrNamespace::Named(namespace)
                    } else {
                        dolang_shell_vfs::XattrNamespace::Default
                    })
                    .await
                    .into_sys(strand)?
                };
                xattr::create_xattr_iter(strand, global, entries, out)
            })
            .method("xattr", async move |this, strand, args, out| {
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                let global = this.annex().global;
                let (name, namespace) = xattr::parse_name(strand, global, &name, None)?;
                let value = {
                    let mut borrow = this.borrow_mut(strand)?;
                    let file = borrow
                        .file
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                    file.xattr(&name, namespace.as_deref())
                        .await
                        .into_sys(strand)?
                };
                Output::set(strand, out, value.as_slice());
                Ok(())
            })
            .method("streams", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = this.annex().global;
                let entries = {
                    let mut borrow = this.borrow_mut(strand)?;
                    let file = borrow
                        .file
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                    file.streams().await.into_sys(strand)?
                };
                stream::create_stream_iter(strand, global, entries, out)
            })
            .method("set_xattr", async move |this, strand, args, _out| {
                let ([name, value], []) = unpack!(strand, args, 2, 0)?;
                let global = this.annex().global;
                let (name, namespace) = xattr::parse_name(strand, global, &name, None)?;
                let value = util::bytes(strand, &value, "value")?;
                let mut borrow = this.borrow_mut(strand)?;
                let file = borrow
                    .file
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                file.set_xattr(&name, namespace.as_deref(), &value)
                    .await
                    .into_sys(strand)
            })
            .method("remove_xattr", async move |this, strand, args, _out| {
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                let global = this.annex().global;
                let (name, namespace) = xattr::parse_name(strand, global, &name, None)?;
                let mut borrow = this.borrow_mut(strand)?;
                let file = borrow
                    .file
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                file.remove_xattr(&name, namespace.as_deref())
                    .await
                    .into_sys(strand)
            })
            .method("tell", async move |this, strand, _args, out| {
                let mut borrow = this.borrow_mut(strand)?;
                let pos = borrow.logical_position(strand).await?;
                Output::set(strand, out, i128::from(pos));
                Ok(())
            })
            .method("seek", async move |this, strand, args, out| {
                let ([], [offset, start, end]) =
                    unpack!(strand, args, 0, 1, start_sym = None, end_sym = None)?;
                let mut borrow = this.borrow_mut(strand)?;
                let seek_from = match (offset, start, end) {
                    (Some(offset), None, None) => {
                        let offset = offset.to_i64(strand).map_err(|_| {
                            Error::type_error(strand, "seek offset must be an integer")
                        })?;
                        let buffered = i64::try_from(borrow.buf.len()).map_err(|_| {
                            Error::runtime(strand, "file buffer is too large to seek")
                        })?;
                        SeekFrom::Current(offset - buffered)
                    }
                    (None, Some(start), None) => {
                        let start = start
                            .to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "start must be an integer"))?;
                        SeekFrom::Start(u64::try_from(start).map_err(|_| {
                            Error::runtime(strand, "start offset must be non-negative")
                        })?)
                    }
                    (None, None, Some(end)) => SeekFrom::End(
                        end.to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "end must be an integer"))?,
                    ),
                    (None, None, None) => {
                        return Err(Error::missing_positional(strand, 0));
                    }
                    (Some(_), Some(_), _) => {
                        return Err(Error::unexpected_key(strand, start_sym));
                    }
                    (Some(_), None, Some(_)) | (None, Some(_), Some(_)) => {
                        return Err(Error::unexpected_key(strand, end_sym));
                    }
                };
                let pos = borrow.seek_to(strand, seek_from).await?;

                Output::set(strand, out, i128::from(pos));
                Ok(())
            })
    }
}
