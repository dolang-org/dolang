use std::{
    fmt,
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::{
    arg::Args,
    bytecode::Variadic,
    error::{Error, Result, ResultExt},
    gc::{Collect, arena::Visit},
    object::protocol::GcObj,
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{self, Output, Slot, Slots, Value},
    vm::Vm,
};

use super::{
    BoundMethod, index, iter,
    protocol::{Inspect, Protocol, Recv, dispatch_native_method},
    range,
};

unsafe impl Collect for str {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

async fn value_to_pattern<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Vec<char>> {
    if let Some(str) = value.as_str_raw(strand) {
        return Ok(str.chars().collect());
    }

    strand
        .with_slots(async move |strand, [mut input, mut elem]| {
            let mut acc = Vec::new();
            value.iter(strand, &mut input).await?;
            while input.next(strand, &mut elem).await? {
                acc.extend(
                    elem.as_str_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "invalid pattern: not a string"))?
                        .chars(),
                );
                strand.check_interrupt_gc()?;
            }
            Ok(acc)
        })
        .await
}

impl<'v> Protocol<'v> for str {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().str.dup())
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.receiver.get()).into_do(strand)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "\"").into_do(strand)?;
        for char in this.receiver.get().chars() {
            match char {
                '"' | '\\' | '$' => write!(w, "\\{char}"),
                '\t' => write!(w, "\\t"),
                '\r' => write!(w, "\\r"),
                '\n' => write!(w, "\\n"),
                '\0' => write!(w, "\0"),
                _ => write!(w, "{char}"),
            }
            .into_do(strand)?
        }
        write!(w, "\"").into_do(strand)
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, _strand: &'a mut Strand<'v, 's>) -> bool {
        !this.get().is_empty()
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(ostr) = other.downcast_ref(strand.builtin_types().str) {
            Ok(Value::from_bool(ostr.get() == this.get()))
        } else {
            Ok(Value::from_bool(false))
        }
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(ostr) = other.downcast_ref(strand.builtin_types().str) {
            Ok(Value::from_bool(this.get() < ostr.get()))
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        this.get().hash(hasher);
        Ok(())
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.get();
        let Some((start, end)) = range::slice_bounds(index, strand, me.len())? else {
            return Err(Error::index(strand));
        };
        let slice = me
            .get(start..end)
            .ok_or_else(|| Error::runtime(strand, "invalid UTF-8 substring boundaries"))?;
        Output::set(strand, out, slice);
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.get();

        match method.tag() {
            sym::STARTS_WITH => {
                let ([prefix], []) = unpack!(strand, args, 1, 0)?;
                let input =
                    this.get()
                        .starts_with(prefix.as_str_raw(strand).ok_or_else(|| {
                            Error::type_error(strand, "str.starts_with: not a string")
                        })?);
                Output::set(strand, out, input);
                Ok(())
            }
            sym::WITHOUT_PREFIX => {
                let ([prefix], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.get();
                let input = borrow
                    .strip_prefix(prefix.as_str_raw(strand).ok_or_else(|| {
                        Error::type_error(strand, "str.without_prefix: not a string")
                    })?)
                    .unwrap_or(borrow);
                Output::set(strand, out, input);
                Ok(())
            }
            sym::ENDS_WITH => {
                let ([suffix], []) = unpack!(strand, args, 1, 0)?;
                let input = this.get().ends_with(
                    suffix
                        .as_str_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "str.ends_with: not a string"))?,
                );
                Output::set(strand, out, input);
                Ok(())
            }
            sym::WITHOUT_SUFFIX => {
                let ([suffix], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.get();
                let input = borrow
                    .strip_suffix(suffix.as_str_raw(strand).ok_or_else(|| {
                        Error::type_error(strand, "str.without_suffix: not a string")
                    })?)
                    .unwrap_or(borrow);
                Output::set(strand, out, input);
                Ok(())
            }
            sym::SPLIT | sym::RSPLIT => {
                let method_sym = method.tag();
                let limit = Sym::well_known(sym::LIMIT);
                let ([delim], [limit]) = unpack!(strand, args, 1, 0, limit = None)?;
                let limit_i64 = limit
                    .map(|l| {
                        l.as_i64(strand)
                            .ok_or_else(|| Error::type_error(strand, "limit: expected `int`"))
                    })
                    .transpose()?;
                let delim_gc = delim
                    .downcast_ref(strand.builtin_types().str)
                    .ok_or_else(|| {
                        Error::type_error(strand, "str.split/rsplit: delimiter not a string")
                    })?
                    .to_strong();
                let forward = method_sym == sym::SPLIT;
                let state = match limit_i64 {
                    Some(l) if l < 0 => {
                        // Negative limit: split from the opposite end, buffer in yield order.
                        let n = (l.unsigned_abs() as usize).saturating_add(1);
                        let src: &str = this.receiver.get();
                        let base = src.as_ptr() as usize;
                        let mut segs: Vec<(usize, usize)> = if forward {
                            // split(limit: -N): split from rear via rsplitn, yield forward
                            src.rsplitn(n, &*delim_gc)
                                .map(|s| {
                                    let st = s.as_ptr() as usize - base;
                                    (st, st + s.len())
                                })
                                .collect()
                        } else {
                            // rsplit(limit: -N): split from front via splitn, yield backward
                            src.splitn(n, &*delim_gc)
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
                                offset: Some(this.receiver.get().len()),
                                limit,
                                reverse: true,
                            }
                        }
                    }
                };
                out.store(Value::from_object(GcObj::new(
                    strand.vm().arena(),
                    strand.vm().builtin_types().str_split,
                    Split {
                        str: this.to_strong(),
                        delim: delim_gc,
                        state,
                        forward,
                    },
                )));
                Ok(())
            }
            sym::JOIN => {
                let ([], [arg]) = unpack!(strand, args, 0, 1)?;
                strand
                    .with_slots(async move |strand, [mut input, mut value]| {
                        if let Some(arg) = arg {
                            arg.iter(strand, &mut input).await?
                        } else {
                            strand.input(&mut input)
                        }
                        let mut acc = String::new();
                        if input.next(strand, &mut value).await? {
                            value.op_display(strand, &mut acc)?;
                        }
                        while input.next(strand, &mut value).await? {
                            acc.push_str(this.receiver.get());
                            value.op_display(strand, &mut acc)?;
                            strand.check_interrupt_gc()?;
                        }
                        out.store(Value::from_str(strand, &acc));
                        Ok(())
                    })
                    .await
            }
            sym::TRIM => {
                let ([], [chars]) = unpack!(strand, args, 0, 1)?;
                let trimmed = match chars {
                    None => me.trim(),
                    Some(chars) => {
                        let pattern = value_to_pattern(strand, &chars).await?;
                        me.trim_matches(pattern.as_slice())
                    }
                };
                Output::set(strand, out, trimmed);
                Ok(())
            }
            sym::TRIM_START => {
                let ([], [chars]) = unpack!(strand, args, 0, 1)?;
                let trimmed = match chars {
                    None => me.trim_start(),
                    Some(chars) => {
                        let pattern = value_to_pattern(strand, &chars).await?;
                        me.trim_start_matches(pattern.as_slice())
                    }
                };
                Output::set(strand, out, trimmed);
                Ok(())
            }
            sym::TRIM_END => {
                let ([], [chars]) = unpack!(strand, args, 0, 1)?;
                let trimmed = match chars {
                    None => me.trim_end(),
                    Some(chars) => {
                        let pattern = value_to_pattern(strand, &chars).await?;
                        me.trim_end_matches(pattern.as_slice())
                    }
                };
                Output::set(strand, out, trimmed);
                Ok(())
            }
            sym::SUB => {
                let ([start], [end]) = unpack!(strand, args, 1, 1)?;
                let start = start.as_i64(strand).ok_or_else(|| Error::index(strand))?;
                let start = index::position(me.len(), start)
                    .ok_or_else(|| Error::runtime(strand, "invalid UTF-8 substring boundaries"))?;
                let slice = match end {
                    None => me.get(start..),
                    Some(end) => {
                        let end = end.as_i64(strand).ok_or_else(|| Error::index(strand))?;
                        let end = index::position(me.len(), end).ok_or_else(|| {
                            Error::runtime(strand, "invalid UTF-8 substring boundaries")
                        })?;
                        me.get(start..end)
                    }
                }
                .ok_or_else(|| Error::runtime(strand, "invalid UTF-8 substring boundaries"))?;
                Output::set(strand, out, slice);
                Ok(())
            }
            sym::UPPER => {
                Output::set(strand, out, me.to_uppercase().as_str());
                Ok(())
            }
            sym::LOWER => {
                Output::set(strand, out, me.to_lowercase().as_str());
                Ok(())
            }
            sym::REPLACE => {
                let ([from, to], []) = unpack!(strand, args, 2, 0)?;
                let from = from
                    .as_str_raw(strand)
                    .ok_or_else(|| Error::type_error(strand, "old value is not a string"))?;
                let to = to
                    .as_str_raw(strand)
                    .ok_or_else(|| Error::type_error(strand, "new value is not a string"))?;
                Output::set(strand, out, me.replace(from, to).as_str());
                Ok(())
            }
            sym::REPEAT => {
                let ([count], []) = unpack!(strand, args, 1, 0)?;
                let count = count.to_index(strand)?;
                let len = me
                    .len()
                    .checked_mul(count)
                    .ok_or_else(|| Error::overflow(strand))?;
                if len > isize::MAX as usize {
                    return Err(Error::overflow(strand));
                }
                Output::set(strand, out, me.repeat(count).as_str());
                Ok(())
            }
            sym::CONTAINS => {
                let ([needle], []) = unpack!(strand, args, 1, 0)?;
                let input = this.get().contains(
                    needle
                        .as_str_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "not a string"))?,
                );
                Output::set(strand, out, input);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "str.len is a field, not a method",
            )),
            _ => Err(Error::field(strand, method)),
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
                Output::set(strand, out, this.receiver.get().len() as i64);
                Ok(())
            }
            sym::STARTS_WITH
            | sym::WITHOUT_PREFIX
            | sym::ENDS_WITH
            | sym::WITHOUT_SUFFIX
            | sym::SPLIT
            | sym::RSPLIT
            | sym::UPPER
            | sym::LOWER
            | sym::SUB
            | sym::JOIN
            | sym::TRIM
            | sym::TRIM_START
            | sym::TRIM_END
            | sym::REPEAT
            | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }
}

