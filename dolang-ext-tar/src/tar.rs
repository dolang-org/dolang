use std::{
    io::SeekFrom,
    path::Path,
    pin::Pin,
    str,
    time::{Duration, SystemTime},
};

use async_compression::tokio::{
    bufread::{GzipDecoder, ZstdDecoder},
    write::{GzipEncoder, ZstdEncoder},
};
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, call,
    error::ResultExt,
    object::{Mut, Ref, TypeBuilder},
    unpack,
    value::{Nil, TypeObject, View},
    vm::Builder,
};
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream,
};
use tokio_stream::StreamExt;
use tokio_tar::{Archive, Builder as TarBuilder, Entries, Entry, EntryType, Header};

use crate::global::Global;

const CHUNK_SIZE: usize = 8192;
const GZIP_MAGIC: &[u8] = b"\x1f\x8b";
const ZSTD_MAGIC: &[u8] = b"\x28\xb5\x2f\xfd";

type DynReader = Pin<Box<dyn AsyncRead + Send>>;
type DynWriter = Pin<Box<dyn AsyncWrite + Send>>;
type NativeEntries = Entries<DynReader>;
type NativeEntry = Entry<Archive<DynReader>>;
type NativeBuilder = TarBuilder<DynWriter>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Zstd,
}

pub(crate) struct TarReader {
    entries: NativeEntries,
    current: Option<NativeEntry>,
    generation: u64,
    compression: Compression,
    closed: bool,
}

pub(crate) struct TarEntry {
    generation: u64,
}

pub(crate) struct TarWriter {
    builder: Option<NativeBuilder>,
    generation: u64,
    active: bool,
    poisoned: bool,
    compression: Compression,
}

pub(crate) struct TarEntryWriter {
    stream: Option<DuplexStream>,
    generation: u64,
    written: u64,
    size: u64,
}

fn compression_sym<'v>(
    global: State<'v, Global<'v>>,
    compression: Compression,
) -> dolang::runtime::Sym<'v, 'v> {
    match compression {
        Compression::None => global.syms.none,
        Compression::Gzip => global.syms.gzip,
        Compression::Zstd => global.syms.zstd,
    }
}

fn entry_type_sym<'v>(
    global: State<'v, Global<'v>>,
    ty: EntryType,
) -> dolang::runtime::Sym<'v, 'v> {
    match ty {
        EntryType::Regular => global.syms.file,
        EntryType::Link => global.syms.hardlink,
        EntryType::Symlink => global.syms.symlink,
        EntryType::Char => global.syms.char_device,
        EntryType::Block => global.syms.block_device,
        EntryType::Directory => global.syms.dir,
        EntryType::Fifo => global.syms.fifo,
        EntryType::Continuous => global.syms.contiguous,
        _ => global.syms.unknown,
    }
}

fn parse_entry_type<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Option<Slot<'v, '_>>,
) -> Result<'v, 's, EntryType> {
    let Some(value) = value else {
        return Ok(EntryType::Regular);
    };
    match value.as_sym(strand) {
        Some(sym) if sym == global.syms.file => Ok(EntryType::Regular),
        Some(sym) if sym == global.syms.hardlink => Ok(EntryType::Link),
        Some(sym) if sym == global.syms.symlink => Ok(EntryType::Symlink),
        Some(sym) if sym == global.syms.char_device => Ok(EntryType::Char),
        Some(sym) if sym == global.syms.block_device => Ok(EntryType::Block),
        Some(sym) if sym == global.syms.dir => Ok(EntryType::Directory),
        Some(sym) if sym == global.syms.fifo => Ok(EntryType::Fifo),
        Some(sym) if sym == global.syms.contiguous => Ok(EntryType::Continuous),
        _ => Err(Error::value(strand, "invalid tar entry type")),
    }
}

fn nonnegative_u64<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    name: &str,
) -> Result<'v, 's, u64> {
    let value = value
        .to_int(strand)
        .map_err(|_| Error::type_error(strand, format!("{name} must be a non-negative integer")))?;
    u64::try_from(value)
        .map_err(|_| Error::value(strand, format!("{name} must be a non-negative integer")))
}

fn optional_u64<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<Slot<'v, '_>>,
    default: u64,
    name: &str,
) -> Result<'v, 's, u64> {
    value
        .map(|value| nonnegative_u64(strand, value, name))
        .unwrap_or(Ok(default))
}

