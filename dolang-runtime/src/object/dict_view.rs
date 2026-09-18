//! Lazy dictionary-like projections over native objects.

use std::{
    hash::{DefaultHasher, Hasher},
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use bitvec::{bitbox, boxed::BitBox};
use dolang_bytecode::{Rest, Variadic};

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod, dict,
        native::{Instance, Object, Spread, SpreadContext, Type as NativeType, Unpack, UnpackItem},
        protocol::{GcObj, Inspect, Protocol, Recv, members},
    },
    sig,
    strand::Strand,
    sym,
    sym::Sym,
    unpack,
    value::{
        AsTuple, Empty, Input, InputBy, Output, Slot, Slots, TypeObject, Value, private::Sealed,
    },
    vm::Vm,
};

/// Receives ordered key/value pairs while a dictionary view is flattened.
pub struct DictViewSink<'v, 'a> {
    pairs: &'a mut Vec<(Value<'v>, Value<'v>)>,
}

impl<'v> DictViewSink<'v, '_> {
    /// Appends one key/value pair to the flattened snapshot.
    pub fn push(
        &mut self,
        strand: &mut Strand<'v, '_>,
        key: impl Input<'v>,
        value: impl Input<'v>,
    ) {
        let key = Value::from_input(strand, key);
        let value = Value::from_input(strand, value);
        self.pairs.push((key, value));
    }
}

/// Implements a lazy dictionary-like projection over a native object.
///
/// Implement this trait on a marker type. Different marker types may expose
/// different views of the same [`Object`]. Methods take `&self` (rather than
/// being purely associated functions on a zero-sized marker) so a view can
/// be parametrized by a runtime value if needed.
pub trait DictLike<'v>: 'v {
    type Object: Object<'v>;

    const MODULE: &'v str;
    const NAME: &'v str;

    fn len(&self, this: Instance<'v, '_, Self::Object>, strand: &mut Strand<'v, '_>) -> usize;

    /// Writes the value for `key` to `out` and returns whether it was
    /// present. When multiple values exist for the same key, `instance`
    /// selects which one, using the same negative-from-the-end indexing
    /// convention as [`Dict::get`]'s own `instance` parameter — `-1` (the
    /// default for plain `[]` indexing) selects the last (most-recently-seen)
    /// value. Implementations that never have more than one value per key
    /// only need to handle `0`/`-1` and can return `Ok(false)` for anything
    /// else.
    ///
    /// [`Dict::get`]: crate::value::view::Dict::get
    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool>;

    fn set<'a, 's>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        _key: Slot<'v, 'a>,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    fn flatten<'s>(
        &self,
        this: Instance<'v, '_, Self::Object>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()>;

    /// Writes an array of every distinct key (first-seen order, no
    /// duplicates) to `out`. Default implementation flattens and
    /// deduplicates — override if there's a cheaper way to enumerate unique
    /// keys for this particular projection.
    fn keys<'s>(
        &self,
        this: Instance<'v, '_, Self::Object>,
        strand: &mut Strand<'v, 's>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let expected = self.len(this, strand);
        let mut pairs = Vec::with_capacity(expected);
        self.flatten(this, strand, &mut DictViewSink { pairs: &mut pairs })?;
        Output::set(strand, &mut out, Empty::Array);
        let array = out.as_array(strand).unwrap();
        let mut seen: Vec<Value<'v>> = Vec::with_capacity(pairs.len());
        for (key, _) in pairs {
            if !seen
                .iter()
                .any(|seen| seen.op_eq(strand, &key).to_bool(strand))
            {
                array.push(strand, &key)?;
                seen.push(key);
            }
        }
        Ok(())
    }

    /// Writes an array of every value stored for `key`, in insertion order,
    /// to `out`. Default implementation never flattens: it just calls
    /// [`get`](Self::get) at increasing non-negative instances (`0`, `1`,
    /// `2`, ...) until one comes back not-found.
    fn values<'s>(
        &self,
        this: Instance<'v, '_, Self::Object>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, &mut out, Empty::Array);
        let array = out.as_array(strand).unwrap();
        let mut instance = 0i64;
        loop {
            let mut tmp = Value::NIL;
            if !self.get(this, strand, key, instance, Slot::new(&mut tmp))? {
                break;
            }
            array.push(strand, &tmp)?;
            instance += 1;
        }
        Ok(())
    }
}

