use std::{
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::value::fmt;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit, with_pin},
    object::protocol::{GcObjPin, instance_get_fallback, members},
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{BinEmbryo, Nil, Output, Slot, Slots, Value, view::PinBin, with_u8_slice},
    vm::Vm,
    with_slots,
};

use super::{
    BoundMethod, iter,
    protocol::{Inspect, Protocol, Recv, instance_mcall_fallback, type_mcall_fallback},
    range,
};

use bstr::{BStr, ByteSlice};

/// A `Bin` object. Its bytes are the tail of its annex.
pub(crate) struct BinObj;

unsafe impl Collect for BinObj {
    const CYCLIC: bool = false;
    type Annex = gc::Tail<(), [u8]>;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

/// Collects a trim pattern as a set of bytes.
///
/// A [`Bin`]([u8]) contributes its bytes, as does each element of an iterable
/// of them. A `Str` is not accepted: the pattern is a *set*, so a multi-byte
/// character would be split into bytes that mean nothing on their own.
async fn value_to_pattern<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Vec<u8>> {
    if let Some(bin) = value.as_bin(strand) {
        return Ok(bin.to_vec());
    }
    if value.as_str(strand).is_some() {
        return Err(Error::type_error(
            strand,
            "invalid pattern: not binary data",
        ));
    }

    with_slots!(strand, |mut input, mut elem| {
        let mut acc = Vec::new();
        value.iter(strand, &mut input).await?;
        while input.next(strand, &mut elem).await? {
            let bin = elem
                .as_bin(strand)
                .ok_or_else(|| Error::type_error(strand, "invalid pattern: not binary data"))?;
            acc.extend_from_slice(&bin.to_vec());
            strand.check_trap_gc()?;
        }
        Ok(acc)
    })
}

fn trim_start_bytes<'a>(me: &'a [u8], pattern: &[u8]) -> &'a [u8] {
    let start = me
        .iter()
        .position(|b| !pattern.contains(b))
        .unwrap_or(me.len());
    &me[start..]
}

fn trim_end_bytes<'a>(me: &'a [u8], pattern: &[u8]) -> &'a [u8] {
    let end = me
        .iter()
        .rposition(|b| !pattern.contains(b))
        .map_or(0, |i| i + 1);
    &me[..end]
}

impl<'v, 'a> Recv<'v, 'a, BinObj> {
    /// The binary's bytes.
    pub(crate) fn bytes(&self) -> &'a [u8] {
        &self.annex().tail
    }
}

impl<'v, 'a> GcObjPin<'v, 'a, BinObj> {
    /// The binary's bytes.
    pub(crate) fn bytes(&self) -> &'a [u8] {
        &self.annex().tail
    }
}