enum SplitState {
    /// Lazy iteration. `reverse=false`: scan left-to-right; `reverse=true`: right-to-left.
    /// `offset` is the start byte (forward) or end byte (reverse) of the remaining string;
    /// `None` means exhausted.
    Lazy {
        offset: Option<usize>,
        limit: usize,
        reverse: bool,
    },
    /// Pre-computed segments (byte ranges into `str`) stored in yield order.
    /// Used when split direction differs from yield direction.
    Buffered {
        segments: Vec<(usize, usize)>,
        index: usize,
    },
}

pub(crate) struct Split<'v> {
    str: GcObj<'v, str>,
    delim: GcObj<'v, str>,
    state: SplitState,
    /// `true` = yield left-to-right (split); `false` = right-to-left (rsplit).
    forward: bool,
}

impl<'v> Split<'v> {
    /// Returns the next segment as a `(start, end)` byte range within `self.str`,
    /// advancing internal state. Returns `None` when exhausted.
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
                        && let Some((before, _)) = self.str[off..].split_once(&*self.delim)
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
                        && let Some((before, _after)) = self.str[..off].rsplit_once(&*self.delim)
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
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.str.accept(visit)
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Protocol<'v> for Split<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let forward = this.borrow_mut(strand)?.forward;
        let label = if forward {
            "<str split>"
        } else {
            "<str rsplit>"
        };
        write!(w, "{label}").into_do(strand)
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

