use std::{
    future::{self, Future},
    mem,
};

use dolang::runtime::{
    Args, BYTE_STREAM_CHUNK_SIZE, Error, Instance, Object, Output, Result, Slot, Strand, Type,
    error::ErrorKind,
    io::{self, IoCaps, IoResultExt as _},
    object::TypeBuilder,
    strand::InterruptMask,
    unpack,
    value::{TypeObject, View},
    vm::Register,
};
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
use futures::future::Either;
use futures::io::AsyncReadExt as _;

pub(crate) struct DigestState;
pub(crate) struct Blake3State(blake3::Hasher);

/// Hash state that can absorb bytes off the VM thread.
trait Absorb: Default + Send + 'static {
    fn absorb(&mut self, bytes: &[u8]);
}

trait Algorithm: digest::Digest + Default + Clone + Send + 'static {
    const TYPE_NAME: &'static str;
}

pub(crate) struct Digestible<T>(T);

impl Algorithm for md5::Md5 {
    const TYPE_NAME: &'static str = "Md5";
}

impl Algorithm for sha1::Sha1 {
    const TYPE_NAME: &'static str = "Sha1";
}

impl Algorithm for sha2::Sha256 {
    const TYPE_NAME: &'static str = "Sha256";
}

impl Algorithm for sha2::Sha512 {
    const TYPE_NAME: &'static str = "Sha512";
}

impl<T: Algorithm> Absorb for T {
    fn absorb(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

impl Absorb for blake3::Hasher {
    fn absorb(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

/// A buffer of up to [`BYTE_STREAM_CHUNK_SIZE`] bytes, zeroed once and reused.
struct Chunk {
    buf: Vec<u8>,
    len: usize,
}

impl Chunk {
    fn new() -> Self {
        Self {
            buf: vec![0; BYTE_STREAM_CHUNK_SIZE],
            len: 0,
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// The bytes to hash: a `Str` or `Bin`, or a byte I/O provider's read handle.
enum Source<'v, 'a> {
    Mem { value: Slot<'v, 'a>, offset: usize },
    Io(Box<dyn io::Read>),
}

/// Calls `f` with the bytes of `value`, which must be a `Str` or `Bin`.
fn with_bytes<'v, R>(
    strand: &mut Strand<'v, '_>,
    value: &Slot<'v, '_>,
    f: impl FnOnce(&[u8]) -> R,
) -> R {
    match value.view(strand.vm()) {
        View::Str(str) => strand.access(|access| f(str.as_str(access).as_bytes())),
        View::Bin(bin) => strand.access(|access| f(bin.as_slice(access))),
        _ => unreachable!("memory source is a Str or Bin"),
    }
}

impl<'v, 'a> Source<'v, 'a> {
    fn new<'s>(strand: &mut Strand<'v, 's>, value: Slot<'v, 'a>) -> Result<'v, 's, Self> {
        if matches!(value.view(strand.vm()), View::Str(_) | View::Bin(_)) {
            return Ok(Self::Mem { value, offset: 0 });
        }
        let handle = io::get(strand, &value, |strand, caps| {
            if !caps.contains(IoCaps::READ) {
                return Err(Error::value(strand, "data must be readable"));
            }
            Ok(())
        })
        .map_err(|err| {
            if err.kind() == ErrorKind::Unsupported {
                Error::type_error(strand, "expected `Str`, `Bin`, or a byte I/O provider")
            } else {
                err
            }
        })?;
        Ok(Self::Io(handle.into_read().expect("data is readable")))
    }

    /// Fills `chunk` until it is full or the input ends, and returns whether it ended.
    async fn fill<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        chunk: &mut Chunk,
    ) -> Result<'v, 's, bool> {
        chunk.len = 0;
        match self {
            Self::Mem { value, offset } => {
                let start = *offset;
                let (copied, rest) = with_bytes(strand, value, |bytes| {
                    let end = bytes.len().min(start + chunk.buf.len());
                    chunk.buf[..end - start].copy_from_slice(&bytes[start..end]);
                    (end - start, bytes.len() - end)
                });
                chunk.len = copied;
                *offset += copied;
                Ok(rest == 0)
            }
            Self::Io(reader) => {
                while chunk.len < chunk.buf.len() {
                    let read = reader
                        .read(&mut chunk.buf[chunk.len..])
                        .await
                        .into_do_io(strand)?;
                    if read == 0 {
                        return Ok(true);
                    }
                    chunk.len += read;
                }
                Ok(false)
            }
        }
    }
}

/// Absorbs `chunk` into `state`, on the blocking pool when there is a tokio runtime.
///
/// The work starts before the returned future is first polled.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn offload<H: Absorb>(mut state: H, chunk: Chunk) -> impl Future<Output = (H, Chunk)> {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let task = runtime.spawn_blocking(move || {
            state.absorb(chunk.bytes());
            (state, chunk)
        });
        return Either::Left(async move {
            task.await
                .unwrap_or_else(|err| std::panic::resume_unwind(err.into_panic()))
        });
    }
    state.absorb(chunk.bytes());
    Either::Right(future::ready((state, chunk)))
}

/// Absorbs `chunk` into `state` inline, as there is no blocking pool.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn offload<H: Absorb>(mut state: H, chunk: Chunk) -> impl Future<Output = (H, Chunk)> {
    state.absorb(chunk.bytes());
    future::ready((state, chunk))
}