/// Input wrapper that creates a dictionary view of a native object.
pub struct DictView<'v, 'a, I: DictLike<'v>> {
    owner: Instance<'v, 'a, I::Object>,
    // `Option` so `input_take` can steal `view` out rather than requiring
    // `I: Clone`. Using a `DictView` more than once (it's meant to be
    // constructed and immediately handed to `Output::set`/`Value::from_input`)
    // is a programming error, not something to recover from.
    view: Option<I>,
}

impl<'v, 'a, I: DictLike<'v>> DictView<'v, 'a, I> {
    pub fn new(owner: Instance<'v, 'a, I::Object>, view: I) -> Self {
        Self {
            owner,
            view: Some(view),
        }
    }

    /// Implements [`Object::iter`] directly without constructing a view object.
    pub fn iter<'s>(
        owner: Instance<'v, '_, I::Object>,
        view: &I,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten(view, owner, strand)?;
        strand
            .builtin_types()
            .dict_view_iter
            .create(strand, Iter { pairs, index: 0 }, out);
        Ok(())
    }

    /// Implements [`Object::spread`] directly without constructing a view object.
    pub fn spread<'s>(
        owner: Instance<'v, '_, I::Object>,
        view: &I,
        strand: &mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten(view, owner, strand)?;
        spread_pairs(strand, &pairs, context, sink)
    }

    /// Implements [`Object::unpack`] directly without constructing a view object.
    pub fn unpack<'s>(
        owner: Instance<'v, '_, I::Object>,
        view: &I,
        strand: &mut Strand<'v, 's>,
        unpack: Unpack<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten(view, owner, strand)?;
        unpack_pairs(strand, pairs, unpack)
    }
}

impl<'v, I: DictLike<'v>> Input<'v> for DictView<'v, '_, I> {
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: Sealed) -> InputBy<'v, 'a> {
        let ty = self.owner.ty(vm);
        let owner = Value::from_input(vm, self.owner);
        let view = self.view.take().expect("DictView used more than once");
        let value = GcObj::new(
            vm.arena(),
            vm.builtin_types().dict_view,
            View {
                owner,
                glue: Box::new(Glue { view, ty }),
            },
        );
        InputBy::Value(Value::from_object(value), None)
    }
}

trait DictViewGlue<'v>: 'v {
    fn module(&self) -> &'v str;
    fn name(&self) -> &'v str;
    fn len(&self, owner: &Value<'v>, strand: &mut Strand<'v, '_>) -> usize;
    fn get<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool>;
    fn set<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;
    fn flatten<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        pairs: &mut Vec<(Value<'v>, Value<'v>)>,
    ) -> Result<'v, 's, ()>;
    fn keys<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;
    fn values<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;
}

struct Glue<'v, I: DictLike<'v>> {
    view: I,
    ty: NativeType<'v, I::Object>,
}

impl<'v, I: DictLike<'v>> DictViewGlue<'v> for Glue<'v, I> {
    fn module(&self) -> &'v str {
        I::MODULE
    }
    fn name(&self) -> &'v str {
        I::NAME
    }
    fn len(&self, owner: &Value<'v>, strand: &mut Strand<'v, '_>) -> usize {
        self.view
            .len(Instance::from_native_value(owner, strand, self.ty), strand)
    }
    fn get<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        self.view.get(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            key,
            instance,
            out,
        )
    }
    fn set<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        self.view.set(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            key,
            value,
        )
    }
    fn flatten<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        pairs: &mut Vec<(Value<'v>, Value<'v>)>,
    ) -> Result<'v, 's, ()> {
        self.view.flatten(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            &mut DictViewSink { pairs },
        )
    }
    fn keys<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.view.keys(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            out,
        )
    }
    fn values<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.view.values(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            key,
            out,
        )
    }
}

pub(crate) struct View<'v> {
    owner: Value<'v>,
    glue: Box<dyn DictViewGlue<'v> + 'v>,
}

pub(crate) struct Iter<'v> {
    pairs: Vec<(Value<'v>, Value<'v>)>,
    index: usize,
}

unsafe impl<'v> Collect for View<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.owner.accept(visit)
    }
    fn clear(&mut self) {
        self.owner.clear()
    }
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (key, value) in &self.pairs {
            key.accept(visit)?;
            value.accept(visit)?;
        }
        ControlFlow::Continue(())
    }
    fn clear(&mut self) {
        self.pairs.clear()
    }
}

