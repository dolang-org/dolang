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
    value::{Output, Slot, StrEmbryo, TypeObject, Value},
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

const BOUNDARY_ERROR: &str = "invalid UTF-8 substring boundaries";

fn check_boundary<'v, 's>(s: &str, idx: usize, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
    if s.is_char_boundary(idx) {
        Ok(())
    } else {
        Err(Error::runtime(strand, BOUNDARY_ERROR))
    }
}

/// Mutable UTF-8 string buffer. The mutable counterpart to `Str`, backed by a
/// [`StrEmbryo`] so incremental growth avoids realloc/copy churn.
///
/// Unlike `BinBuf`, `StrBuf` supports only *range* indexing/assignment/removal, never a
/// scalar index — a single UTF-8 code unit is rarely a useful result, matching `Str` itself.
pub(crate) struct StrBuf<'v> {
    embryo: StrEmbryo<'v>,
    /// Capacity hint carried across `freeze()`, since the embryo itself resets to zero
    /// capacity when swapped out. Consulted lazily on the next reserve, never acted on
    /// eagerly right after a freeze.
    grow_hint: usize,
    /// Logical start of live data within `embryo`, always a UTF-8 char boundary.
    /// `drain()` advances this directly instead of memmoving the tail down on every
    /// chunk, so repeated draining stays O(1) per chunk rather than O(remaining length).
    /// Every other mutation settles first (see [`Self::settle`]) so its own logic can
    /// keep assuming offset zero.
    offset: usize,
}

unsafe impl<'v> Collect for StrBuf<'v> {
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

impl<'v> Default for StrBuf<'v> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'v> StrBuf<'v> {
    pub(crate) fn new() -> Self {
        Self {
            embryo: StrEmbryo::new(),
            grow_hint: 0,
            offset: 0,
        }
    }

    fn len(&self) -> usize {
        self.embryo.len() - self.offset
    }

    fn as_str(&self) -> &str {
        &self.embryo.as_str()[self.offset..]
    }

