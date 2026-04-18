#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
use std::{io, path, result, str};

use bstr::ByteSlice;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, call,
    error::ResultExt,
    method,
    object::TypeBuilder,
    unpack,
    value::{TypeObject, View},
};
use dolang_shell_vfs::{OpenOptions, Vfs};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};

use crate::{
    error::{ErrorExt as _, ResultExt as _},
    global::Global,
};

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
pub(crate) struct File {
    file: Option<fs::File>,
    buf: Vec<u8>,
}

pub(crate) struct FileAnnex<'v> {
    global: State<'v, Global<'v>>,
    is_binary: bool,
}

pub(crate) async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &path::Path,
    mode: &str,
) -> io::Result<fs::File> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    let mut opts = vfs.open_options();
    configure_options(&mut opts, mode);
    opts.open(&path).await
}

impl File {
    pub(crate) fn create<'v>(
        global: State<'v, Global<'v>>,
        file: fs::File,
        is_binary: bool,
    ) -> (Self, FileAnnex<'v>) {
        (
            File {
                file: Some(file),
                buf: Default::default(),
            },
            FileAnnex { global, is_binary },
        )
    }

    #[cfg(unix)]
    pub(crate) fn fd<'v, 'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Option<OwnedFd>> {
        let borrow = this.borrow(strand)?;
        if !borrow.buf.is_empty() {
            return Ok(None);
        }
        let file_ref = borrow
            .file
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        file_ref
            .as_fd()
            .try_clone_to_owned()
            .map(Some)
            .into_sys(strand)
    }

    pub(crate) async fn open<'v, 's>(
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        path: path::PathBuf,
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

        let file = open(strand, global, &path, &mode).await.into_sys(strand)?;

        if let Some(block) = block {
            strand
                .with_slots(async move |strand, [mut handle, mut tmp]| {
                    // Block scope mode: create handle, call block with auto-close
                    global.types.file.create_with_annex(
                        strand,
                        File {
                            file: Some(file),
                            buf: Default::default(),
                        },
                        FileAnnex {
                            global,
                            is_binary: mode.contains('b'),
                        },
                        &mut handle,
                    );

                    // Call the block with the handle as argument
                    let result = call!(strand, block, out, &handle).await;

                    // Always close the file, even on error
                    let _ = method!(strand, &handle, global.syms.close, &mut tmp).await;

                    result
                })
                .await
        } else {
            // No block: just return the handle in the slot
            global.types.file.create_with_annex(
                strand,
                File {
                    file: Some(file),
                    buf: Default::default(),
                },
                FileAnnex {
                    global,
                    is_binary: mode.contains('b'),
                },
                out,
            );
            Ok(())
        }
    }

    async fn fill_buf<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        n: usize,
    ) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let buf = &mut self.buf;
        if buf.len() >= n {
            return Ok(());
        }
        if buf.len() + buf.capacity() < n {
            buf.reserve(n - buf.len() - buf.capacity())
        }

        file_ref.read_buf(buf).await.into_sys(strand)?;
        Ok(())
    }

    async fn fill_buf_all<'v, 's>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let buf = &mut self.buf;
        file_ref.read_to_end(buf).await.into_sys(strand)?;
        Ok(())
    }

    async fn read_binary<'v, 's>(
        &mut self,
        n: usize,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.fill_buf(strand, n).await?;
        Output::set(strand, out, self.buf.as_slice());
        self.buf.clear();
        Ok(())
    }

    async fn read_binary_all<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.fill_buf_all(strand).await?;
        Output::set(strand, out, self.buf.as_slice());
        self.buf.clear();
        Ok(())
    }

    async fn read_text<'v, 's>(
        &mut self,
        n: usize,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.fill_buf(strand, n).await?;
        match maximal_utf8_prefix(&self.buf) {
            Ok(s) => {
                let consumed = s.len();
                Output::set(strand, out, s);
                self.buf.drain(..consumed);
                Ok(())
            }
            Err(()) => Err(Error::runtime(strand, "invalid UTF-8 data")),
        }
    }

    async fn read_text_all<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.fill_buf_all(strand).await?;
        match str::from_utf8(&self.buf) {
            Ok(s) => {
                Output::set(strand, out, s);
                self.buf.clear();
                Ok(())
            }
            Err(_) => Err(Error::runtime(strand, "invalid UTF-8 data")),
        }
    }

    async fn write<'v, 'a, 's>(
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

        Output::set(strand, out, bytes_written as i64);
        Ok(())
    }

    async fn metadata<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

        let metadata = global
            .local
            .get(strand)
            .vfs()
            .file_metadata(file_ref)
            .await
            .into_sys(strand)?;

        super::metadata_to_record(strand, global, &metadata, &mut out).await?;
        #[cfg(unix)]
        super::unix::unix_metadata_to_record(strand, global, &out, &metadata).await?;
        Ok(())
    }
}

impl<'v> Object<'v> for File {
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
            const CHUNK_SIZE: usize = 8192;
            let mut buf = std::mem::take(&mut borrow.buf);
            buf.reserve(CHUNK_SIZE.saturating_sub(buf.len()));

            let file_ref = borrow
                .file
                .as_mut()
                .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

            match file_ref.read_buf(&mut buf).await {
                Ok(0) => {
                    borrow.buf = buf;
                    Ok(false)
                }
                Ok(_) => {
                    Output::set(strand, out, buf.as_slice());
                    buf.clear();
                    borrow.buf = buf;
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
            let mut buf = std::mem::take(&mut borrow.buf);

            loop {
                // Check if we already have a complete line in the buffer
                if let Some((line, _rest)) = buf.split_once_str(b"\n") {
                    let line = str::from_utf8(line).into_do(strand)?;
                    Output::set(strand, out, line.strip_suffix('\r').unwrap_or(line));
                    // Drain the consumed bytes (line + \n) from the buffer
                    buf.drain(..line.len() + 1);
                    borrow.buf = buf;
                    return Ok(true);
                }

                // Need to read more data
                const CHUNK_SIZE: usize = 8192;
                buf.reserve(CHUNK_SIZE.saturating_sub(buf.len()));

                let file_ref = borrow
                    .file
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

                match file_ref.read_buf(&mut buf).await {
                    Ok(0) => {
                        // EOF reached
                        if buf.is_empty() {
                            borrow.buf = buf;
                            return Ok(false);
                        } else {
                            // Return final line without newline
                            let line = str::from_utf8(&buf).into_do(strand)?;
                            Output::set(strand, out, line);
                            buf.clear();
                            borrow.buf = buf;
                            return Ok(true);
                        }
                    }
                    Ok(_) => {
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

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            .supertype(TypeObject::Sink)
            .method("close", async move |this, strand, _args, _out| {
                let mut borrow = this.borrow_mut(strand)?;
                if let Some(file) = borrow.file.as_mut() {
                    file.flush().await.into_sys(strand)?;
                    borrow.file = None;
                }
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([], [size]) = unpack!(strand, args, 0, 1)?;
                let size: Option<usize> = size
                    .map(|s| {
                        s.as_i64(strand)
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
            .method("metadata", async move |this, strand, _args, out| {
                this.borrow_mut(strand)?
                    .metadata(strand, this.annex().global, out)
                    .await
            })
    }
}
