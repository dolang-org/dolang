use std::{
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::protocol::members,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{BinEmbryo, Output, Slot, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod, index, iter,
    protocol::{
        GcObj, Inspect, Protocol, Recv, instance_mcall_fallback, is_special_mcall,
        type_mcall_fallback,
    },
    range,
};

/// Mutable byte buffer. The mutable counterpart to `Bin`, backed by a
/// [`BinEmbryo`] so incremental growth avoids realloc/copy churn.
pub(crate) struct BinBuf<'v> {
    embryo: BinEmbryo<'v>,
    /// Capacity hint carried across `freeze()`, since the embryo itself resets to zero
    /// capacity when swapped out. Consulted lazily on the next reserve, never acted on
    /// eagerly right after a freeze.
    grow_hint: usize,
    /// Logical start of live data within `embryo`. `drain()` advances this directly
    /// instead of memmoving the tail down on every chunk, so repeated draining stays O(1)
    /// per chunk rather than O(remaining length). Every other mutation settles first (see
    /// [`Self::settle`]) so its own logic can keep assuming offset zero.
    offset: usize,
}

unsafe impl<'v> Collect for BinBuf<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Default for BinBuf<'v> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'v> BinBuf<'v> {
    pub(crate) fn new() -> Self {
        Self {
            embryo: BinEmbryo::new(),
            grow_hint: 0,
            offset: 0,
        }
    }

    fn len(&self) -> usize {
        self.embryo.len() - self.offset
    }

    fn as_slice(&self) -> &[u8] {
        &self.embryo.as_slice()[self.offset..]
    }

    /// Discards the already-drained prefix (if any) by memmoving the live tail down to
    /// offset zero, so callers that splice/truncate at raw indices can keep treating
    /// `embryo` indices as logical indices afterward.
    fn settle<'s>(&mut self, strand: &mut Strand<'v, 's>) {
        if self.offset != 0 {
            self.embryo.splice(strand, 0, self.offset, &[]);
            self.offset = 0;
        }
    }

    fn reserve<'s>(&mut self, strand: &mut Strand<'v, 's>, additional: usize) {
        // Settle unconditionally: a drained prefix is dead weight that only gets bigger
        // (and thus more expensive to move) the longer it's left in place, so there's no
        // benefit to deferring this until growth is actually forced.
        self.settle(strand);
        if self.embryo.capacity() == 0 && self.grow_hint > additional {
            self.embryo.reserve(strand, self.grow_hint);
        } else {
            self.embryo.reserve(strand, additional);
        }
    }

    fn extend_raw<'s>(&mut self, strand: &mut Strand<'v, 's>, bytes: &[u8]) {
        self.reserve(strand, bytes.len());
        self.embryo.extend(strand, bytes);
    }

    /// Copies the raw bytes of `value` (a `Str` or `Bin`) directly into the buffer using a
    /// GC-algorithm-agnostic access path (no assumption that the source won't move), rather
    /// than `Value::as_u8_slice_raw`. Returns `false` if `value` is neither.
    fn copy_from_value<'s>(&mut self, strand: &mut Strand<'v, 's>, value: &Value<'v>) -> bool {
        if let Some(bin) = value.as_bin(strand.vm()) {
            let len = bin.len();
            self.reserve(strand, len);
            strand.access(|access| unsafe {
                let bytes = bin.as_slice(access);
                let spare = &mut self.embryo.spare_capacity_mut()[..len];
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), spare.as_mut_ptr().cast::<u8>(), len);
            });
            unsafe { self.embryo.advance(len) };
            true
        } else if let Some(s) = value.as_str(strand.vm()) {
            let len = s.len();
            self.reserve(strand, len);
            strand.access(|access| unsafe {
                let bytes = s.as_str(access).as_bytes();
                let spare = &mut self.embryo.spare_capacity_mut()[..len];
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), spare.as_mut_ptr().cast::<u8>(), len);
            });
            unsafe { self.embryo.advance(len) };
            true
        } else {
            false
        }
    }

    /// Appends `value`: `Str`/`Bin` input is copied as raw bytes; anything else is
    /// stringified via `op_display` written straight into the buffer, with no
    /// intermediate `Str`/`Vec<u8>` allocation.
    fn append<'s>(&mut self, strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, ()> {
        if self.copy_from_value(strand, value) {
            Ok(())
        } else {
            value.op_display(strand, &mut self.embryo)
        }
    }

    /// Appends raw bytes from `value`, which must be `Str` or `Bin`.
    fn extend_method<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, ()> {
        if self.copy_from_value(strand, value) {
            Ok(())
        } else {
            Err(Error::type_error(strand, "not binary data: unknown"))
        }
    }

    /// Splices at logical indices. Settles first so `start`/`end` (already computed against
    /// `len()`) line up with `embryo`'s own indices.
    fn splice<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        start: usize,
        end: usize,
        replacement: &[u8],
    ) {
        self.settle(strand);
        self.embryo.splice(strand, start, end, replacement);
    }

    fn clear(&mut self) {
        self.embryo.truncate(0);
        self.offset = 0;
    }

    fn freeze<'s>(&mut self, strand: &mut Strand<'v, 's>, out: impl Output<'v>) {
        self.settle(strand);
        self.grow_hint = self.grow_hint.max(self.embryo.capacity());
        self.embryo.freeze_reset(strand, out);
    }
}