    /// Discards the already-drained prefix (if any) by memmoving the live tail down to
    /// offset zero, so callers that splice/truncate at raw indices can keep treating
    /// `embryo` indices as logical indices afterward. `offset` is always a char boundary,
    /// so the splice below is always safe to perform on an empty (thus trivially valid
    /// UTF-8) replacement.
    fn settle<'s>(&mut self, strand: &mut Strand<'v, 's>) {
        if self.offset != 0 {
            unsafe { self.embryo.splice(strand, 0, self.offset, &[]) };
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

    /// Copies the raw bytes of `value` (which must be a `Str`) directly into the buffer
    /// using a GC-algorithm-agnostic access path, rather than `Value::as_str_raw`. Returns
    /// `false` if `value` is not a `Str` (a `Bin` value is deliberately not raw-copied here
    /// either — it must go through `op_display`/UTF-8 validation like anything else, since
    /// `StrBuf` must maintain the UTF-8 invariant).
    fn copy_from_value<'s>(&mut self, strand: &mut Strand<'v, 's>, value: &Value<'v>) -> bool {
        if let Some(s) = value.as_str(strand.vm()) {
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

    /// Appends `value`: `Str` input is copied as raw bytes; anything else is stringified
    /// via `op_display` written straight into the buffer, with no intermediate `Str`
    /// allocation.
    fn append<'s>(&mut self, strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, ()> {
        if self.copy_from_value(strand, value) {
            Ok(())
        } else {
            value.op_display(strand, &mut self.embryo)
        }
    }

    /// Appends raw bytes from `value`, which must be `Str`.
    fn extend_method<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, ()> {
        if self.copy_from_value(strand, value) {
            Ok(())
        } else {
            Err(Error::type_error(strand, "StrBuf.extend: expected Str"))
        }
    }

    fn clear(&mut self) {
        unsafe { self.embryo.truncate(0) }
        self.offset = 0;
    }

    fn freeze<'s>(&mut self, strand: &mut Strand<'v, 's>, out: impl Output<'v>) {
        self.settle(strand);
        self.grow_hint = self.grow_hint.max(self.embryo.capacity());
        self.embryo.freeze_reset(strand, out);
    }
}

impl<'v> Protocol<'v> for StrBuf<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().strbuf)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        crate::fmt!(strand, w, "StrBuf({:?})", borrow.as_str())
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let borrow = this.borrow(strand).expect("conflicting borrow");
        !borrow.as_str().is_empty()
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_native(strand, strand.builtin_types().strbuf) else {
            return Ok(Value::FALSE);
        };
        let this_borrow = this.borrow(strand)?;
        let other_borrow = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        Ok(Value::from_bool(
            this_borrow.as_str() == other_borrow.as_str(),
        ))
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_native(strand, strand.builtin_types().strbuf) else {
            return Err(Error::not_supported(strand));
        };
        let this_borrow = this.borrow(strand)?;
        let other_borrow = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        Ok(Value::from_bool(
            this_borrow.as_str() < other_borrow.as_str(),
        ))
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        this.borrow(strand)?.as_str().hash(hasher);
        Ok(())
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let me = borrow.as_str();
        let Some((start, end)) = range::slice_bounds(index, strand, me.len())? else {
            return Err(Error::index(strand));
        };
        let slice = me
            .get(start..end)
            .ok_or_else(|| Error::runtime(strand, BOUNDARY_ERROR))?;
        Output::set(strand, out, slice);
        Ok(())
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let len = this.borrow(strand)?.len();
        let Some((start, end)) = range::slice_bounds(&index, strand, len)? else {
            return Err(Error::index(strand));
        };
        let Some(s) = value.as_str(strand.vm()) else {
            return Err(Error::type_error(strand, "StrBuf assignment: expected Str"));
        };
        let mut buf = this.borrow_mut(strand)?;
        buf.settle(strand);
        {
            let cur = buf.as_str();
            check_boundary(cur, start, strand)?;
            check_boundary(cur, end, strand)?;
        }
        let growth = s.len().saturating_sub(end - start);
        buf.reserve(strand, growth);
        strand.access(|access| unsafe {
            let bytes = s.as_str(access).as_bytes();
            buf.embryo.splice_unchecked(start, end, bytes);
        });
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
            sym::EXTEND => {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.extend_method(strand, &value)
            }
            sym::INSERT => {
                let ([idx, s], []) = unpack!(strand, args, 2, 0)?;
                let idx_i64 = idx.to_i64(strand).map_err(|_| Error::index(strand))?;
                let Some(s) = s.as_str(strand.vm()) else {
                    return Err(Error::type_error(strand, "StrBuf.insert: expected Str"));
                };
                let len = this.borrow(strand)?.len();
                let idx = index::position(len, idx_i64).ok_or_else(|| Error::index(strand))?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.settle(strand);
                check_boundary(borrow.as_str(), idx, strand)?;
                let s_len = s.len();
                borrow.reserve(strand, s_len);
                strand.access(|access| unsafe {
                    let bytes = s.as_str(access).as_bytes();
                    borrow.embryo.splice_unchecked(idx, idx, bytes);
                });
                Ok(())
            }
            sym::REMOVE => {
                let ([r], []) = unpack!(strand, args, 1, 0)?;
                let len = this.borrow(strand)?.len();
                let Some((start, end)) = range::slice_bounds(&r, strand, len)? else {
                    return Err(Error::type_error(strand, "StrBuf.remove: expected a range"));
                };
                let mut borrow = this.borrow_mut(strand)?;
                borrow.settle(strand);
                let removed = {
                    let cur = borrow.as_str();
                    check_boundary(cur, start, strand)?;
                    check_boundary(cur, end, strand)?;
                    cur[start..end].to_owned()
                };
                unsafe { borrow.embryo.splice(strand, start, end, &[]) };
                Output::set(strand, out, removed.as_str());
                Ok(())
            }
            sym::TRUNCATE => {
                let ([len], []) = unpack!(strand, args, 1, 0)?;
                let len = len.to_index(strand)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.settle(strand);
                check_boundary(borrow.as_str(), len, strand)?;
                unsafe { borrow.embryo.truncate(len) };
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
                let input =
                    borrow
                        .as_str()
                        .starts_with(prefix.as_str_raw(strand).ok_or_else(|| {
                            Error::type_error(strand, "str.starts_with: not a string")
                        })?);
                Output::set(strand, out, input);
                Ok(())
            }
            sym::ENDS_WITH => {
                let ([suffix], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                let input = borrow.as_str().ends_with(
                    suffix
                        .as_str_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "str.ends_with: not a string"))?,
                );
                Output::set(strand, out, input);
                Ok(())
            }
            sym::CONTAINS => {
                let ([needle], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                let input = borrow.as_str().contains(
                    needle
                        .as_str_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "not a string"))?,
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
                    .strbuf_chunks
                    .create(strand, chunks, out);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "StrBuf.len is a field, not a method",
            )),
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

    /// Appends the value's string form verbatim.
    ///
    /// No line terminator is added: framing is the caller's decision, expressed
    /// with `crimp`/`precrimp` when wanted. That is what lets a `StrBuf` stand in
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

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
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
        crate::fmt!(strand, w, "<type std.StrBuf>")
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
        let mut buf = StrBuf::new();
        if let Some(initial) = &initial
            && !buf.copy_from_value(strand, initial)
        {
            return Err(Error::type_error(strand, "StrBuf: expected Str"));
        }
        strand.builtin_types().strbuf.create(strand, buf, out);
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
                let mut buf = StrBuf::new();
                if let Some(initial) = &initial
                    && !buf.copy_from_value(strand, initial)
                {
                    return Err(Error::type_error(strand, "StrBuf: expected Str"));
                }
                strand
                    .builtin_types()
                    .strbuf
                    .create(strand, buf, Slot::reborrow(&mut out));
                let native = out.take();
                self_val.op_fill(strand, &strand.singletons().strbuf, native)?;
                Ok(())
            }
            _ => type_mcall_fallback(strand, &strand.singletons().strbuf, method, args, out).await,
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

/// Iterator produced by `StrBuf.drain()`. Each step drains up to `chunk_size` bytes
/// (rounded to a UTF-8 char boundary — down if that still yields a non-empty chunk,
/// otherwise up) off the *front* of the shared source buffer by advancing its `offset`
/// field, never memmoving the remaining tail — draining stays O(1) per chunk rather than
/// O(remaining length). Because the drain cursor (`offset`) lives on the buffer itself
/// rather than on this iterator, several `Chunks` iterators over the same `StrBuf`
/// interleave draining the same stream rather than needing to invalidate each other.
pub(crate) struct Chunks<'v> {
    buf: GcObj<'v, StrBuf<'v>>,
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
        crate::fmt!(strand, w, "<StrBuf drain iterator>")
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
        let s = buf.as_str();
        let mut end = want;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            end = want;
            while !s.is_char_boundary(end) {
                end += 1;
            }
        }
        let chunk = s[..end].to_owned();
        buf.offset += end;
        Output::set(strand, out, chunk.as_str());
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