/// Absorbs all of `source` into `state`.
///
/// An input that fits in one chunk is hashed inline. A longer one is hashed a chunk at a
/// time by [`offload`] while the next chunk is read.
async fn absorb<'v, 's, H: Absorb>(
    strand: &mut Strand<'v, 's>,
    state: &mut H,
    mut source: Source<'v, '_>,
) -> Result<'v, 's, ()> {
    if let Source::Mem { value, .. } = &source
        && with_bytes(strand, value, |bytes| bytes.len()) <= BYTE_STREAM_CHUNK_SIZE
    {
        with_bytes(strand, value, |bytes| state.absorb(bytes));
        return Ok(());
    }

    let mut chunk = Chunk::new();
    let mut end = source.fill(strand, &mut chunk).await?;
    if end {
        state.absorb(chunk.bytes());
        return Ok(());
    }
    let mut next = Chunk::new();
    loop {
        let task = offload(mem::take(state), chunk);
        // Read the next chunk meanwhile. An interrupt or error must not drop `task`, which
        // holds the state, so both wait for it below before they propagate.
        let read = if end {
            None
        } else {
            Some(
                strand
                    .interrupt_guard(async |strand| source.fill(strand, &mut next).await)
                    .await,
            )
        };
        // The wait is bounded by one chunk's hash.
        (*state, chunk) = strand
            .with_interrupt_mask(InterruptMask::all(), async move |_| task.await)
            .await;
        let Some(read) = read else {
            return Ok(());
        };
        end = read?;
        mem::swap(&mut chunk, &mut next);
        if chunk.len == 0 {
            return Ok(());
        }
    }
}

async fn digest_single<'v, 's, T: Algorithm>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let source = Source::new(strand, value)?;
    let mut state = T::default();
    absorb(strand, &mut state, source).await?;
    Output::set(strand, out, state.finalize().as_slice());
    Ok(())
}

async fn digest_update<'v, 's, T: Algorithm>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, Digestible<T>>,
    value: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let source = Source::new(strand, value)?;
    let mut state = this.borrow_mut(strand)?;
    absorb(strand, &mut state.0, source).await
}

async fn blake3_single<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let source = Source::new(strand, value)?;
    let mut state = blake3::Hasher::new();
    absorb(strand, &mut state, source).await?;
    Output::set(strand, out, state.finalize().as_slice());
    Ok(())
}

async fn blake3_update<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, Blake3State>,
    value: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let source = Source::new(strand, value)?;
    let mut state = this.borrow_mut(strand)?;
    absorb(strand, &mut state.0, source).await
}

impl<'v> Object<'v> for DigestState {
    const NAME: &'v str = "State";
    const MODULE: &'v str = "digest";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
}

impl<'v> Object<'v> for Blake3State {
    const NAME: &'v str = "Blake3";
    const MODULE: &'v str = "digest";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], []) = unpack!(strand, args, 0, 0)?;
        this.create(strand, Self(blake3::Hasher::new()), out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("update", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                blake3_update(strand, this, value).await?;
                Output::set(strand, out, this);
                Ok(())
            })
            .method("digest", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let digest = this.borrow(strand)?.0.clone().finalize();
                Output::set(strand, out, digest.as_slice());
                Ok(())
            })
    }

    async fn sink<'a, 's>(
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
        blake3_update(strand, this, value).await
    }
}

impl<'v, T: Algorithm> Object<'v> for Digestible<T> {
    const NAME: &'v str = T::TYPE_NAME;
    const MODULE: &'v str = "digest";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], []) = unpack!(strand, args, 0, 0)?;
        this.create(strand, Self(T::default()), out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("update", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                digest_update(strand, this, value).await?;
                Output::set(strand, out, this);
                Ok(())
            })
            .method("digest", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let digest = this.borrow(strand)?.0.clone().finalize();
                Output::set(strand, out, digest.as_slice());
                Ok(())
            })
    }

    async fn sink<'a, 's>(
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
        digest_update(strand, this, value).await
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>) {
    let state = builder.register_type::<DigestState>();
    let blake3 = builder
        .build_type::<Blake3State>((), ())
        .nominal_supertype(state)
        .build();
    let md5 = builder
        .build_type::<Digestible<md5::Md5>>((), ())
        .nominal_supertype(state)
        .build();
    let sha1 = builder
        .build_type::<Digestible<sha1::Sha1>>((), ())
        .nominal_supertype(state)
        .build();
    let sha256 = builder
        .build_type::<Digestible<sha2::Sha256>>((), ())
        .nominal_supertype(state)
        .build();
    let sha512 = builder
        .build_type::<Digestible<sha2::Sha512>>((), ())
        .nominal_supertype(state)
        .build();

    builder
        .module("digest")
        .value("State", state)
        .value("Blake3", blake3)
        .value("Md5", md5)
        .value("Sha1", sha1)
        .value("Sha256", sha256)
        .value("Sha512", sha512)
        .function("blake3", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            blake3_single(strand, value, out).await
        })
        .function("md5", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            digest_single::<md5::Md5>(strand, value, out).await
        })
        .function("sha1", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            digest_single::<sha1::Sha1>(strand, value, out).await
        })
        .function("sha256", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            digest_single::<sha2::Sha256>(strand, value, out).await
        })
        .function("sha512", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            digest_single::<sha2::Sha512>(strand, value, out).await
        })
        .commit();
}