impl<'v> Protocol<'v> for BinBuf<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().binbuf)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        crate::fmt!(
            strand,
            w,
            "BinBuf({:?})",
            bstr::BStr::new(borrow.as_slice())
        )
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let borrow = this.borrow(strand).expect("conflicting borrow");
        !borrow.as_slice().is_empty()
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_native(strand, strand.builtin_types().binbuf) else {
            return Ok(Value::FALSE);
        };
        let this_borrow = this.borrow(strand)?;
        let other_borrow = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        Ok(Value::from_bool(
            this_borrow.as_slice() == other_borrow.as_slice(),
        ))
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_native(strand, strand.builtin_types().binbuf) else {
            return Err(Error::not_supported(strand));
        };
        let this_borrow = this.borrow(strand)?;
        let other_borrow = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        Ok(Value::from_bool(
            this_borrow.as_slice() < other_borrow.as_slice(),
        ))
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        this.borrow(strand)?.as_slice().hash(hasher);
        Ok(())
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let me = borrow.as_slice();
        if let Some(slice) = range::slice(index, strand, me.len())? {
            match slice {
                range::Slice::Contiguous { start, end } => {
                    let slice = me.get(start..end).ok_or_else(|| Error::index(strand))?;
                    Output::set(strand, out, slice);
                }
                range::Slice::Stepped(indices) => {
                    let bytes: Vec<u8> = indices.into_iter().map(|i| me[i]).collect();
                    Output::set(strand, out, bytes.as_slice());
                }
            }
            return Ok(());
        }
        let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
        let index = index::element(me.len(), index).ok_or_else(|| Error::index(strand))?;
        Output::set(strand, out, me[index]);
        Ok(())
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let len = this.borrow(strand)?.len();
        if let Some(slice) = range::slice(&index, strand, len)? {
            let range::Slice::Contiguous { start, end } = slice else {
                return Err(Error::index(strand));
            };
            if let Some(bin) = value.as_bin(strand.vm()) {
                let growth = bin.len().saturating_sub(end - start);
                let mut buf = this.borrow_mut(strand)?;
                buf.settle(strand);
                buf.reserve(strand, growth);
                strand.access(|access| unsafe {
                    let bytes = bin.as_slice(access);
                    buf.embryo.splice_unchecked(start, end, bytes);
                });
                return Ok(());
            }
            if let Some(s) = value.as_str(strand.vm()) {
                let growth = s.len().saturating_sub(end - start);
                let mut buf = this.borrow_mut(strand)?;
                buf.settle(strand);
                buf.reserve(strand, growth);
                strand.access(|access| unsafe {
                    let bytes = s.as_str(access).as_bytes();
                    buf.embryo.splice_unchecked(start, end, bytes);
                });
                return Ok(());
            }
            return Err(Error::type_error(strand, "not binary data: unknown"));
        }
        let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
        let mut borrow = this.borrow_mut(strand)?;
        let index = index::element(borrow.len(), index).ok_or_else(|| Error::index(strand))?;
        let byte = value
            .to_i64(strand)
            .ok()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| Error::type_error(strand, "expected byte value 0..=255"))?;
        borrow.splice(strand, index, index + 1, &[byte]);
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::APPEND => {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.append(strand, &value)
            }
            sym::PUSH => {
                let ([], [], values) = unpack!(strand, args, 0, 0, *)?;
                let mut borrow = this.borrow_mut(strand)?;
                for value in values {
                    let byte = value
                        .to_i64(strand)
                        .ok()
                        .and_then(|v| u8::try_from(v).ok())
                        .ok_or_else(|| Error::type_error(strand, "expected byte value 0..=255"))?;
                    borrow.extend_raw(strand, &[byte]);
                }
                Ok(())
            }
            sym::EXTEND => {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.extend_method(strand, &value)
            }
            sym::INSERT => {
                let ([index, value], []) = unpack!(strand, args, 2, 0)?;
                let index_i64 = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                let len = this.borrow(strand)?.len();
                let index = index::position(len, index_i64).ok_or_else(|| Error::index(strand))?;
                if let Some(bin) = value.as_bin(strand.vm()) {
                    let mut borrow = this.borrow_mut(strand)?;
                    borrow.settle(strand);
                    borrow.reserve(strand, bin.len());
                    strand.access(|access| unsafe {
                        let bytes = bin.as_slice(access);
                        borrow.embryo.splice_unchecked(index, index, bytes);
                    });
                    Ok(())
                } else if let Some(s) = value.as_str(strand.vm()) {
                    let mut borrow = this.borrow_mut(strand)?;
                    borrow.settle(strand);
                    borrow.reserve(strand, s.len());
                    strand.access(|access| unsafe {
                        let bytes = s.as_str(access).as_bytes();
                        borrow.embryo.splice_unchecked(index, index, bytes);
                    });
                    Ok(())
                } else if let Some(byte) =
                    value.to_i64(strand).ok().and_then(|v| u8::try_from(v).ok())
                {
                    let mut borrow = this.borrow_mut(strand)?;
                    borrow.splice(strand, index, index, &[byte]);
                    Ok(())
                } else {
                    Err(Error::type_error(
                        strand,
                        "expected byte value 0..=255, Str, or Bin",
                    ))
                }
            }
            sym::REMOVE => {
                let ([index_or_range], []) = unpack!(strand, args, 1, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                let len = borrow.len();
                if let Some(slice) = range::slice(&index_or_range, strand, len)? {
                    let range::Slice::Contiguous { start, end } = slice else {
                        return Err(Error::index(strand));
                    };
                    let removed = borrow.as_slice()[start..end].to_vec();
                    borrow.splice(strand, start, end, &[]);
                    Output::set(strand, out, removed.as_slice());
                    return Ok(());
                }
                let idx = index_or_range
                    .to_i64(strand)
                    .map_err(|_| Error::index(strand))?;
                let idx = index::element(len, idx).ok_or_else(|| Error::index(strand))?;
                let byte = borrow.as_slice()[idx];
                borrow.splice(strand, idx, idx + 1, &[]);
                Output::set(strand, out, byte);
                Ok(())
            }
            sym::TRUNCATE => {
                let ([len], []) = unpack!(strand, args, 1, 0)?;
                let len = len.to_index(strand)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.settle(strand);
                borrow.embryo.truncate(len);
                Ok(())
            }
            sym::CLEAR => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                this.borrow_mut(strand)?.clear();
                Ok(())
            }
            sym::FREEZE => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.freeze(strand, out);
                Ok(())
            }
            sym::STARTS_WITH => {
                let ([prefix], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                let input = borrow.as_slice().starts_with(
                    prefix
                        .as_u8_slice_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "not binary data: unknown"))?,
                );
                Output::set(strand, out, input);
                Ok(())
            }
            sym::ENDS_WITH => {
                let ([suffix], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                let input = borrow.as_slice().ends_with(
                    suffix
                        .as_u8_slice_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "not binary data: unknown"))?,
                );
                Output::set(strand, out, input);
                Ok(())
            }
            sym::CONTAINS => {
                use bstr::ByteSlice;
                let ([needle], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                let input = borrow.as_slice().contains_str(
                    needle
                        .as_u8_slice_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "not binary data: unknown"))?,
                );
                Output::set(strand, out, input);
                Ok(())
            }
            sym::DRAIN => {
                let ([], [size]) = unpack!(strand, args, 0, 1)?;
                let chunk_size = match size {
                    Some(size) => size.to_index(strand)?,
                    None => crate::BYTE_STREAM_CHUNK_SIZE,
                };
                if chunk_size == 0 {
                    return Err(Error::value(strand, "chunk size must be positive"));
                }
                let chunks = Chunks {
                    buf: this.to_strong(),
                    chunk_size,
                };
                strand
                    .builtin_types()
                    .binbuf_chunks
                    .create(strand, chunks, out);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(strand, "len is a field, not a method")),
            _ if is_special_mcall(method.tag()) => {
                instance_mcall_fallback(strand, &this, method, args, out)
                    .await
                    .expect("supported special method")
            }
            _ => iter::sink_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let len = this.borrow(strand)?.len();
                Output::set(strand, out, len);
                Ok(())
            }
            sym::APPEND
            | sym::PUSH
            | sym::EXTEND
            | sym::INSERT
            | sym::REMOVE
            | sym::TRUNCATE
            | sym::CLEAR
            | sym::FREEZE
            | sym::STARTS_WITH
            | sym::ENDS_WITH
            | sym::CONTAINS
            | sym::DRAIN => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => iter::sink_get(strand, &this, field, out),
        }
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    /// Appends the value's bytes verbatim.
    ///
    /// No line terminator is added: framing is the caller's decision, expressed
    /// with `crimp`/`precrimp` when wanted. That is what lets a `BinBuf` stand in
    /// for any other sink without changing what gets written.
    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        borrow.append(strand, &value)
    }
}

