use std::{
    hash::{DefaultHasher, Hash},
    mem,
    ops::ControlFlow,
    str::Chars,
};

use crate::value::fmt::Format;

use unicode_segmentation::{Graphemes, UnicodeSegmentation};

use crate::{
    arg::Args,
    bytecode::Rest,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit, with_pin},
    object::protocol::{GcObjPin, instance_get_fallback, members},
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Nil, Output, Slot, Slots, StrEmbryo, Value, view::PinStr},
    vm::Vm,
    with_slice, with_slots, with_str,
};

use super::{
    BoundMethod, iter,
    protocol::{Inspect, Protocol, Recv, instance_mcall_fallback, type_mcall_fallback},
    range,
};

/// A `Str` object. Its text is the tail of its annex.
pub(crate) struct StrObj;

unsafe impl Collect for StrObj {
    const CYCLIC: bool = false;
    type Annex = gc::Tail<(), str>;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

/// Collects a trim pattern as a set of characters.
///
/// A [`Str`](str) contributes its characters, as does each element of an
/// iterable of them. A `Bin` is not accepted: the pattern is a *set* of
/// characters, and bytes only become characters through a decode the caller
/// should ask for.
async fn value_to_pattern<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Vec<char>> {
    if let Some(str) = value.as_str(strand) {
        return Ok(String::from(str).chars().collect());
    }
    if value.as_bin(strand).is_some() {
        return Err(Error::type_error(strand, "invalid pattern: not a string"));
    }

    with_slots!(strand, |mut input, mut elem| {
        let mut acc = Vec::new();
        value.iter(strand, &mut input).await?;
        while input.next(strand, &mut elem).await? {
            let str = elem
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "invalid pattern: not a string"))?;
            acc.extend(String::from(str).chars());
            strand.check_trap_gc()?;
        }
        Ok(acc)
    })
}

impl<'v, 'a> Recv<'v, 'a, StrObj> {
    /// The string's text.
    pub(crate) fn str(&self) -> &'a str {
        &self.annex().tail
    }
}

impl<'v, 'a> GcObjPin<'v, 'a, StrObj> {
    /// The string's text.
    pub(crate) fn str(&self) -> &'a str {
        &self.annex().tail
    }
}