fn flatten<'v, 's, I: DictLike<'v>>(
    view: &I,
    owner: Instance<'v, '_, I::Object>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, Vec<(Value<'v>, Value<'v>)>> {
    let expected = view.len(owner, strand);
    let mut pairs = Vec::with_capacity(expected);
    view.flatten(owner, strand, &mut DictViewSink { pairs: &mut pairs })?;
    if pairs.len() != expected {
        return Err(Error::runtime(
            strand,
            "dictionary view length changed while flattening",
        ));
    }
    Ok(pairs)
}

fn flatten_glue<'v, 's>(
    view: &View<'v>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, Vec<(Value<'v>, Value<'v>)>> {
    let expected = view.glue.len(&view.owner, strand);
    let mut pairs = Vec::with_capacity(expected);
    view.glue.flatten(&view.owner, strand, &mut pairs)?;
    if pairs.len() != expected {
        return Err(Error::runtime(
            strand,
            "dictionary view length changed while flattening",
        ));
    }
    Ok(pairs)
}

fn snapshot_dict<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: Vec<(Value<'v>, Value<'v>)>,
) -> Result<'v, 's, Value<'v>> {
    let mut snapshot = dict::Dict::new();
    for (key, value) in pairs {
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher)?;
        let hash = hasher.finish();
        snapshot.insert(strand, key, value, hash, false);
    }
    Ok(Value::from_object(GcObj::new(
        strand.arena(),
        strand.builtin_types().dict,
        snapshot,
    )))
}

fn spread_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: &[(Value<'v>, Value<'v>)],
    context: SpreadContext,
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    // The pairs stay borrowed from wherever they're rooted, so each one is
    // copied into scratch slots (never bare stack `Value`s) for the duration
    // of the sink call.
    strand.with_slots_sync(|strand, [mut first, mut second]| {
        for (key, value) in pairs {
            if context == SpreadContext::Pairs {
                Output::set(strand, &mut first, key);
                Output::set(strand, &mut second, value);
                sink.keyed(
                    strand,
                    Slot::reborrow(&mut first),
                    Slot::reborrow(&mut second),
                )?;
            } else {
                // The pair tuple only needs one slot; `second` goes unused.
                Output::set(strand, &mut first, AsTuple::new([key, value]));
                sink.positional(strand, Slot::reborrow(&mut first))?;
            }
        }
        Ok(())
    })
}

