use std::{
    cell::Cell,
    hash::{DefaultHasher, Hash},
    mem,
    ops::ControlFlow,
    str::Chars,
};

use crate::value::fmt::Format;

use unicode_segmentation::{Graphemes, UnicodeSegmentation};

use crate::{
    arg::Args,
    bytecode::Variadic,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::protocol::{GcObj, members},
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{self, Output, Slot, Slots, StrEmbryo, Value, view::PinStr},
    vm::Vm,
};

use super::{
    BoundMethod, index, iter,
    protocol::{
        Inspect, Protocol, Recv, Spread, SpreadContext, instance_mcall_fallback, is_special_mcall,
        type_mcall_fallback,
    },
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

    strand
        .with_slots(async move |strand, [mut input, mut elem]| {
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
        .await
}

impl<'v> Protocol<'v> for str {
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
        crate::fmt!(strand, w, "{}", this.receiver.get())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "\"")?;
        for char in this.receiver.get().chars() {
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.get();

        match method.tag() {
            sym::SCALARS | sym::GRAPHEMES => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                strand.builtin_types().str_view.create(
                    strand,
                    View {
                        str: this.to_strong(),
                        kind: if method.tag() == sym::SCALARS {
                            ViewKind::Scalar
                        } else {
                            ViewKind::Grapheme
                        },
                        len: Cell::new(None),
                    },
                    out,
                );
                Ok(())
            }
            sym::SCALAR => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
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
                        l.to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "limit: expected `Int`"))
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
                strand.builtin_types().str_split.create(
                    strand,
                    Split {
                        str: this.to_strong(),
                        delim: delim_gc,
                        state,
                        forward,
                    },
                    out,
                );
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
                        let mut acc = StrEmbryo::new();
                        if input.next(strand, &mut value).await? {
                            value.op_display(strand, &mut acc)?;
                        }
                        while input.next(strand, &mut value).await? {
                            acc.extend(strand, this.receiver.get());
                            value.op_display(strand, &mut acc)?;
                            strand.check_trap_gc()?;
                        }
                        acc.finish(strand, out);
                        Ok(())
                    })
                    .await
            }
            sym::CHOMP => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let end = me.len() - iter::line_terminator_len(me.as_bytes());
                Output::set(strand, out, &me[..end]);
                Ok(())
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
                "Str.len is a field, not a method",
            )),
            _ if is_special_mcall(method.tag()) => {
                instance_mcall_fallback(strand, &this, method, args, out)
                    .await
                    .expect("supported special method")
            }
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
                Output::set(strand, out, this.receiver.get().len());
                Ok(())
            }
            sym::STARTS_WITH
            | sym::SCALARS
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
            | sym::REPEAT
            | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ViewKind {
    Scalar,
    Grapheme,
}

pub(crate) struct View<'v> {
    str: GcObj<'v, str>,
    kind: ViewKind,
    len: Cell<Option<usize>>,
}

impl View<'_> {
    fn len(&self) -> usize {
        if let Some(len) = self.len.get() {
            return len;
        }
        let len = match self.kind {
            ViewKind::Scalar => self.str.chars().count(),
            ViewKind::Grapheme => self.str.graphemes(true).count(),
        };
        self.len.set(Some(len));
        len
    }

    fn byte_at(&self, position: usize) -> Option<usize> {
        match self.kind {
            ViewKind::Scalar => self
                .str
                .char_indices()
                .nth(position)
                .map(|(index, _)| index),
            ViewKind::Grapheme => self
                .str
                .grapheme_indices(true)
                .nth(position)
                .map(|(index, _)| index),
        }
    }

    fn bounds(&self, index: usize) -> Option<(usize, usize)> {
        let start = self.byte_at(index)?;
        let end = self.byte_at(index + 1).unwrap_or(self.str.len());
        Some((start, end))
    }

    fn position(&self, index: usize, len: usize) -> Option<usize> {
        if index == len {
            Some(self.str.len())
        } else {
            self.byte_at(index)
        }
    }
}

unsafe impl<'v> Collect for View<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.str.accept(visit)
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