impl<'v> Protocol<'v> for StrObj {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().str)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "{}", this.str())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "\"")?;
        for char in this.str().chars() {
            match char {
                '"' | '\\' | '$' => crate::fmt!(strand, w, "\\{char}"),
                '\t' => crate::fmt!(strand, w, "\\t"),
                '\r' => crate::fmt!(strand, w, "\\r"),
                '\n' => crate::fmt!(strand, w, "\\n"),
                '\0' => crate::fmt!(strand, w, "\0"),
                _ => crate::fmt!(strand, w, "{char}"),
            }?
        }
        crate::fmt!(strand, w, "\"")
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        Ok(!this.str().is_empty())
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(ostr) = other.downcast_native(strand, strand.builtin_types().str) {
            let eq = with_pin!(strand, ostr, |ostr| ostr.str() == this.str());
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
        if let Some(ostr) = other.downcast_ref(strand.builtin_types().str) {
            let lt = with_pin!(strand, ostr, |ostr| this.str() < ostr.str());
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
        this.str().hash(hasher);
        Ok(())
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.str();
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.str();

        match method.tag() {
            sym::SCALARS | sym::GRAPHEMES | sym::SCALAR_BOUNDS | sym::GRAPHEME_BOUNDS => {
                unpack!(strand, args =>);
                strand.builtin_types().str_unicode_iter.create(
                    strand,
                    UnicodeIter::new(
                        PinStr::new(this.to_strong()),
                        if matches!(method.tag(), sym::SCALARS | sym::SCALAR_BOUNDS) {
                            UnicodeKind::Scalar
                        } else {
                            UnicodeKind::Grapheme
                        },
                        matches!(method.tag(), sym::SCALAR_BOUNDS | sym::GRAPHEME_BOUNDS),
                    ),
                    out,
                );
                Ok(())
            }
            sym::SCALAR => {
                unpack!(strand, args =>);
                let mut scalars = me.chars();
                let scalar = scalars.next().ok_or_else(|| {
                    Error::value(strand, "str.scalar: expected exactly one Unicode scalar")
                })?;
                if scalars.next().is_some() {
                    return Err(Error::value(
                        strand,
                        "str.scalar: expected exactly one Unicode scalar",
                    ));
                }
                Output::set(strand, out, scalar as u32 as usize);
                Ok(())
            }
            sym::STARTS_WITH => {
                unpack!(strand, args => prefix);
                let prefix = prefix
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "str.starts_with: not a string"))?;
                let input = with_str!(strand, prefix, |prefix| this.str().starts_with(prefix));
                Output::set(strand, out, input);
                Ok(())
            }
            sym::WITHOUT_PREFIX => {
                unpack!(strand, args => prefix);
                let borrow = this.str();
                let prefix = prefix
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "str.without_prefix: not a string"))?;
                with_str!(strand, prefix, |prefix| {
                    Output::set(strand, out, borrow.strip_prefix(prefix).unwrap_or(borrow))
                });
                Ok(())
            }
            sym::ENDS_WITH => {
                unpack!(strand, args => suffix);
                let suffix = suffix
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "str.ends_with: not a string"))?;
                let input = with_str!(strand, suffix, |suffix| this.str().ends_with(suffix));
                Output::set(strand, out, input);
                Ok(())
            }
            sym::WITHOUT_SUFFIX => {
                unpack!(strand, args => suffix);
                let borrow = this.str();
                let suffix = suffix
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "str.without_suffix: not a string"))?;
                with_str!(strand, suffix, |suffix| {
                    Output::set(strand, out, borrow.strip_suffix(suffix).unwrap_or(borrow))
                });
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
                    .downcast_ref(strand.builtin_types().str)
                    .ok_or_else(|| {
                        Error::type_error(strand, "str.split/rsplit: delimiter not a string")
                    })?;
                // SAFETY: the pin owns its reference as a root
                let delim_gc = PinStr::new(unsafe { delim_gc.dup() });
                let forward = method_sym == sym::SPLIT;
                let state = match limit_i64 {
                    Some(l) if l < 0 => {
                        // Negative limit: split from the opposite end, buffer in yield order.
                        let n = (l.unsigned_abs() as usize).saturating_add(1);
                        let src: &str = this.str();
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
                                offset: Some(this.str().len()),
                                limit,
                                reverse: true,
                            }
                        }
                    }
                };
                strand.builtin_types().str_split.create(
                    strand,
                    Split {
                        str: PinStr::new(this.to_strong()),
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
                    let mut acc = StrEmbryo::new();
                    if input.next(strand, &mut value).await? {
                        value.op_display(strand, &mut acc)?;
                    }
                    while input.next(strand, &mut value).await? {
                        acc.extend(strand, this.str());
                        value.op_display(strand, &mut acc)?;
                        strand.check_trap_gc()?;
                    }
                    acc.finish(strand, out);
                    Ok(())
                })
            }
            sym::CHOMP => {
                unpack!(strand, args =>);
                let end = me.len() - iter::line_terminator_len(me.as_bytes());
                Output::set(strand, out, &me[..end]);
                Ok(())
            }
            sym::TRIM => {
                unpack!(strand, args => ?chars);
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
                unpack!(strand, args => ?chars);
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
                unpack!(strand, args => ?chars);
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
            sym::UPPER => {
                Output::set(strand, out, me.to_uppercase().as_str());
                Ok(())
            }
            sym::LOWER => {
                Output::set(strand, out, me.to_lowercase().as_str());
                Ok(())
            }
            sym::FIND | sym::RFIND => {
                let range = Sym::well_known(sym::RANGE);
                unpack!(strand, args => needle, ?:range);
                let needle = needle
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "not a string"))?;
                let (start, end) = range::window(strand, range.as_deref(), me.len())?;
                let window = me
                    .get(start..end)
                    .ok_or_else(|| Error::runtime(strand, "invalid UTF-8 substring boundaries"))?;
                let found = with_str!(strand, needle, |needle| {
                    if method.tag() == sym::FIND {
                        window.find(needle)
                    } else {
                        window.rfind(needle)
                    }
                });
                match found {
                    Some(offset) => Output::set(strand, out, start + offset),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            }
            sym::REPLACE => {
                let range = Sym::well_known(sym::RANGE);
                unpack!(strand, args => from, to, ?:range);
                let from = from
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "old value is not a string"))?;
                let to = to
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "new value is not a string"))?;
                let (start, end) = range::window(strand, range.as_deref(), me.len())?;
                let window = me
                    .get(start..end)
                    .ok_or_else(|| Error::runtime(strand, "invalid UTF-8 substring boundaries"))?;
                with_str!(strand, from, |from| {
                    with_str!(strand, to, |to| {
                        let mut acc = StrEmbryo::new_with_capacity(strand, me.len());
                        acc.extend(strand, &me[..start]);
                        let mut last = 0;
                        for (at, _) in window.match_indices(from) {
                            acc.extend(strand, &window[last..at]);
                            acc.extend(strand, to);
                            last = at + from.len();
                        }
                        acc.extend(strand, &window[last..]);
                        acc.extend(strand, &me[end..]);
                        acc.finish(strand, out);
                    })
                });
                Ok(())
            }
            sym::REPEAT => {
                unpack!(strand, args => count);
                let count = count.to_index(strand)?;
                let len = me
                    .len()
                    .checked_mul(count)
                    .ok_or_else(|| Error::overflow(strand))?;
                if len > isize::MAX as usize {
                    return Err(Error::overflow(strand));
                }
                let mut acc = StrEmbryo::new_with_capacity(strand, len);
                for _ in 0..count {
                    acc.extend(strand, me);
                }
                acc.finish(strand, out);
                Ok(())
            }
            sym::CONTAINS => {
                unpack!(strand, args => needle);
                let needle = needle
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "not a string"))?;
                let input = with_str!(strand, needle, |needle| this.str().contains(needle));
                Output::set(strand, out, input);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "Str.len is a field, not a method",
            )),
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
                Output::set(strand, out, this.str().len());
                Ok(())
            }
            sym::STARTS_WITH
            | sym::SCALARS
            | sym::SCALAR_BOUNDS
            | sym::GRAPHEME_BOUNDS
            | sym::SCALAR
            | sym::GRAPHEMES
            | sym::WITHOUT_PREFIX
            | sym::ENDS_WITH
            | sym::WITHOUT_SUFFIX
            | sym::SPLIT
            | sym::RSPLIT
            | sym::UPPER
            | sym::LOWER
            | sym::JOIN
            | sym::CHOMP
            | sym::TRIM
            | sym::TRIM_START
            | sym::TRIM_END
            | sym::REPLACE
            | sym::REPEAT
            | sym::FIND
            | sym::RFIND
            | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UnicodeKind {
    Scalar,
    Grapheme,
}