fn unpack_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: Vec<(Value<'v>, Value<'v>)>,
    mut unpack: Unpack<'v, '_>,
) -> Result<'v, 's, ()> {
    let (pos_rest, key_rest, mixed) = (unpack.pos_rest(), unpack.key_rest(), unpack.mixed_rest());
    let mut consumed = bitbox![0; pairs.len()];
    let mut position = 0i64;
    let mut rests = Vec::new();
    for item in unpack.iter() {
        let (key, slot, default) = match item {
            UnpackItem::Pos { slot, default } => {
                let key = Value::from_i64(strand, position);
                position = position
                    .checked_add(1)
                    .ok_or_else(|| Error::overflow(strand))?;
                (key, slot, default)
            }
            UnpackItem::SymKey { key, slot, default } => {
                (Value::from_input(strand, key.as_str(strand)), slot, default)
            }
            UnpackItem::ConstKey { key, slot, default } => (key.dup(), slot, default),
            rest => {
                rests.push(rest);
                continue;
            }
        };

        let found = pairs
            .iter()
            .enumerate()
            .find_map(|(index, (candidate, value))| {
                if !consumed[index] && candidate.op_eq(strand, &key).to_bool(strand) {
                    Some((index, value))
                } else {
                    None
                }
            });
        if let Some((index, value)) = found {
            consumed.set(index, true);
            Output::set(strand, slot, value);
        } else if let Some(default) = default {
            Output::set(strand, slot, default);
        } else if let Some(sym) = key.as_sym(strand) {
            return Err(Error::missing_key(strand, sym));
        } else if position != 0 && key.to_i64(strand).is_ok() {
            return Err(Error::missing_positional(strand, position as usize - 1));
        } else {
            return Err(Error::missing_key(strand, &key));
        }
    }
    let start = usize::try_from(position).map_err(|_| Error::overflow(strand))?;
    let run = settle_leftovers(
        strand,
        &pairs,
        &mut consumed,
        start,
        pos_rest,
        key_rest,
        mixed,
    )?;
    for rest in rests {
        match rest {
            UnpackItem::Rest { slot } | UnpackItem::KeyRest { slot } => {
                let pairs = unconsumed(&pairs, &consumed);
                strand.builtin_types().dict_view_iter.create(
                    strand,
                    Iter { pairs, index: 0 },
                    slot,
                );
            }
            UnpackItem::PosRest { slot } => {
                let tuple = run_tuple(strand, &pairs, &run);
                Output::set(strand, slot, &tuple);
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

/// Settles the pairs an unpack left over.
///
/// Positional items are those with the integer keys counting up from
/// `start`. This checks for leftovers of a kind that no rest takes and, unless
/// a `...` rest takes both kinds in order, marks consumed the positional
/// items that a `*` rest takes. A `**` rest alone takes them as keyed items.
///
/// Returns the indices of the positional items a `*` rest takes.
fn settle_leftovers<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: &[(Value<'v>, Value<'v>)],
    consumed: &mut BitBox,
    start: usize,
    pos_rest: Rest,
    key_rest: Rest,
    mixed: bool,
) -> Result<'v, 's, Vec<usize>> {
    if mixed {
        return Ok(Vec::new());
    }
    let mut run = Vec::new();
    let mut next = start as i128;
    while let Some(index) = (0..pairs.len())
        .find(|&index| !consumed[index] && pairs[index].0.as_int(strand) == Some(next))
    {
        run.push(index);
        next += 1;
    }
    if pos_rest == Rest::None {
        if key_rest == Rest::None && !run.is_empty() {
            return Err(Error::unexpected_positional(strand, start));
        }
        run.clear();
    }
    for &index in &run {
        consumed.set(index, true);
    }
    if key_rest == Rest::None
        && let Some(index) = consumed.first_zero()
    {
        return Err(Error::unexpected_key(strand, &pairs[index].0));
    }
    Ok(run)
}

fn settle_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sig: &sig::Unpack<'v, '_>,
    pairs: &[(Value<'v>, Value<'v>)],
    consumed: &mut BitBox,
) -> Result<'v, 's, Vec<usize>> {
    let mixed = matches!(sig.variadic, Variadic::Discard | Variadic::Capture);
    let start = sig.required + sig.optional.len();
    settle_leftovers(
        strand,
        pairs,
        consumed,
        start,
        sig.pos_rest(),
        sig.key_rest(),
        mixed,
    )
}

fn unconsumed<'v>(
    pairs: &[(Value<'v>, Value<'v>)],
    consumed: &BitBox,
) -> Vec<(Value<'v>, Value<'v>)> {
    pairs
        .iter()
        .zip(consumed.iter().by_vals())
        .filter(|(_, consumed)| !*consumed)
        .map(|((key, value), _)| (key.dup(), value.dup()))
        .collect()
}

fn run_tuple<'v>(
    strand: &mut Strand<'v, '_>,
    pairs: &[(Value<'v>, Value<'v>)],
    run: &[usize],
) -> Value<'v> {
    let values: Vec<_> = run.iter().map(|&index| pairs[index].1.dup()).collect();
    Value::from_object(super::tuple::tuple(strand, values))
}

/// Stores a `*name` rest's tuple and a `**name` rest's iterator.
fn store_split_rests<'v>(
    strand: &mut Strand<'v, '_>,
    sig: &sig::Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    pairs: &[(Value<'v>, Value<'v>)],
    consumed: &BitBox,
    run: &[usize],
) {
    if sig.variadic == Variadic::Capture {
        return;
    }
    if let Some(i) = sig.pos_rest_slot() {
        out.at(i).store(run_tuple(strand, pairs, run));
    }
    if let Some(i) = sig.key_rest_slot() {
        let pairs = unconsumed(pairs, consumed);
        strand
            .builtin_types()
            .dict_view_iter
            .create(strand, Iter { pairs, index: 0 }, out.at(i));
    }
}