impl<'v> Protocol<'v> for BinObj {
    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &fmt::Spec,
        w: &mut dyn fmt::Format<'v>,
    ) -> Result<'v, 's, ()> {
        use fmt::{Fill, Kind, Pad};

        let kind = spec.kind.ok_or_else(|| fmt::unresolved_kind(strand))?;
        if kind.is_text() {
            if spec.sign.is_some() || spec.alt || spec.fill == Fill::Zero {
                return Err(Error::type_error(
                    strand,
                    "unsupported binary format option",
                ));
            }
            let mut pad = Pad::new(*spec, w);
            match kind {
                Kind::Str => Self::op_display(this, strand, &mut pad)?,
                Kind::Dbg => Self::op_debug(this, strand, &mut pad)?,
                Kind::Verbatim => Self::op_verbatim(this, strand, &mut pad)?,
                _ => unreachable!(),
            }
            return pad.finish(strand);
        }
        if spec.sign.is_some()
            || spec.precision.is_some()
            || matches!(kind, Kind::Exp | Kind::Fixed)
            || (spec.alt && kind == Kind::Dec)
        {
            return Err(Error::type_error(
                strand,
                "unsupported binary format option",
            ));
        }
        let mut digits = String::new();
        for byte in this.bytes() {
            use std::fmt::Write as _;
            match kind {
                Kind::Hex => write!(&mut digits, "{byte:02x}"),
                Kind::Oct => write!(&mut digits, "{byte:03o}"),
                Kind::Bin => write!(&mut digits, "{byte:08b}"),
                Kind::Dec => write!(&mut digits, "{byte:03}"),
                _ => unreachable!(),
            }
            .unwrap();
        }
        let prefix = if spec.alt {
            match kind {
                Kind::Hex => "0x",
                Kind::Oct => "0o",
                Kind::Bin => "0b",
                _ => "",
            }
        } else {
            ""
        };
        fmt::finish_numeric(strand, spec, w, "", prefix, &digits)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().bin)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "{}", BStr::new(this.bytes()))
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "b{:?}", BStr::new(this.bytes()))
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        Ok(!this.bytes().is_empty())
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(oslice) = other.downcast_ref(strand.builtin_types().bin) {
            let eq = with_pin!(strand, oslice, |oslice| oslice.bytes() == this.bytes());
            Ok(Value::from_bool(eq))
        } else {
            Err(Error::unsupported(strand, "`==`"))
        }
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(oslice) = other.downcast_ref(strand.builtin_types().bin) {
            let lt = with_pin!(strand, oslice, |oslice| this.bytes() < oslice.bytes());
            Ok(Value::from_bool(lt))
        } else {
            Err(Error::unsupported(strand, "`<`"))
        }
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        this.bytes().hash(hasher);
        Ok(())
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.bytes();
        let Some(slice) = range::slice(index, strand, me.len())? else {
            return Err(Error::index(strand));
        };
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
            sym::STARTS_WITH => {
                unpack!(strand, args => prefix);
                let input =
                    with_u8_slice!(strand, prefix, |prefix| this.bytes().starts_with(prefix))
                        .ok_or_else(|| {
                            let msg = "not binary data: unknown".to_string();
                            Error::type_error(strand, msg)
                        })?;
                Output::set(strand, out, input);
                Ok(())
            }
            sym::WITHOUT_PREFIX => {
                unpack!(strand, args => prefix);
                let borrow = this.bytes();
                with_u8_slice!(strand, prefix, |prefix| {
                    Output::set(strand, out, borrow.strip_prefix(prefix).unwrap_or(borrow))
                })
                .ok_or_else(|| {
                    let msg = "not binary data: unknown".to_string();
                    Error::type_error(strand, msg)
                })?;
                Ok(())
            }
            sym::ENDS_WITH => {
                unpack!(strand, args => suffix);
                let input = with_u8_slice!(strand, suffix, |suffix| this.bytes().ends_with(suffix))
                    .ok_or_else(|| {
                        let msg = "not binary data: unknown".to_string();
                        Error::type_error(strand, msg)
                    })?;
                Output::set(strand, out, input);
                Ok(())
            }
            sym::WITHOUT_SUFFIX => {
                unpack!(strand, args => suffix);
                let borrow = this.bytes();
                with_u8_slice!(strand, suffix, |suffix| {
                    Output::set(strand, out, borrow.strip_suffix(suffix).unwrap_or(borrow))
                })
                .ok_or_else(|| {
                    let msg = "not binary data: unknown".to_string();
                    Error::type_error(strand, msg)
                })?;
                Ok(())
            }
            sym::SPLIT | sym::RSPLIT => {
                let method_sym = method.tag();
                let limit = Sym::well_known(sym::LIMIT);
                unpack!(strand, args => delim, ?:limit);
                let limit_i64 = limit
                    .map(|l| {
                        l.to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "limit: expected `Int`"))
                    })
                    .transpose()?;
                let delim_gc = delim
                    .downcast_ref(strand.builtin_types().bin)
                    .ok_or_else(|| {
                        let msg = "not binary data: unknown".to_string();
                        Error::type_error(strand, msg)
                    })?;
                // SAFETY: the pin owns its reference as a root
                let delim_gc = PinBin::new(unsafe { delim_gc.dup() });
                let forward = method_sym == sym::SPLIT;
                let state = match limit_i64 {
                    Some(l) if l < 0 => {
                        let n = (l.unsigned_abs() as usize).saturating_add(1);
                        let src: &[u8] = this.bytes();
                        let base = src.as_ptr() as usize;
                        let mut segs: Vec<(usize, usize)> = if forward {
                            src.rsplitn_str(n, &*delim_gc)
                                .map(|s| {
                                    let st = s.as_ptr() as usize - base;
                                    (st, st + s.len())
                                })
                                .collect()
                        } else {
                            src.splitn_str(n, &*delim_gc)
                                .map(|s| {
                                    let st = s.as_ptr() as usize - base;
                                    (st, st + s.len())
                                })
                                .collect()
                        };
                        segs.reverse();
                        SplitState::Buffered {
                            segments: segs,
                            index: 0,
                        }
                    }
                    _ => {
                        let limit = limit_i64
                            .map(|l| l.try_into().map_err(|_| Error::overflow(strand)))
                            .transpose()?
                            .unwrap_or(usize::MAX);
                        if forward {
                            SplitState::Lazy {
                                offset: Some(0),
                                limit,
                                reverse: false,
                            }
                        } else {
                            SplitState::Lazy {
                                offset: Some(this.bytes().len()),
                                limit,
                                reverse: true,
                            }
                        }
                    }
                };
                strand.builtin_types().bin_split.create(
                    strand,
                    Split {
                        str: PinBin::new(this.to_strong()),
                        delim: delim_gc,
                        state,
                        forward,
                    },
                    out,
                );
                Ok(())
            }
            sym::JOIN => {
                unpack!(strand, args => ?arg);
                with_slots!(strand, |mut input, mut value| {
                    if let Some(arg) = arg {
                        arg.iter(strand, &mut input).await?
                    } else {
                        strand.input(&mut input)
                    }
                    let mut acc = BinEmbryo::new();
                    if input.next(strand, &mut value).await? {
                        with_u8_slice!(strand, value, |slice| acc.extend(strand, slice))
                            .ok_or_else(|| {
                                Error::type_error(strand, "element was not binary data")
                            })?;
                    }
                    while input.next(strand, &mut value).await? {
                        acc.extend(strand, this.bytes());
                        with_u8_slice!(strand, value, |slice| acc.extend(strand, slice))
                            .ok_or_else(|| {
                                Error::type_error(strand, "element was not binary data")
                            })?;
                    }
                    acc.finish(strand, out);
                    Ok(())
                })
            }
            sym::CHOMP => {
                let me = this.bytes();
                unpack!(strand, args =>);
                let end = me.len() - iter::line_terminator_len(me);
                Output::set(strand, out, &me[..end]);
                Ok(())
            }
            sym::TRIM => {
                let me = this.bytes();
                unpack!(strand, args => ?chars);
                let trimmed = match chars {
                    None => me.trim(),
                    Some(chars) => {
                        let pattern = value_to_pattern(strand, &chars).await?;
                        trim_end_bytes(trim_start_bytes(me, &pattern), &pattern)
                    }
                };
                Output::set(strand, out, trimmed);
                Ok(())
            }
            sym::TRIM_START => {
                let me = this.bytes();
                unpack!(strand, args => ?chars);
                let trimmed = match chars {
                    None => me.trim_start(),
                    Some(chars) => {
                        let pattern = value_to_pattern(strand, &chars).await?;
                        trim_start_bytes(me, &pattern)
                    }
                };
                Output::set(strand, out, trimmed);
                Ok(())
            }
            sym::TRIM_END => {
                let me = this.bytes();
                unpack!(strand, args => ?chars);
                let trimmed = match chars {
                    None => me.trim_end(),
                    Some(chars) => {
                        let pattern = value_to_pattern(strand, &chars).await?;
                        trim_end_bytes(me, &pattern)
                    }
                };
                Output::set(strand, out, trimmed);
                Ok(())
            }
            sym::CONTAINS => {
                unpack!(strand, args => needle);
                let input =
                    with_u8_slice!(strand, needle, |needle| this.bytes().contains_str(needle))
                        .ok_or_else(|| {
                            let msg = "not binary data: unknown".to_string();
                            Error::type_error(strand, msg)
                        })?;
                Output::set(strand, out, input);
                Ok(())
            }
            sym::FIND | sym::RFIND => {
                let range = Sym::well_known(sym::RANGE);
                unpack!(strand, args => needle, ?:range);
                let me = this.bytes();
                let (start, end) = range::window(strand, range.as_deref(), me.len())?;
                let window = &me[start..end];
                let found = with_u8_slice!(strand, needle, |needle| {
                    if method.tag() == sym::FIND {
                        window.find(needle)
                    } else {
                        window.rfind(needle)
                    }
                })
                .ok_or_else(|| {
                    let msg = "not binary data: unknown".to_string();
                    Error::type_error(strand, msg)
                })?;
                match found {
                    Some(offset) => Output::set(strand, out, start + offset),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            }
            sym::REPLACE => {
                let range = Sym::well_known(sym::RANGE);
                unpack!(strand, args => from, to, ?:range);
                let me = this.bytes();
                let (start, end) = range::window(strand, range.as_deref(), me.len())?;
                let window = &me[start..end];
                let mut acc = BinEmbryo::new_with_capacity(strand, me.len());
                acc.extend(strand, &me[..start]);
                let mut last = 0;
                with_u8_slice!(strand, from, |from| {
                    with_u8_slice!(strand, to, |to| {
                        for at in window.find_iter(from) {
                            acc.extend(strand, &window[last..at]);
                            acc.extend(strand, to);
                            last = at + from.len();
                        }
                    })
                    .ok_or_else(|| Error::type_error(strand, "new value is not binary data"))
                })
                .ok_or_else(|| Error::type_error(strand, "old value is not binary data"))??;
                acc.extend(strand, &window[last..]);
                acc.extend(strand, &me[end..]);
                acc.finish(strand, out);
                Ok(())
            }
            sym::REPEAT => {
                unpack!(strand, args => count);
                let count = count.to_index(strand)?;
                let me = this.bytes();
                let len = me
                    .len()
                    .checked_mul(count)
                    .ok_or_else(|| Error::overflow(strand))?;
                if len > isize::MAX as usize {
                    return Err(Error::overflow(strand));
                }
                let mut acc = BinEmbryo::new_with_capacity(strand, len);
                for _ in 0..count {
                    acc.extend(strand, me);
                }
                acc.finish(strand, out);
                Ok(())
            }
            sym::UNPACK => {
                unpack!(strand, args =>);
                super::array_view::unpack_bin(strand, Value::from_object(this.to_strong()), out);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(strand, "len is a field, not a method")),
            _ => instance_mcall_fallback(strand, &this, method, args, out),
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
                Output::set(strand, out, this.bytes().len());
                Ok(())
            }
            sym::STARTS_WITH
            | sym::WITHOUT_PREFIX
            | sym::ENDS_WITH
            | sym::WITHOUT_SUFFIX
            | sym::SPLIT
            | sym::RSPLIT
            | sym::JOIN
            | sym::CHOMP
            | sym::TRIM
            | sym::TRIM_START
            | sym::TRIM_END
            | sym::CONTAINS
            | sym::FIND
            | sym::RFIND
            | sym::REPLACE
            | sym::REPEAT
            | sym::UNPACK => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }
}