        let mut borrow = this.borrow_mut(strand)?;

        // Fill required positional slots
        for i in 0..sig.required {
            let Some((start, end)) = borrow.next_segment() else {
                return Err(Error::missing_positional(strand, sig.required));
            };
            out.at(i)
                .store(Value::from_str(strand, &borrow.str[start..end]));
        }

        // Fill optional positional slots
        for i in 0..sig.optional.len() {
            if let Some((start, end)) = borrow.next_segment() {
                out.at(sig.required + i)
                    .store(Value::from_str(strand, &borrow.str[start..end]));
            } else {
                out.at(sig.required + i).store(sig.optional[i].dup());
            }
        }

        // If variadic, assign this (now with updated state) to variadic slot
        match sig.variadic {
            Variadic::None | Variadic::Discard => {}
            Variadic::Capture => {
                value::Output::set(strand, out.at(pos_count + sig.keys.len()), &this);
            }
        }

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
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.str>").into_do(strand)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Constructor: str(value) - convert to string
        let ([value], _) = unpack!(strand, args, 1, 0)?;
        let str = value.to_string(strand)?;
        Output::set(strand, out, str.as_str());
        Ok(())
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: vec![
                Sym::well_known(sym::STR_METHOD),
                Sym::well_known(sym::DBG_METHOD),
                Sym::well_known(sym::EQ_METHOD),
                Sym::well_known(sym::LT_METHOD),
                Sym::well_known(sym::BOOL_METHOD),
                Sym::well_known(sym::HASH_METHOD),
                Sym::well_known(sym::LEN),
                Sym::well_known(sym::STARTS_WITH),
                Sym::well_known(sym::WITHOUT_PREFIX),
                Sym::well_known(sym::ENDS_WITH),
                Sym::well_known(sym::WITHOUT_SUFFIX),
                Sym::well_known(sym::SPLIT),
                Sym::well_known(sym::RSPLIT),
                Sym::well_known(sym::JOIN),
                Sym::well_known(sym::TRIM),
                Sym::well_known(sym::TRIM_START),
                Sym::well_known(sym::TRIM_END),
                Sym::well_known(sym::SUB),
                Sym::well_known(sym::UPPER),
                Sym::well_known(sym::LOWER),
                Sym::well_known(sym::REPEAT),
                Sym::well_known(sym::CONTAINS),
            ],
        })
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
            | sym::TRIM
            | sym::TRIM_START
            | sym::TRIM_END
            | sym::SUB
            | sym::UPPER
            | sym::LOWER
            | sym::REPEAT
            | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
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
                let ([self_val, value], []) = unpack!(strand, args, 2, 0)?;
                let s = value.to_string(strand)?;
                let native = Value::from_str(strand, s.as_str());
                self_val.op_fill(strand, &strand.vm().singletons().str, native)?;
                Ok(())
            }
            _ => {
                let vm = strand.vm();
                dispatch_native_method(strand, &vm.singletons().str, method, args, out).await
            }
        }
    }
}