/// Matches `pairs` against `sig`, filling in the positional and key slots of
/// `out`.
///
/// Returns the mask of pairs that were consumed; the caller settles the
/// leftovers with [`settle_leftovers`] and owns producing the
/// [`Variadic::Capture`] tail, since how the leftovers are best represented
/// depends on where the pairs came from (a fresh snapshot can be moved into a
/// new iterator, an existing iterator can just drop them).
fn unpack_sig_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: &[(Value<'v>, Value<'v>)],
    sig: &sig::Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
) -> Result<'v, 's, BitBox> {
    let mut consumed = bitbox![0; pairs.len()];
    let pos_count = sig.required + sig.optional.len();
    for index in 0..pos_count {
        let index_value = i64::try_from(index).map_err(|_| Error::overflow(strand))?;
        let key = Value::from_i64(strand, index_value);
        if let Some((found, (_, value))) = pairs
            .iter()
            .enumerate()
            .find(|(found, pair)| !consumed[*found] && pair.0.op_eq(strand, &key).to_bool(strand))
        {
            consumed.set(found, true);
            out.at(index).store(value.dup());
        } else if let Some(default) = sig.optional.get(index.saturating_sub(sig.required)) {
            out.at(index).store(default.dup());
        } else {
            return Err(Error::missing_positional(strand, index));
        }
    }
    for (offset, spec) in sig.keys.iter().enumerate() {
        let key = match &spec.kind {
            sig::UnpackKeyKind::Sym(sym) => Value::from_input(strand, sym.as_str(strand)),
            sig::UnpackKeyKind::Const(value) => value.dup(),
        };
        if let Some((found, (_, value))) = pairs
            .iter()
            .enumerate()
            .find(|(found, pair)| !consumed[*found] && pair.0.op_eq(strand, &key).to_bool(strand))
        {
            consumed.set(found, true);
            out.at(pos_count + offset).store(value.dup());
        } else if let Some(default) = &spec.default {
            out.at(pos_count + offset).store(default.dup());
        } else {
            return Err(match &spec.kind {
                sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                sig::UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
            });
        }
    }
    Ok(consumed)
}

fn debug<'v, 's>(
    module: &str,
    name: &str,
    strand: &mut Strand<'v, 's>,
    w: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    crate::fmt!(strand, w, "<{module}.{name}>")
}