fn optional_string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<Slot<'v, '_>>,
    name: &str,
) -> Result<'v, 's, Option<String>> {
    value
        .map(|value| {
            value
                .as_str(strand)
                .map(|value| value.to_string())
                .ok_or_else(|| Error::type_error(strand, format!("{name} must be a string")))
        })
        .transpose()
}

fn unix_path_string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    name: &str,
) -> Result<'v, 's, String> {
    if let Some(path) = dolang_ext_shell::as_unix_path(strand, &value) {
        Ok(path.as_str().to_owned())
    } else if let Some(path) = value.as_str(strand) {
        Ok(path.to_string())
    } else {
        Err(Error::type_error(
            strand,
            format!("{name} must be a str or UnixPath"),
        ))
    }
}

fn optional_unix_path_string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<Slot<'v, '_>>,
    name: &str,
) -> Result<'v, 's, Option<String>> {
    value
        .map(|value| unix_path_string(strand, value, name))
        .transpose()
}

macro_rules! active_entry {
    ($this:expr, $strand:expr, $entry:ident) => {
        let child = $this.borrow($strand)?;
        let reader = $this
            .annex()
            .types
            .reader
            .downcast(Ref::slot::<0>(&child))
            .ok_or_else(|| Error::state_error($strand, "tar entry is invalid"))?;
        let $entry = reader.borrow($strand)?;
        if $entry.generation != child.generation {
            return Err(Error::state_error($strand, "tar entry is no longer active"));
        }
    };
}

macro_rules! validate_entry_writer {
    ($this:expr, $strand:expr) => {{
        let child = $this.borrow($strand)?;
        let writer = $this
            .annex()
            .types
            .writer
            .downcast(Ref::slot::<0>(&child))
            .ok_or_else(|| Error::state_error($strand, "tar entry writer is invalid"))?;
        let parent = writer.borrow($strand)?;
        if !parent.active || parent.generation != child.generation || child.stream.is_none() {
            return Err(Error::state_error($strand, "tar entry writer is closed"));
        }
    }};
}

impl<'v> Object<'v> for TarReader {
    const NAME: &'v str = "Reader";
    const MODULE: &'v str = "tar";
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            .get("compression", |this, strand, out| {
                let compression = this.borrow(strand)?.compression;
                Output::set(strand, out, compression_sym(*this.annex(), compression));
                Ok(())
            })
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let global = *this.annex();
        let mut borrow = this.borrow_mut(strand)?;
        if borrow.closed {
            return Err(Error::state_error(strand, "tar reader is closed"));
        }
        borrow.current = None;
        borrow.generation = borrow.generation.wrapping_add(1);
        let Some(entry) = borrow.entries.next().await.transpose().into_do(strand)? else {
            return Ok(false);
        };
        let generation = borrow.generation;
        borrow.current = Some(entry);
        drop(borrow);

        global.types.entry.create_with_annex(
            strand,
            TarEntry { generation },
            global,
            Slot::reborrow(&mut out),
        );
        let entry = global.types.entry.downcast(&out).unwrap();
        let mut entry_borrow = entry.borrow_mut_unwrap();
        Output::set(strand, Mut::slot_mut::<0>(&mut entry_borrow), this);
        Ok(true)
    }
}