enum ViewIterState {
    Scalar(Chars<'static>),
    Grapheme(Graphemes<'static>),
}

pub(crate) struct ViewIter<'v> {
    // Drop order is significant: borrowed iterator, pin, then GC root.
    state: ViewIterState,
    _pin: PinStr<'v, 'static>,
    str: GcObj<'v, str>,
}

impl<'v> ViewIter<'v> {
    fn new(view: &View<'v>, strand: &Strand<'v, '_>) -> Self {
        let value = Value::from_object(view.str.clone());
        let pin = value.as_str(strand.vm()).unwrap().pin();
        // SAFETY: `str` roots the object and field order drops `state` before
        // `_pin`, then drops the pin before the root.
        let pin = unsafe { pin.into_static_unchecked() };
        // SAFETY: the widened pin is stored below and is dropped only after
        // `state`, the sole user of this reference.
        let pinned = unsafe { mem::transmute::<&str, &'static str>(&pin) };
        let state = match view.kind {
            ViewKind::Scalar => ViewIterState::Scalar(pinned.chars()),
            ViewKind::Grapheme => ViewIterState::Grapheme(pinned.graphemes(true)),
        };
        Self {
            state,
            _pin: pin,
            str: view.str.clone(),
        }
    }

    fn next_str(&mut self) -> Option<&str> {
        match &mut self.state {
            ViewIterState::Scalar(chars) => chars.as_str().chars().next().map(|ch| {
                let len = ch.len_utf8();
                let value = &chars.as_str()[..len];
                chars.next();
                value
            }),
            ViewIterState::Grapheme(graphemes) => graphemes.next(),
        }
    }
}

unsafe impl<'v> Collect for ViewIter<'v> {
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

fn fill_unpack<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sig: &sig::Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    mut next: impl FnMut() -> Option<String>,
) -> Result<'v, 's, ()> {
    for i in 0..sig.required {
        let value = next().ok_or_else(|| Error::missing_positional(strand, sig.required))?;
        out.at(i).store(Value::from_str(strand, &value));
    }
    for (i, default) in sig.optional.iter().enumerate() {
        if let Some(value) = next() {
            out.at(sig.required + i)
                .store(Value::from_str(strand, &value));
        } else {
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
    if sig.variadic == Variadic::None && next().is_some() {
        return Err(Error::unexpected_positional(strand, pos_count));
    }
    Ok(())
}

impl<'v> Protocol<'v> for View<'v> {
    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let label = match this.get().kind {
            ViewKind::Scalar => "scalars",
            ViewKind::Grapheme => "graphemes",
        };
        crate::fmt!(strand, w, "<str {label}>")
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().iterable)
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index_value: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let view = this.get();
        let len = view.len();
        if let Some((start, end)) = range::slice_bounds(index_value, strand, len)? {
            let start = view
                .position(start, len)
                .ok_or_else(|| Error::index(strand))?;
            let end = view
                .position(end, len)
                .ok_or_else(|| Error::index(strand))?;
            Output::set(strand, out, &view.str[start..end]);
            return Ok(());
        }
        let index = index_value
            .to_i64(strand)
            .map_err(|_| Error::index(strand))?;
        let index = index::element(len, index).ok_or_else(|| Error::index(strand))?;
        let (start, end) = view.bounds(index).ok_or_else(|| Error::index(strand))?;
        Output::set(strand, out, &view.str[start..end]);
        Ok(())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::LEN {
            Output::set(strand, out, this.get().len());
            Ok(())
        } else {
            iter::iterable_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if method.tag() == sym::LEN {
            Err(Error::type_error(
                strand,
                "string view len is a field, not a method",
            ))
        } else {
            iter::iterable_mcall(strand, &this, method, args, out).await
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand
            .builtin_types()
            .str_view_iter
            .create(strand, ViewIter::new(this.get(), strand), out);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut iter = ViewIter::new(this.get(), strand);
        while let Some(value) = iter.next_str() {
            let mut slot = Value::from_str(strand, value);
            sink.positional(strand, Slot::new(&mut slot))?;
        }
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut iter = ViewIter::new(this.get(), strand);
        fill_unpack(strand, sig, &mut out, || iter.next_str().map(str::to_owned))?;
        if sig.variadic == Variadic::Capture {
            strand
                .builtin_types()
                .str_view_iter
                .create(strand, iter, out.at(sig.len() - 1));
        }
        Ok(())
    }
}

impl<'v> Protocol<'v> for ViewIter<'v> {
    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<str view iter>")
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
        let Some(value) = iter.next_str() else {
            return Ok(false);
        };
        Output::set(strand, out, value);
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

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut iter = this.borrow_mut(strand)?;
        fill_unpack(strand, sig, &mut out, || iter.next_str().map(str::to_owned))?;
        drop(iter);
        if sig.variadic == Variadic::Capture {
            Output::set(strand, out.at(sig.len() - 1), &this);
        }
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
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        if let Some(value) = value.as_str_raw(strand) {
            Output::set(strand, out, value);
            Ok(())
        } else if let Some(value) = value.as_bin_raw(strand) {
            let value = std::str::from_utf8(value)
                .map_err(|_| Error::value(strand, "Str: invalid UTF-8"))?;
            Output::set(strand, out, value);
            Ok(())
        } else {
            Err(Error::type_error(strand, "Str: expected Str or Bin"))
        }
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
                Method(sym::SCALARS),
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
                Method(sym::REPEAT),
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([self_val, value], []) = unpack!(strand, args, 2, 0)?;
                if let Some(value) = value.as_str_raw(strand) {
                    Output::set(strand, &mut out, value);
                } else if let Some(value) = value.as_bin_raw(strand) {
                    let value = std::str::from_utf8(value)
                        .map_err(|_| Error::value(strand, "Str: invalid UTF-8"))?;
                    Output::set(strand, &mut out, value);
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