impl<'v> Protocol<'v> for View<'v> {
    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let view = this.get();
        debug(view.glue.module(), view.glue.name(), strand, w)
    }
    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let view = this.get();
        view.glue.len(&view.owner, strand) != 0
    }
    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let equal = other
            .downcast_ref(strand.builtin_types().dict_view)
            .is_some_and(|other| this.as_header() == other.into_raw().cast());
        Ok(Value::from_bool(equal))
    }
    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let view = this.get();
        if view.glue.get(&view.owner, strand, key, -1, out)? {
            Ok(())
        } else {
            Err(Error::index(strand))
        }
    }
    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let view = this.get();
        view.glue.set(&view.owner, strand, key, value)
    }
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::LEN {
            let view = this.get();
            let len = view.glue.len(&view.owner, strand);
            Output::set(strand, out, len);
            Ok(())
        } else if matches!(
            field.tag(),
            sym::GET
                | sym::PAIRS
                | sym::KEYS
                | sym::VALUES
                | sym::COUNT
                | sym::COPY
                | sym::CONTAINS
        ) {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            super::iter::iterable_get(strand, &this, field, out)
        }
    }
    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if method.tag() == sym::LEN {
            return Err(Error::type_error(
                strand,
                "dictionary view len is a field, not a method",
            ));
        }
        if method.tag() == sym::GET {
            // Implemented directly against the glue, unlike every other
            // method below, so a lookup never needs to flatten/snapshot the
            // whole view into a real `Dict` first — the entire point of a
            // lazy projection.
            let default_key = Sym::well_known(sym::DEFAULT);
            let else_key = Sym::well_known(sym::ELSE);
            let ([key], [instance, default, or_else]) =
                unpack!(strand, args, 1, 1, default_key = None, else_key = None)?;
            if default.is_some() && or_else.is_some() {
                return Err(Error::unexpected_key(strand, else_key));
            }
            let instance = match instance {
                Some(s) => s.to_i64(strand).map_err(|_| Error::index(strand))?,
                None => -1,
            };
            let found = {
                let view = this.get();
                view.glue.get(
                    &view.owner,
                    strand,
                    &key,
                    instance,
                    Slot::reborrow(&mut out),
                )?
            };
            return if found {
                Ok(())
            } else if let Some(mut default) = default {
                out.store(default.take());
                Ok(())
            } else if let Some(or_else) = or_else {
                call!(strand, or_else, out).await
            } else {
                out.store(Value::NIL);
                Ok(())
            };
        }
        if method.tag() == sym::KEYS {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let view = this.get();
            return view.glue.keys(&view.owner, strand, out);
        }
        if method.tag() == sym::VALUES && args.len() > 0 {
            // Only handled lazily when a key is given; `.values()` with no
            // key (every value, unfiltered) falls through to the generic
            // flatten-based path below, same as every other method here.
            if args.len() > 1 {
                return Err(Error::unexpected_positional(strand, 1));
            }
            let key = match args.next() {
                Some(Arg::Pos(slot)) => Value::from_input(strand, slot),
                Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
                None => unreachable!(),
            };
            let view = this.get();
            return view.glue.values(&view.owner, strand, &key, out);
        }
        let pairs = {
            let view = this.get();
            flatten_glue(view, strand)?
        };
        snapshot_dict(strand, pairs)?
            .op_mcall(strand, method, args, out)
            .await
    }
    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten_glue(this.get(), strand)?;
        strand
            .builtin_types()
            .dict_view_iter
            .create(strand, Iter { pairs, index: 0 }, out);
        Ok(())
    }
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten_glue(this.get(), strand)?;
        spread_pairs(strand, &pairs, context, sink)
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten_glue(this.get(), strand)?;
        let mut consumed = unpack_sig_pairs(strand, &pairs, sig, &mut out)?;
        let run = settle_pairs(strand, sig, &pairs, &mut consumed)?;
        store_split_rests(strand, sig, &mut out, &pairs, &consumed, &run);
        if sig.variadic == Variadic::Capture {
            // The snapshot is ours, so the tail moves into the iterator.
            let pairs = pairs
                .into_iter()
                .zip(consumed.iter().by_vals())
                .filter_map(|(pair, consumed)| (!consumed).then_some(pair))
                .collect();
            strand.builtin_types().dict_view_iter.create(
                strand,
                Iter { pairs, index: 0 },
                out.at(sig.len() - 1),
            );
        }
        Ok(())
    }
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().dict_view)
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<dictionary view iterator>")
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut iter = this.borrow_mut(strand)?;
        let Some((key, value)) = iter.pairs.get(iter.index) else {
            return Ok(false);
        };
        out.store(Value::from_object(super::tuple::tuple(
            strand,
            [key.dup(), value.dup()],
        )));
        iter.index += 1;
        Ok(true)
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Unpack against the remaining pairs in place: no copy of the tail,
        // and the iterator is only advanced once the unpack has succeeded, so
        // a failure leaves it exactly where it was. The shared borrow is held
        // across the unpack, so re-entering this iterator from a key
        // comparison is reported as a concurrency error rather than seeing a
        // half-consumed iterator.
        let consumed = {
            let iter = this.borrow(strand)?;
            let pairs = &iter.pairs[iter.index..];
            let mut consumed = unpack_sig_pairs(strand, pairs, sig, &mut out)?;
            let run = settle_pairs(strand, sig, pairs, &mut consumed)?;
            store_split_rests(strand, sig, &mut out, pairs, &consumed, &run);
            consumed
        };
        {
            let mut iter = this.borrow_mut(strand)?;
            if sig.variadic == Variadic::Capture {
                // The capture is this same iterator, rewound over whatever the
                // unpack left behind: drop the pairs it took (along with
                // everything consumed by earlier operations) and keep the rest
                // in place.
                let start = iter.index;
                let mut index = 0;
                iter.pairs.retain(|_| {
                    let keep = index >= start && !consumed[index - start];
                    index += 1;
                    keep
                });
                iter.index = 0;
            } else {
                iter.index = iter.pairs.len();
            }
        }
        if sig.variadic == Variadic::Capture {
            Output::set(strand, out.at(sig.len() - 1), &this);
        }
        Ok(())
    }
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        // As in `op_unpack`: spread the remaining pairs in place rather than
        // copying the tail out, and only mark them consumed once the whole
        // spread has succeeded.
        let end = {
            let iter = this.borrow(strand)?;
            spread_pairs(strand, &iter.pairs[iter.index..], context, sink)?;
            iter.pairs.len()
        };
        this.borrow_mut(strand)?.index = end;
        Ok(())
    }
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iter_get(strand, &this, field, out)
    }
    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iter_mcall(strand, &this, method, args, out).await
    }
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }
}

/// Type object shared by every dict view.
///
/// See [`crate::object::array_view::Type`] for why views need a real type
/// object rather than answering `Value`.
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

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.DictView>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: &[],
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
            ],
        })
    }
}