pub(crate) struct Class;

unsafe impl Collect for Class {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Class {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.BinBuf>")
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().sinkable)
            || supertype.eq(strand, TypeObject::Value)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], [initial]) = unpack!(strand, args, 0, 1)?;
        let mut buf = BinBuf::new();
        if let Some(initial) = &initial {
            buf.copy_from_value(strand, initial);
        }
        strand.builtin_types().binbuf.create(strand, buf, out);
        Ok(())
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
            members: members![
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::LT_METHOD),
                Method(sym::BOOL_METHOD),
                Method(sym::HASH_METHOD),
                Getter(sym::LEN),
                Method(sym::APPEND),
                Method(sym::PUSH),
                Method(sym::EXTEND),
                Method(sym::INSERT),
                Method(sym::REMOVE),
                Method(sym::TRUNCATE),
                Method(sym::CLEAR),
                Method(sym::FREEZE),
                Method(sym::STARTS_WITH),
                Method(sym::ENDS_WITH),
                Method(sym::CONTAINS),
                Method(sym::DRAIN),
            ],
        })
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([self_val], [initial]) = unpack!(strand, args, 1, 1)?;
                let mut buf = BinBuf::new();
                if let Some(initial) = &initial {
                    buf.copy_from_value(strand, initial);
                }
                strand
                    .builtin_types()
                    .binbuf
                    .create(strand, buf, Slot::reborrow(&mut out));
                let native = out.take();
                self_val.op_fill(strand, &strand.singletons().binbuf, native)?;
                Ok(())
            }
            _ => type_mcall_fallback(strand, &strand.singletons().binbuf, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::INIT_METHOD
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::BOOL_METHOD
            | sym::HASH_METHOD
            | sym::LEN
            | sym::APPEND
            | sym::PUSH
            | sym::EXTEND
            | sym::INSERT
            | sym::REMOVE
            | sym::TRUNCATE
            | sym::CLEAR
            | sym::FREEZE
            | sym::STARTS_WITH
            | sym::ENDS_WITH
            | sym::CONTAINS
            | sym::DRAIN => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }
}

/// Iterator produced by `BinBuf.drain()`. Each step drains up to `chunk_size` bytes off
/// the *front* of the shared source buffer by advancing its `offset` field, never
/// memmoving the remaining tail — draining stays O(1) per chunk rather than O(remaining
/// length). Because the drain cursor (`offset`) lives on the buffer itself rather than on
/// this iterator, several `Chunks` iterators over the same `BinBuf` interleave draining the
/// same stream rather than needing to invalidate each other.
pub(crate) struct Chunks<'v> {
    buf: GcObj<'v, BinBuf<'v>>,
    chunk_size: usize,
}

unsafe impl<'v> Collect for Chunks<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.buf.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Chunks<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<BinBuf drain iterator>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand)?;
        let mut buf = borrow
            .buf
            .borrow_mut()
            .ok_or_else(|| Error::concurrency(strand))?;
        let len = buf.len();
        if len == 0 {
            return Ok(false);
        }
        let want = borrow.chunk_size.min(len);
        let chunk = buf.as_slice()[..want].to_vec();
        buf.offset += want;
        Output::set(strand, out, chunk.as_slice());
        Ok(true)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
    }
}