impl<'v> Object<'v> for TarEntry {
    const NAME: &'v str = "Entry";
    const MODULE: &'v str = "tar";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            .get("path", |this, strand, out| {
                active_entry!(this, strand, borrow);
                let bytes = borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .path_bytes()
                    .into_do(strand)?;
                let path = str::from_utf8(&bytes).into_do(strand)?;
                dolang_ext_shell::unix_path(strand, path, out)
            })
            .get("type", |this, strand, out| {
                active_entry!(this, strand, borrow);
                let ty = borrow.current.as_ref().unwrap().header().entry_type();
                Output::set(strand, out, entry_type_sym(*this.annex(), ty));
                Ok(())
            })
            .get("size", |this, strand, out| {
                active_entry!(this, strand, borrow);
                let value = borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .header()
                    .size()
                    .into_do(strand)?;
                Output::set(strand, out, value);
                Ok(())
            })
            .get("mode", |this, strand, out| {
                active_entry!(this, strand, borrow);
                let value = borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .header()
                    .mode()
                    .into_do(strand)?;
                Output::set(strand, out, value);
                Ok(())
            })
            .get("uid", |this, strand, out| {
                active_entry!(this, strand, borrow);
                let value = borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .header()
                    .uid()
                    .into_do(strand)?;
                Output::set(strand, out, value);
                Ok(())
            })
            .get("gid", |this, strand, out| {
                active_entry!(this, strand, borrow);
                let value = borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .header()
                    .gid()
                    .into_do(strand)?;
                Output::set(strand, out, value);
                Ok(())
            })
            .get("mtime", |this, strand, out| {
                active_entry!(this, strand, borrow);
                let seconds = borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .header()
                    .mtime()
                    .into_do(strand)?;
                dolang_ext_shell::datetime(
                    strand,
                    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
                    out,
                )
                .into_do(strand)
            })
            .get("user_name", |this, strand, out| {
                active_entry!(this, strand, borrow);
                match borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .username()
                    .into_do(strand)?
                {
                    Some(value) => Output::set(strand, out, value),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .get("group_name", |this, strand, out| {
                active_entry!(this, strand, borrow);
                match borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .groupname()
                    .into_do(strand)?
                {
                    Some(value) => Output::set(strand, out, value),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .get("link_name", |this, strand, out| {
                active_entry!(this, strand, borrow);
                match borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .link_name_bytes()
                    .into_do(strand)?
                {
                    Some(value) => {
                        let value = str::from_utf8(&value).into_do(strand)?;
                        dolang_ext_shell::unix_path(strand, value, out)?
                    }
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .get("device_major", |this, strand, out| {
                active_entry!(this, strand, borrow);
                match borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .header()
                    .device_major()
                    .into_do(strand)?
                {
                    Some(value) => Output::set(strand, out, value),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .get("device_minor", |this, strand, out| {
                active_entry!(this, strand, borrow);
                match borrow
                    .current
                    .as_ref()
                    .unwrap()
                    .header()
                    .device_minor()
                    .into_do(strand)?
                {
                    Some(value) => Output::set(strand, out, value),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = usize::try_from(nonnegative_u64(strand, size, "size")?)
                    .map_err(|_| Error::overflow(strand))?;
                let child = this.borrow(strand)?;
                let reader = this
                    .annex()
                    .types
                    .reader
                    .downcast(Ref::slot::<0>(&child))
                    .ok_or_else(|| Error::state_error(strand, "tar entry is invalid"))?;
                let mut borrow = reader.borrow_mut(strand)?;
                if borrow.generation != child.generation {
                    return Err(Error::state_error(strand, "tar entry is no longer active"));
                }
                let mut data = vec![0; size];
                let count = borrow
                    .current
                    .as_mut()
                    .unwrap()
                    .read(&mut data)
                    .await
                    .into_do(strand)?;
                data.truncate(count);
                Output::set(strand, out, data.as_slice());
                Ok(())
            })
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
        let child = this.borrow(strand)?;
        let reader = this
            .annex()
            .types
            .reader
            .downcast(Ref::slot::<0>(&child))
            .ok_or_else(|| Error::state_error(strand, "tar entry is invalid"))?;
        let mut borrow = reader.borrow_mut(strand)?;
        if borrow.generation != child.generation {
            return Err(Error::state_error(strand, "tar entry is no longer active"));
        }
        let mut data = vec![0; CHUNK_SIZE];
        let count = borrow
            .current
            .as_mut()
            .unwrap()
            .read(&mut data)
            .await
            .into_do(strand)?;
        if count == 0 {
            return Ok(false);
        }
        data.truncate(count);
        Output::set(strand, out, data.as_slice());
        Ok(true)
    }
}

impl TarEntryWriter {
    async fn write<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        data: &[u8],
    ) -> Result<'v, 's, ()> {
        let len = u64::try_from(data.len()).map_err(|_| Error::overflow(strand))?;
        if self.written.saturating_add(len) > self.size {
            return Err(Error::value(strand, "tar entry exceeds declared size"));
        }
        self.stream
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "tar entry writer is closed"))?
            .write_all(data)
            .await
            .into_do(strand)?;
        self.written += len;
        Ok(())
    }
}

impl<'v> Object<'v> for TarEntryWriter {
    const NAME: &'v str = "EntryWriter";
    const MODULE: &'v str = "tar";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Sink).method(
            "write",
            async move |this, strand, args, _out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let data = match data.view(strand) {
                    View::Str(value) => String::from(value).into_bytes(),
                    View::Bin(value) => value.to_vec(),
                    _ => return Err(Error::type_error(strand, "expected str or bin")),
                };
                validate_entry_writer!(this, strand);
                this.borrow_mut(strand)?.write(strand, &data).await
            },
        )
    }

    async fn output<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        validate_entry_writer!(this, strand);
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let data = match value.view(strand) {
            View::Str(value) => String::from(value).into_bytes(),
            View::Bin(value) => value.to_vec(),
            _ => return Err(Error::type_error(strand, "expected str or bin")),
        };
        validate_entry_writer!(this, strand);
        this.borrow_mut(strand)?.write(strand, &data).await
    }
}

impl<'v> Object<'v> for TarWriter {
    const NAME: &'v str = "Writer";
    const MODULE: &'v str = "tar";
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let size_sym = builder.sym("size");
        let type_sym = builder.sym("type");
        let mode_sym = builder.sym("mode");
        let uid_sym = builder.sym("uid");
        let gid_sym = builder.sym("gid");
        let mtime_sym = builder.sym("mtime");
        let user_name_sym = builder.sym("user_name");
        let group_name_sym = builder.sym("group_name");
        let link_name_sym = builder.sym("link_name");
        let device_major_sym = builder.sym("device_major");
        let device_minor_sym = builder.sym("device_minor");
        builder
            .get("compression", |this, strand, out| {
                let compression = this.borrow(strand)?.compression;
                Output::set(strand, out, compression_sym(*this.annex(), compression));
                Ok(())
            })
            .method_with_slots(
                "entry",
                async move |this, strand, args, out, [mut handle]| {
                    let global = *this.annex();
                    let (
                        [path, block],
                        [
                            size,
                            ty,
                            mode,
                            uid,
                            gid,
                            mtime,
                            user_name,
                            group_name,
                            link_name,
                            device_major,
                            device_minor,
                        ],
                    ) = unpack!(
                        strand,
                        args,
                        2,
                        0,
                        size_sym = None,
                        type_sym = None,
                        mode_sym = None,
                        uid_sym = None,
                        gid_sym = None,
                        mtime_sym = None,
                        user_name_sym = None,
                        group_name_sym = None,
                        link_name_sym = None,
                        device_major_sym = None,
                        device_minor_sym = None
                    )?;
                    let path = unix_path_string(strand, path, "path")?;
                    let size = size.ok_or_else(|| Error::value(strand, "size is required"))?;
                    let size = nonnegative_u64(strand, size, "size")?;
                    let ty = parse_entry_type(strand, global, ty)?;
                    let mode = u32::try_from(optional_u64(strand, mode, 0o644, "mode")?)
                        .map_err(|_| Error::overflow(strand))?;
                    let uid = optional_u64(strand, uid, 0, "uid")?;
                    let gid = optional_u64(strand, gid, 0, "gid")?;
                    let mtime = match mtime {
                        Some(value) => dolang_ext_shell::as_datetime(strand, &value)
                            .ok_or_else(|| Error::type_error(strand, "mtime must be a DateTime"))?
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map_err(|_| Error::value(strand, "mtime is before the Unix epoch"))?
                            .as_secs(),
                        None => 0,
                    };
                    let user_name = optional_string(strand, user_name, "user_name")?;
                    let group_name = optional_string(strand, group_name, "group_name")?;
                    let link_name = optional_unix_path_string(strand, link_name, "link_name")?;
                    let device_major = device_major
                        .map(|v| {
                            nonnegative_u64(strand, v, "device_major")
                                .and_then(|v| u32::try_from(v).map_err(|_| Error::overflow(strand)))
                        })
                        .transpose()?;
                    let device_minor = device_minor
                        .map(|v| {
                            nonnegative_u64(strand, v, "device_minor")
                                .and_then(|v| u32::try_from(v).map_err(|_| Error::overflow(strand)))
                        })
                        .transpose()?;

                    if matches!(ty, EntryType::Link | EntryType::Symlink) && link_name.is_none() {
                        return Err(Error::value(
                            strand,
                            "link_name is required for link entries",
                        ));
                    }
                    if matches!(ty, EntryType::Char | EntryType::Block)
                        && (device_major.is_none() || device_minor.is_none())
                    {
                        return Err(Error::value(
                            strand,
                            "device_major and device_minor are required for device entries",
                        ));
                    }

                    let (mut native_builder, generation) = {
                        let mut parent = this.borrow_mut(strand)?;
                        if parent.poisoned {
                            return Err(Error::state_error(strand, "tar writer is poisoned"));
                        }
                        if parent.active {
                            return Err(Error::concurrency(strand));
                        }
                        parent.active = true;
                        parent.generation = parent.generation.wrapping_add(1);
                        let generation = parent.generation;
                        let native_builder = parent
                            .builder
                            .take()
                            .ok_or_else(|| Error::state_error(strand, "tar writer is closed"))?;
                        (native_builder, generation)
                    };

                    let mut header = Header::new_gnu();
                    header.set_entry_type(ty);
                    header.set_size(size);
                    header.set_mode(mode);
                    header.set_uid(uid);
                    header.set_gid(gid);
                    header.set_mtime(mtime);
                    if let Some(value) = &user_name {
                        header.set_username(value).into_do(strand)?;
                    }
                    if let Some(value) = &group_name {
                        header.set_groupname(value).into_do(strand)?;
                    }
                    if let Some(value) = &link_name {
                        header.set_link_name(value).into_do(strand)?;
                    }
                    if let Some(value) = device_major {
                        header.set_device_major(value).into_do(strand)?;
                    }
                    if let Some(value) = device_minor {
                        header.set_device_minor(value).into_do(strand)?;
                    }

                    let (entry_stream, archive_stream) = tokio::io::duplex(CHUNK_SIZE * 2);
                    global.types.entry_writer.create_with_annex(
                        strand,
                        TarEntryWriter {
                            stream: Some(entry_stream),
                            generation,
                            written: 0,
                            size,
                        },
                        global,
                        &mut handle,
                    );
                    let handle_obj = global.types.entry_writer.downcast(&handle).unwrap();
                    let mut handle_borrow = handle_obj.borrow_mut_unwrap();
                    Output::set(strand, Mut::slot_mut::<0>(&mut handle_borrow), this);
                    drop(handle_borrow);

                    let append = async move {
                        let result = native_builder
                            .append_data(&mut header, Path::new(&path), archive_stream)
                            .await;
                        (native_builder, result)
                    };
                    let invoke = async {
                        let result = call!(strand, block, out, &handle).await;
                        let written = {
                            let mut handle_borrow = handle_obj.borrow_mut(strand)?;
                            if let Some(mut stream) = handle_borrow.stream.take() {
                                let _ = stream.shutdown().await;
                            }
                            handle_borrow.written
                        };
                        Ok((result, written))
                    };
                    let ((native_builder, append_result), invoke_result) =
                        tokio::join!(append, invoke);
                    let (block_result, written) = invoke_result?;

                    {
                        let mut parent = this.borrow_mut(strand)?;
                        parent.active = false;
                        parent.builder = Some(native_builder);
                        if append_result.is_err() || written != size {
                            parent.poisoned = true;
                        }
                    }

                    block_result?;
                    append_result.into_do(strand)?;
                    if written != size {
                        return Err(Error::value(
                            strand,
                            format!("tar entry declared {size} bytes but received {written}"),
                        ));
                    }
                    Ok(())
                },
            )
    }
}

async fn open_reader<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
) -> Result<'v, 's, (NativeEntries, Compression)> {
    let mut file = dolang_ext_shell::open(strand, path, "r")
        .await
        .into_do(strand)?;
    let mut magic = [0; ZSTD_MAGIC.len()];
    let mut magic_len = 0;
    while magic_len < magic.len() {
        let read = file.read(&mut magic[magic_len..]).await.into_do(strand)?;
        if read == 0 {
            break;
        }
        magic_len += read;
    }
    file.seek(SeekFrom::Start(0)).await.into_do(strand)?;
    let magic = &magic[..magic_len];
    let compression = if magic.starts_with(GZIP_MAGIC) {
        Compression::Gzip
    } else if magic.starts_with(ZSTD_MAGIC) {
        Compression::Zstd
    } else {
        Compression::None
    };
    let buffered = BufReader::with_capacity(CHUNK_SIZE, file);
    let reader: DynReader = match compression {
        Compression::None => Box::pin(buffered),
        Compression::Gzip => Box::pin(GzipDecoder::new(buffered)),
        Compression::Zstd => Box::pin(ZstdDecoder::new(buffered)),
    };
    let mut archive = Archive::new(reader);
    let entries = archive.entries().into_do(strand)?;
    Ok((entries, compression))
}

fn parse_compression<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Option<Slot<'v, '_>>,
    path: &Path,
) -> Result<'v, 's, Compression> {
    let Some(value) = value else {
        let extension = path.extension().and_then(|extension| extension.to_str());
        return Ok(match extension {
            Some(extension)
                if extension.eq_ignore_ascii_case("gz")
                    || extension.eq_ignore_ascii_case("tgz") =>
            {
                Compression::Gzip
            }
            Some(extension)
                if extension.eq_ignore_ascii_case("zst")
                    || extension.eq_ignore_ascii_case("tzst") =>
            {
                Compression::Zstd
            }
            _ => Compression::None,
        });
    };
    match value.as_sym(strand) {
        Some(sym) if sym == global.syms.none => Ok(Compression::None),
        Some(sym) if sym == global.syms.gzip => Ok(Compression::Gzip),
        Some(sym) if sym == global.syms.zstd => Ok(Compression::Zstd),
        _ => Err(Error::value(
            strand,
            "compression must be :NONE:, :GZIP:, or :ZSTD:",
        )),
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let compression_sym = builder.sym("compression");
    builder
        .module("tar")
        .value("Reader", global.types.reader)
        .value("Entry", global.types.entry)
        .value("Writer", global.types.writer)
        .value("EntryWriter", global.types.entry_writer)
        .function("read", async move |strand, args, out| {
            let ([path, block], []) = unpack!(strand, args, 2, 0)?;
            let path = dolang_ext_shell::as_path(strand, &path)
                .ok_or_else(|| Error::type_error(strand, "path must be a str or Path"))?;
            let (entries, compression) = open_reader(strand, path.as_ref()).await?;
            strand
                .with_slots(async move |strand, [mut reader]| {
                    global.types.reader.create_with_annex(
                        strand,
                        TarReader {
                            entries,
                            current: None,
                            generation: 0,
                            compression,
                            closed: false,
                        },
                        global,
                        &mut reader,
                    );
                    let result = call!(strand, block, out, &reader).await;
                    let reader_obj = global.types.reader.downcast(&reader).unwrap();
                    let mut borrow = reader_obj.borrow_mut(strand)?;
                    borrow.current = None;
                    borrow.closed = true;
                    borrow.generation = borrow.generation.wrapping_add(1);
                    result
                })
                .await
        })
        .function("write", async move |strand, args, out| {
            let ([path, block], [compression]) =
                unpack!(strand, args, 2, 0, compression_sym = None)?;
            let path = dolang_ext_shell::as_path(strand, &path)
                .ok_or_else(|| Error::type_error(strand, "path must be a str or Path"))?;
            let compression = parse_compression(strand, global, compression, path.as_ref())?;
            let file = dolang_ext_shell::open(strand, path.as_ref(), "w")
                .await
                .into_do(strand)?;
            let writer: DynWriter = match compression {
                Compression::None => Box::pin(file),
                Compression::Gzip => Box::pin(GzipEncoder::new(file)),
                Compression::Zstd => Box::pin(ZstdEncoder::new(file)),
            };
            let native_builder = TarBuilder::new(writer);
            strand
                .with_slots(async move |strand, [mut writer]| {
                    global.types.writer.create_with_annex(
                        strand,
                        TarWriter {
                            builder: Some(native_builder),
                            generation: 0,
                            active: false,
                            poisoned: false,
                            compression,
                        },
                        global,
                        &mut writer,
                    );
                    let result = call!(strand, block, out, &writer).await;
                    let writer_obj = global.types.writer.downcast(&writer).unwrap();
                    let (builder, poisoned, active) = {
                        let mut borrow = writer_obj.borrow_mut(strand)?;
                        (borrow.builder.take(), borrow.poisoned, borrow.active)
                    };
                    let cleanup = if active || poisoned {
                        Err(Error::state_error(
                            strand,
                            "tar writer did not finish cleanly",
                        ))
                    } else {
                        async {
                            let mut inner = builder
                                .ok_or_else(|| Error::state_error(strand, "tar writer is closed"))?
                                .into_inner()
                                .await
                                .into_do(strand)?;
                            inner.shutdown().await.into_do(strand)?;
                            Ok(())
                        }
                        .await
                    };
                    result.and(cleanup)
                })
                .await
        })
        .commit();
}