enum SplitState {
    Lazy {
        offset: Option<usize>,
        limit: usize,
        reverse: bool,
    },
    Buffered {
        segments: Vec<(usize, usize)>,
        index: usize,
    },
}

/// Clones the remaining state; a buffered clone keeps only unconsumed segments.
impl Clone for SplitState {
    fn clone(&self) -> Self {
        match self {
            Self::Lazy {
                offset,
                limit,
                reverse,
            } => Self::Lazy {
                offset: *offset,
                limit: *limit,
                reverse: *reverse,
            },
            Self::Buffered { segments, index } => Self::Buffered {
                segments: segments[*index..].to_vec(),
                index: 0,
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct Split<'v> {
    // Pinned, since segments are copied out while allocating
    str: PinBin<'v>,
    delim: PinBin<'v>,
    state: SplitState,
    forward: bool,
}

impl<'v> Split<'v> {
    fn next_segment(&mut self) -> Option<(usize, usize)> {
        match &mut self.state {
            SplitState::Buffered { segments, index } => {
                if *index >= segments.len() {
                    return None;
                }
                let r = segments[*index];
                *index += 1;
                Some(r)
            }
            SplitState::Lazy {
                offset,
                limit,
                reverse,
            } => {
                let off = (*offset)?;
                let delim_len = self.delim.len();
                if !*reverse {
                    if *limit != 0
                        && let Some((before, _)) = self.str[off..].split_once_str(&*self.delim)
                    {
                        let end = off + before.len();
                        *offset = Some(end + delim_len);
                        *limit -= 1;
                        Some((off, end))
                    } else {
                        *offset = None;
                        Some((off, self.str.len()))
                    }
                } else {
                    if *limit != 0
                        && let Some((before, _after)) =
                            self.str[..off].rsplit_once_str(&*self.delim)
                    {
                        let after_start = before.len() + delim_len;
                        *offset = Some(before.len());
                        *limit -= 1;
                        Some((after_start, off))
                    } else {
                        *offset = None;
                        Some((0, off))
                    }
                }
            }
        }
    }
}

unsafe impl<'v> Collect for Split<'v> {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.str.obj().accept(visit)
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Protocol<'v> for Split<'v> {
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: crate::object::protocol::SpreadContext,
        sink: &'a mut dyn crate::object::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Format<'v>,
    ) -> Result<'v, 's, ()> {
        let forward = this.borrow_mut(strand)?.forward;
        let label = if forward {
            "<bin split>"
        } else {
            "<bin rsplit>"
        };
        crate::fmt!(strand, w, "{label}")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Reject keys without defaults
        for key in &sig.keys {
            if key.default.is_none() {
                return Err(match &key.kind {
                    sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                    sig::UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
                });
            }
        }

        // Fill keys with defaults
        let pos_count = sig.required + sig.optional.len();
        for (i, key) in sig.keys.iter().enumerate() {
            out.at(pos_count + i)
                .store(key.default.as_ref().unwrap().dup());
        }

        let mut borrow = this.borrow(strand)?.clone();

        // Fill required positional slots
        for i in 0..sig.required {
            let Some((start, end)) = borrow.next_segment() else {
                return Err(Error::missing_positional(strand, sig.required));
            };
            out.at(i)
                .store(Value::from_u8_slice(strand, &borrow.str[start..end]));
        }

        // Fill optional positional slots
        for i in 0..sig.optional.len() {
            if let Some((start, end)) = borrow.next_segment() {
                out.at(sig.required + i)
                    .store(Value::from_u8_slice(strand, &borrow.str[start..end]));
            } else {
                out.at(sig.required + i).store(sig.optional[i].dup());
            }
        }

        if sig.pos_rest() == crate::bytecode::Rest::None && borrow.next_segment().is_some() {
            return Err(Error::unexpected_positional(strand, pos_count));
        }

        if let Some(i) = sig.pos_rest_slot() {
            strand
                .builtin_types()
                .bin_split
                .create(strand, borrow, out.at(i));
        }
        sig.fill_empty_key_rest(strand, &mut out);

        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let Some((start, end)) = borrow.next_segment() else {
            return Ok(false);
        };
        Output::set(strand, out, &borrow.str[start..end]);
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

pub(crate) struct Class;

unsafe impl Collect for Class {
    const CYCLIC: bool = false;
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
        w: &mut dyn fmt::Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Bin>")
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => value);
        with_u8_slice!(strand, value, |slice| Output::set(strand, out, slice))
            .ok_or_else(|| Error::type_error(strand, "Bin: expected Str or Bin"))
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            is_final: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
                Method(sym::PACK),
            ],
            members: members![
                Method(sym::INIT_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::LT_METHOD),
                Method(sym::BOOL_METHOD),
                Method(sym::HASH_METHOD),
                Getter(sym::LEN),
                Method(sym::STARTS_WITH),
                Method(sym::WITHOUT_PREFIX),
                Method(sym::ENDS_WITH),
                Method(sym::WITHOUT_SUFFIX),
                Method(sym::SPLIT),
                Method(sym::RSPLIT),
                Method(sym::JOIN),
                Method(sym::CHOMP),
                Method(sym::TRIM),
                Method(sym::TRIM_START),
                Method(sym::TRIM_END),
                Method(sym::CONTAINS),
                Method(sym::FIND),
                Method(sym::RFIND),
                Method(sym::REPLACE),
                Method(sym::REPEAT),
                Method(sym::UNPACK),
            ],
        })
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => self_val, value);
                let native =
                    with_u8_slice!(strand, value, |slice| Value::from_u8_slice(strand, slice))
                        .ok_or_else(|| Error::type_error(strand, "Bin: expected Str or Bin"))?;
                self_val.op_fill(strand, &strand.singletons().bin, native)?;
                Ok(())
            }
            sym::PACK => {
                unpack!(strand, args => obj);
                with_slots!(strand, |mut iter, mut value| {
                    let mut acc = Vec::new();
                    obj.iter(strand, &mut iter).await?;
                    while iter.next(strand, &mut value).await? {
                        let value = value
                            .to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "non-integer element"))?;
                        let value: u8 = value.try_into().map_err(|_| Error::overflow(strand))?;
                        acc.push(value);
                    }
                    Output::set(strand, out, acc.as_slice());
                    Ok(())
                })
            }
            _ => type_mcall_fallback(strand, &strand.singletons().bin, method, args, out).await,
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
            | sym::PACK
            | sym::UNPACK
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::BOOL_METHOD
            | sym::HASH_METHOD
            | sym::LEN
            | sym::STARTS_WITH
            | sym::WITHOUT_PREFIX
            | sym::ENDS_WITH
            | sym::WITHOUT_SUFFIX
            | sym::SPLIT
            | sym::RSPLIT
            | sym::JOIN
            | sym::CHOMP
            | sym::TRIM
            | sym::TRIM_START
            | sym::TRIM_END
            | sym::CONTAINS
            | sym::FIND
            | sym::RFIND
            | sym::REPLACE
            | sym::REPEAT
            | sym::UNPACK => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }
}