#[derive(Clone)]
enum UnicodeIterState {
    Scalar(Chars<'static>),
    Grapheme(Graphemes<'static>),
}

pub(crate) struct UnicodeIter<'v> {
    // Drop order is significant: the borrowing iterator, then the pin.
    state: UnicodeIterState,
    boundaries: bool,
    offset: usize,
    terminal_emitted: bool,
    str: PinStr<'v>,
}

impl<'v> UnicodeIter<'v> {
    fn new(str: PinStr<'v>, kind: UnicodeKind, boundaries: bool) -> Self {
        // SAFETY: the pin is stored below and is dropped only after `state`, the
        // sole user of this reference.
        let pinned = unsafe { mem::transmute::<&str, &'static str>(&str) };
        let state = match kind {
            UnicodeKind::Scalar => UnicodeIterState::Scalar(pinned.chars()),
            UnicodeKind::Grapheme => UnicodeIterState::Grapheme(pinned.graphemes(true)),
        };
        Self {
            state,
            boundaries,
            offset: 0,
            terminal_emitted: false,
            str,
        }
    }

    fn snapshot(&self) -> Self {
        let kind = match self.state {
            UnicodeIterState::Scalar(_) => UnicodeKind::Scalar,
            UnicodeIterState::Grapheme(_) => UnicodeKind::Grapheme,
        };
        let mut snapshot = Self::new(self.str.clone(), kind, self.boundaries);
        snapshot.state = self.state.clone();
        snapshot.offset = self.offset;
        snapshot.terminal_emitted = self.terminal_emitted;
        snapshot
    }

    fn next_value(&mut self, strand: &mut Strand<'v, '_>, out: Slot<'v, '_>) -> bool {
        let boundaries = self.boundaries;
        let offset = self.offset;
        if let Some(fragment) = self.next_str() {
            let len = fragment.len();
            if boundaries {
                Output::set(strand, out, offset);
            } else {
                Output::set(strand, out, fragment);
            }
            self.offset += len;
            true
        } else if boundaries && !self.terminal_emitted {
            self.terminal_emitted = true;
            Output::set(strand, out, offset);
            true
        } else {
            false
        }
    }

    fn next_str(&mut self) -> Option<&str> {
        match &mut self.state {
            UnicodeIterState::Scalar(chars) => chars.as_str().chars().next().map(|ch| {
                let len = ch.len_utf8();
                let value = &chars.as_str()[..len];
                chars.next();
                value
            }),
            UnicodeIterState::Grapheme(graphemes) => graphemes.next(),
        }
    }
}

unsafe impl<'v> Collect for UnicodeIter<'v> {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.str.obj().accept(visit)
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

fn fill_unpack<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sig: &sig::Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    mut next: impl FnMut(&mut Strand<'v, 's>, Slot<'v, '_>) -> bool,
) -> Result<'v, 's, ()> {
    for i in 0..sig.required {
        if !next(strand, out.at(i)) {
            return Err(Error::missing_positional(strand, sig.required));
        }
    }
    for (i, default) in sig.optional.iter().enumerate() {
        if !next(strand, out.at(sig.required + i)) {
            out.at(sig.required + i).store(default.dup());
        }
    }
    let pos_count = sig.required + sig.optional.len();
    for (i, key) in sig.keys.iter().enumerate() {
        if let Some(default) = &key.default {
            out.at(pos_count + i).store(default.dup());
        } else {
            return Err(match &key.kind {
                sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                sig::UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
            });
        }
    }
    if sig.pos_rest() == Rest::None && with_slots!(strand, |extra| next(strand, extra)) {
        return Err(Error::unexpected_positional(strand, pos_count));
    }
    Ok(())
}

impl<'v> Protocol<'v> for UnicodeIter<'v> {
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: crate::object::protocol::SpreadContext,
        sink: &'a mut dyn crate::object::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<str unicode iter>")
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
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
        let mut iter = this.borrow_mut(strand)?;
        Ok(iter.next_value(strand, out))
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

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut iter = this.borrow(strand)?.snapshot();
        fill_unpack(strand, sig, &mut out, |strand, out| {
            iter.next_value(strand, out)
        })?;
        if let Some(i) = sig.pos_rest_slot() {
            strand
                .builtin_types()
                .str_unicode_iter
                .create(strand, iter, out.at(i));
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
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
    /// Pre-computed segments (byte ranges into `Str`) stored in yield order.
    /// Used when split direction differs from yield direction.
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
    str: PinStr<'v>,
    delim: PinStr<'v>,
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let forward = this.borrow_mut(strand)?.forward;
        let label = if forward {
            "<str split>"
        } else {
            "<str rsplit>"
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
            Output::set(strand, out.at(i), &borrow.str[start..end]);
        }

        // Fill optional positional slots
        for i in 0..sig.optional.len() {
            if let Some((start, end)) = borrow.next_segment() {
                Output::set(strand, out.at(sig.required + i), &borrow.str[start..end]);
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
                .str_split
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

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
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
        crate::fmt!(strand, w, "<type std.Str>")
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => value);
        if let Some(value) = value.as_str(strand) {
            with_str!(strand, value, |value| Output::set(strand, out, value));
            Ok(())
        } else if let Some(value) = value.as_bin(strand) {
            with_slice!(strand, value, |value| {
                let value = std::str::from_utf8(value)
                    .map_err(|_| Error::data(strand, "Str: invalid UTF-8"))?;
                Output::set(strand, out, value);
                Ok(())
            })
        } else {
            Err(Error::type_error(strand, "Str: expected Str or Bin"))
        }
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
                Method(sym::SCALARS),
                Method(sym::SCALAR_BOUNDS),
                Method(sym::GRAPHEME_BOUNDS),
                Method(sym::SCALAR),
                Method(sym::GRAPHEMES),
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
                Method(sym::UPPER),
                Method(sym::LOWER),
                Method(sym::REPLACE),
                Method(sym::REPEAT),
                Method(sym::FIND),
                Method(sym::RFIND),
                Method(sym::CONTAINS),
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
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::BOOL_METHOD
            | sym::HASH_METHOD
            | sym::LEN
            | sym::SCALARS
            | sym::SCALAR_BOUNDS
            | sym::GRAPHEME_BOUNDS
            | sym::SCALAR
            | sym::GRAPHEMES
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
            | sym::UPPER
            | sym::LOWER
            | sym::REPLACE
            | sym::REPEAT
            | sym::FIND
            | sym::RFIND
            | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
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
                unpack!(strand, args => self_val, value);
                if let Some(value) = value.as_str(strand) {
                    with_str!(strand, value, |value| Output::set(strand, &mut out, value));
                } else if let Some(value) = value.as_bin(strand) {
                    with_slice!(strand, value, |value| {
                        let value = std::str::from_utf8(value)
                            .map_err(|_| Error::data(strand, "Str: invalid UTF-8"))?;
                        Output::set(strand, &mut out, value);
                        Ok(())
                    })?;
                } else {
                    return Err(Error::type_error(strand, "Str: expected Str or Bin"));
                }
                let native = out.take();
                self_val.op_fill(strand, &strand.singletons().str, native)?;
                Ok(())
            }
            _ => {
                let vm = strand.vm();
                type_mcall_fallback(strand, &vm.singletons().str, method, args, out).await
            }
        }
    }
}
