//! Lazy dictionary-like projections over native objects.

use std::{convert::Infallible, hash::Hasher, marker::PhantomData, ops::ControlFlow};

use crate::value::fmt::Format;

use bitvec::{bitbox, boxed::BitBox};
use dolang_bytecode::{Rest as RestKind, Variadic};

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit},
    object::{
        BoundMethod, dict,
        native::{Instance, Object, Spread, SpreadContext, Type as NativeType, Unpack, UnpackItem},
        protocol::{Inspect, Protocol, Recv, members},
    },
    sig,
    strand::Strand,
    sym,
    sym::Sym,
    unpack,
    value::{Input, Output, Slot, Slots, TypeObject, Value},
    vm::{Alloc, AllocExt, Vm},
    with_slots,
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

    /// Removes every value for `key`, returning whether any were present.
    fn delete<'s>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &mut Strand<'v, 's>,
        _key: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        Err(Error::immutable(strand))
    }

    /// Removes one instance into `out`, returning whether it was present.
    /// Negative instances count from the end; a plain `pop` passes zero.
    fn pop<'a, 's>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        _key: &Value<'v>,
        _instance: i64,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        Err(Error::immutable(strand))
    }

    /// Removes all entries.
    fn clear<'s>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &mut Strand<'v, 's>,
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
        strand.new_array(&mut out);
        let array = out.as_array(strand).unwrap();
        let mut seen: Vec<Value<'v>> = Vec::with_capacity(pairs.len());
        'keys: for (key, _) in pairs {
            for seen in &seen {
                if seen.eq(strand, &key)? {
                    continue 'keys;
                }
            }
            array.push(strand, &key)?;
            seen.push(key);
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
        strand.new_array(&mut out);
        let array = out.as_array(strand).unwrap();
        with_slots!(strand, |mut tmp| {
            let mut instance = 0i64;
            while self.get(this, strand, key, instance, Slot::reborrow(&mut tmp))? {
                array.push(strand, &tmp)?;
                instance += 1;
            }
            Ok(())
        })
    }
}

/// [`Object`] method implementations over a [`DictLike`] projection of a native object.
///
/// To expose the projection itself as a value, use
/// [`AllocExt::dict_view`](crate::vm::AllocExt::dict_view).
pub struct DictView<'v, I: DictLike<'v>>(Infallible, PhantomData<(&'v (), I)>);

impl<'v, I: DictLike<'v>> DictView<'v, I> {
    /// Invokes a view method on a native owner, rooting the temporary view.
    pub async fn method<'s>(
        owner: Instance<'v, '_, I::Object>,
        view: I,
        strand: &mut Strand<'v, 's>,
        method: Sym<'v, '_>,
        args: Args<'v, '_>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut slot| {
            strand.dict_view(owner, view, &mut slot);
            slot.method(strand, method, args, out).await
        })
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
        _context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten(view, owner, strand)?;
        spread_pairs(strand, &pairs, sink)
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

/// Store a new dictionary view of `owner` into `out`.
pub(crate) fn create<'v, I: DictLike<'v>>(
    alloc: &mut impl Alloc<'v>,
    owner: Instance<'v, '_, I::Object>,
    view: I,
    out: impl Output<'v>,
) {
    let vm = alloc.alloc_vm(crate::vm::private::Sealed);
    let ty = owner.ty(vm);
    // A duplicate of a rooted owner, which stays rooted where it is.
    let owner = Value::from_input(alloc, owner);
    vm.builtin_types().dict_view.create_imm(
        alloc,
        View {
            owner,
            glue: Box::new(Glue { view, ty }),
        },
        out,
    );
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
    fn delete<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
    ) -> Result<'v, 's, bool>;
    fn pop<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool>;
    fn clear<'s>(&self, owner: &Value<'v>, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()>;
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
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.len(inst, strand)
        })
    }
    fn get<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.get(inst, strand, key, instance, out)
        })
    }
    fn set<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.set(inst, strand, key, value)
        })
    }
    fn delete<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.delete(inst, strand, key)
        })
    }
    fn pop<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.pop(inst, strand, key, instance, out)
        })
    }
    fn clear<'s>(&self, owner: &Value<'v>, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.clear(inst, strand)
        })
    }
    fn flatten<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        pairs: &mut Vec<(Value<'v>, Value<'v>)>,
    ) -> Result<'v, 's, ()> {
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.flatten(inst, strand, &mut DictViewSink { pairs })
        })
    }
    fn keys<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.keys(inst, strand, out)
        })
    }
    fn values<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Instance::with_native_value(owner, strand, self.ty, |strand, inst| {
            self.view.values(inst, strand, key, out)
        })
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

/// A keyed rest: a snapshot of the pairs left over.
pub(crate) struct Rest<'v> {
    pairs: Vec<(Value<'v>, Value<'v>)>,
}

unsafe impl<'v> Collect for Rest<'v> {
    const CYCLIC: bool = true;
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

unsafe impl<'v> crate::gc::ImmState for View<'v> {
    const CYCLIC: bool = true;
}

impl<'v> crate::gc::Annex for View<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.owner.accept(visit)
    }
}

pub(crate) type ViewObj<'v> = crate::gc::Imm<View<'v>>;

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
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
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let mut snapshot = dict::Dict::new();
    for (key, value) in pairs {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hash = hasher.finish();
        snapshot.insert(strand, key, value, hash, false)?;
    }
    strand.builtin_types().dict.create(strand, snapshot, out);
    Ok(())
}

/// Spreads pairs keyed, as a dict does: a symbol key as a symbol, any other
/// key as is, so the sink decides what fits
fn spread_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: &[(Value<'v>, Value<'v>)],
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    // The pairs stay borrowed from wherever they're rooted, so each one is
    // copied into scratch slots (never bare stack `Value`s) for the duration
    // of the sink call.
    with_slots!(strand, |mut first, mut second| {
        for (key, value) in pairs {
            Output::set(strand, &mut first, key);
            Output::set(strand, &mut second, value);
            match first.as_sym(strand) {
                Some(sym) => sink.symbol(strand, sym, Slot::reborrow(&mut second))?,
                None => sink.keyed(
                    strand,
                    Slot::reborrow(&mut first),
                    Slot::reborrow(&mut second),
                )?,
            }
        }
        Ok(())
    })
}

/// Matches `pairs` against `unpack`. A view holds only keyed items, so a
/// positional item takes its default or is missing, and a `*` rest is empty.
fn unpack_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: Vec<(Value<'v>, Value<'v>)>,
    mut unpack: Unpack<'v, '_>,
) -> Result<'v, 's, ()> {
    let (key_rest, mixed) = (unpack.key_rest(), unpack.mixed_rest());
    let mut consumed = bitbox![0; pairs.len()];
    let mut position = 0usize;
    let mut rests = Vec::new();
    for item in unpack.iter() {
        let (key, slot, default) = match item {
            UnpackItem::Pos { slot, default } => {
                match default {
                    Some(default) => Output::set(strand, slot, default),
                    None => return Err(Error::missing_positional(strand, position)),
                }
                position += 1;
                continue;
            }
            UnpackItem::SymKey { key, slot, default } => {
                (Value::from_input(strand, key), slot, default)
            }
            UnpackItem::ConstKey { key, slot, default } => (key.dup(), slot, default),
            rest => {
                rests.push(rest);
                continue;
            }
        };

        let found = find_pair(strand, &pairs, &consumed, &key)?;
        if let Some((index, (_, value))) = found {
            consumed.set(index, true);
            Output::set(strand, slot, value);
        } else if let Some(default) = default {
            Output::set(strand, slot, default);
        } else if let Some(sym) = key.as_sym(strand) {
            return Err(Error::missing_key(strand, sym));
        } else {
            return Err(Error::missing_key(strand, &key));
        }
    }
    settle_leftovers(strand, &pairs, &consumed, key_rest, mixed)?;
    for rest in rests {
        match rest {
            UnpackItem::Rest { slot } | UnpackItem::KeyRest { slot } => {
                let pairs = unconsumed(&pairs, &consumed);
                strand
                    .builtin_types()
                    .dict_view_rest
                    .create(strand, Rest { pairs }, slot);
            }
            UnpackItem::PosRest { slot } => {
                let tuple = empty_tuple(strand);
                Output::set(strand, slot, &tuple);
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

/// Checks the pairs an unpack left over: unless a `**` or `...` rest takes
/// them, there must be none.
fn settle_leftovers<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: &[(Value<'v>, Value<'v>)],
    consumed: &BitBox,
    key_rest: RestKind,
    mixed: bool,
) -> Result<'v, 's, ()> {
    if !mixed
        && key_rest == RestKind::None
        && let Some(index) = consumed.first_zero()
    {
        return Err(Error::unexpected_key(strand, &pairs[index].0));
    }
    Ok(())
}

fn settle_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sig: &sig::Unpack<'v, '_>,
    pairs: &[(Value<'v>, Value<'v>)],
    consumed: &BitBox,
) -> Result<'v, 's, ()> {
    let mixed = matches!(sig.variadic, Variadic::Discard | Variadic::Capture);
    settle_leftovers(strand, pairs, consumed, sig.key_rest(), mixed)
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

fn empty_tuple<'v>(strand: &mut Strand<'v, '_>) -> Value<'v> {
    Value::from_object(super::tuple::tuple(strand, []))
}

/// Stores a `*name` rest's tuple, which is empty, and a `**name` rest's
/// view.
fn store_split_rests<'v>(
    strand: &mut Strand<'v, '_>,
    sig: &sig::Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    pairs: &[(Value<'v>, Value<'v>)],
    consumed: &BitBox,
) {
    if sig.variadic == Variadic::Capture {
        return;
    }
    if let Some(i) = sig.pos_rest_slot() {
        out.at(i).store(empty_tuple(strand));
    }
    if let Some(i) = sig.key_rest_slot() {
        let pairs = unconsumed(pairs, consumed);
        strand
            .builtin_types()
            .dict_view_rest
            .create(strand, Rest { pairs }, out.at(i));
    }
}

fn find_pair<'a, 'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: &'a [(Value<'v>, Value<'v>)],
    consumed: &bitvec::slice::BitSlice,
    key: &Value<'v>,
) -> Result<'v, 's, Option<(usize, &'a (Value<'v>, Value<'v>))>> {
    for (index, pair) in pairs.iter().enumerate() {
        if !consumed[index] && pair.0.eq(strand, key)? {
            return Ok(Some((index, pair)));
        }
    }
    Ok(None)
}

/// Matches `pairs` against `sig`, filling in the positional and key slots of
/// `out`.
///
/// Returns the mask of pairs that were consumed; the caller settles the
/// leftovers with [`settle_leftovers`] and owns producing the
/// [`Variadic::Capture`] tail, since how the leftovers are best represented
/// depends on where the pairs came from.
fn unpack_sig_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: &[(Value<'v>, Value<'v>)],
    sig: &sig::Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
) -> Result<'v, 's, BitBox> {
    let mut consumed = bitbox![0; pairs.len()];
    if sig.required > 0 {
        return Err(Error::missing_positional(strand, 0));
    }
    let pos_count = sig.optional.len();
    for (index, default) in sig.optional.iter().enumerate() {
        out.at(index).store(default.dup());
    }
    for (offset, spec) in sig.keys.iter().enumerate() {
        let key = Value::from_input(strand, &spec.kind);
        if let Some((found, (_, value))) = find_pair(strand, pairs, &consumed, &key)? {
            consumed.set(found, true);
            out.at(pos_count + offset).store(value.dup());
        } else if let Some(default) = &spec.default {
            out.at(pos_count + offset).store(default.dup());
        } else {
            return Err(Error::missing_key(strand, &spec.kind));
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

impl<'v> Protocol<'v> for ViewObj<'v> {
    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let view = this.get();
        debug(view.glue.module(), view.glue.name(), strand, w)
    }
    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        let view = this.get();
        Ok(view.glue.len(&view.owner, strand) != 0)
    }
    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().dict_view) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        Ok(Value::from_bool(
            this.as_header() == gc::Base::as_ptr(other).cast(),
        ))
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
                | sym::DELETE
                | sym::POP
                | sym::CLEAR
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
        if method.tag() == sym::DELETE {
            let ([key], []) = unpack!(strand, args, 1, 0)?;
            let view = this.get();
            let removed = view.glue.delete(&view.owner, strand, &key)?;
            Output::set(strand, out, removed);
            return Ok(());
        }
        if method.tag() == sym::CLEAR {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let view = this.get();
            return view.glue.clear(&view.owner, strand);
        }
        if matches!(method.tag(), sym::GET | sym::POP) {
            // Lookups stay lazy, and pops must change the owner rather than
            // the temporary snapshot used by read-only methods below.
            let default_key = Sym::well_known(sym::DEFAULT);
            let else_key = Sym::well_known(sym::ELSE);
            let ([key], [instance, default, or_else]) =
                unpack!(strand, args, 1, 1, default_key = None, else_key = None)?;
            if default.is_some() && or_else.is_some() {
                return Err(Error::unexpected_key(strand, else_key));
            }
            let instance = match instance {
                Some(s) => s.to_i64(strand).map_err(|_| Error::index(strand))?,
                None if method.tag() == sym::POP => 0,
                None => -1,
            };
            let found = {
                let view = this.get();
                if method.tag() == sym::POP {
                    view.glue.pop(
                        &view.owner,
                        strand,
                        &key,
                        instance,
                        Slot::reborrow(&mut out),
                    )?
                } else {
                    view.glue.get(
                        &view.owner,
                        strand,
                        &key,
                        instance,
                        Slot::reborrow(&mut out),
                    )?
                }
            };
            return if found {
                Ok(())
            } else if let Some(mut default) = default {
                out.store(default.take());
                Ok(())
            } else if let Some(or_else) = or_else {
                call!(strand, or_else, out)
            } else if method.tag() == sym::POP {
                Err(Error::index(strand))
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
        // The snapshot is called across an await, so it lives in a scanned slot.
        with_slots!(strand, |mut snapshot| {
            snapshot_dict(strand, pairs, &mut snapshot)?;
            snapshot.op_mcall(strand, method, args, out).await
        })
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
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten_glue(this.get(), strand)?;
        spread_pairs(strand, &pairs, sink)
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten_glue(this.get(), strand)?;
        let consumed = unpack_sig_pairs(strand, &pairs, sig, &mut out)?;
        settle_pairs(strand, sig, &pairs, &consumed)?;
        store_split_rests(strand, sig, &mut out, &pairs, &consumed);
        if sig.variadic == Variadic::Capture {
            let pairs = unconsumed(&pairs, &consumed);
            strand.builtin_types().dict_view_rest.create(
                strand,
                Rest { pairs },
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut iter = this.borrow_mut(strand)?;
        let Some((key, value)) = iter.pairs.get(iter.index) else {
            return Ok(false);
        };
        super::tuple::create(strand, [key.dup(), value.dup()], out);
        iter.index += 1;
        Ok(true)
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let iter = this.borrow(strand)?;
        let next = super::iter::unpack_cursor(strand, sig, &mut out, iter.index, |strand, i| {
            let (key, value) = iter.pairs.get(i)?;
            let pair = super::tuple::tuple(strand, [key.dup(), value.dup()]);
            Some((i + 1, Value::from_object(pair)))
        })?;
        if let Some(i) = sig.pos_rest_slot() {
            let pairs = iter.pairs[next..]
                .iter()
                .map(|(key, value)| (key.dup(), value.dup()))
                .collect();
            strand.builtin_types().dict_view_iter.create(
                strand,
                Iter { pairs, index: 0 },
                out.at(i),
            );
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
    }
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this, sink).await
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
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().iterable)
                .unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
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
            is_abstract: false,
            is_final: true,
            members: &[],
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
            ],
        })
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            "std.DictView is not instantiable",
        ))
    }
}

impl<'v> Protocol<'v> for Rest<'v> {
    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<dictionary view rest>")
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let rest = this.borrow(strand)?;
        let consumed = unpack_sig_pairs(strand, &rest.pairs, sig, &mut out)?;
        settle_pairs(strand, sig, &rest.pairs, &consumed)?;
        store_split_rests(strand, sig, &mut out, &rest.pairs, &consumed);
        if sig.variadic == Variadic::Capture {
            let pairs = unconsumed(&rest.pairs, &consumed);
            strand.builtin_types().dict_view_rest.create(
                strand,
                Rest { pairs },
                out.at(sig.len() - 1),
            );
        }
        Ok(())
    }
    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pairs = this
            .borrow(strand)?
            .pairs
            .iter()
            .map(|(key, value)| (key.dup(), value.dup()))
            .collect();
        strand
            .builtin_types()
            .dict_view_iter
            .create(strand, Iter { pairs, index: 0 }, out);
        Ok(())
    }
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let rest = this.borrow(strand)?;
        spread_pairs(strand, &rest.pairs, sink)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, TypeObject::Iterable)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iterable_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iterable_mcall(strand, &this, method, args, out).await
    }
}

#[cfg(test)]
mod tests {
    use crate::{error::ErrorKind, method, test_support::with_builder, with_instance};

    use super::*;

    struct Fixture(Vec<(i64, Option<i64>)>);

    impl<'v> Object<'v> for Fixture {
        const NAME: &'v str = "Fixture";
        const MODULE: &'v str = "test";
        type Annex = ();
        type Type = ();
        type TypeAnnex = ();
    }

    struct Projection<const MUTABLE: bool>;

    fn selected(pairs: &[(i64, Option<i64>)], key: i64, instance: i64) -> Option<usize> {
        let matches: Vec<_> = pairs
            .iter()
            .enumerate()
            .filter_map(|(i, (k, _))| (*k == key).then_some(i))
            .collect();
        let index = if instance < 0 {
            matches
                .len()
                .checked_sub(instance.unsigned_abs() as usize)?
        } else {
            usize::try_from(instance).ok()?
        };
        matches.get(index).copied()
    }

    impl<'v, const MUTABLE: bool> DictLike<'v> for Projection<MUTABLE> {
        type Object = Fixture;
        const NAME: &'v str = "Projection";
        const MODULE: &'v str = "test";

        fn len(&self, this: Instance<'v, '_, Fixture>, _strand: &mut Strand<'v, '_>) -> usize {
            this.borrow_unwrap().0.len()
        }

        fn get<'a, 's>(
            &self,
            this: Instance<'v, '_, Fixture>,
            strand: &'a mut Strand<'v, 's>,
            key: &Value<'v>,
            instance: i64,
            out: Slot<'v, 'a>,
        ) -> Result<'v, 's, bool> {
            let pairs = this.borrow(strand)?;
            let Some(index) = selected(&pairs.0, key.to_i64(strand)?, instance) else {
                return Ok(false);
            };
            match pairs.0[index].1 {
                Some(value) => Output::set(strand, out, value),
                None => Output::set(strand, out, &Value::NIL),
            }
            Ok(true)
        }

        fn delete<'s>(
            &self,
            this: Instance<'v, '_, Fixture>,
            strand: &mut Strand<'v, 's>,
            key: &Value<'v>,
        ) -> Result<'v, 's, bool> {
            if !MUTABLE {
                return Err(Error::immutable(strand));
            }
            let key = key.to_i64(strand)?;
            let mut pairs = this.borrow_mut(strand)?;
            let old_len = pairs.0.len();
            pairs.0.retain(|(k, _)| *k != key);
            Ok(old_len != pairs.0.len())
        }

        fn pop<'a, 's>(
            &self,
            this: Instance<'v, '_, Fixture>,
            strand: &'a mut Strand<'v, 's>,
            key: &Value<'v>,
            instance: i64,
            out: Slot<'v, 'a>,
        ) -> Result<'v, 's, bool> {
            if !MUTABLE {
                return Err(Error::immutable(strand));
            }
            let mut pairs = this.borrow_mut(strand)?;
            let Some(index) = selected(&pairs.0, key.to_i64(strand)?, instance) else {
                return Ok(false);
            };
            match pairs.0.remove(index).1 {
                Some(value) => Output::set(strand, out, value),
                None => Output::set(strand, out, &Value::NIL),
            }
            Ok(true)
        }

        fn clear<'s>(
            &self,
            this: Instance<'v, '_, Fixture>,
            strand: &mut Strand<'v, 's>,
        ) -> Result<'v, 's, ()> {
            if !MUTABLE {
                return Err(Error::immutable(strand));
            }
            this.borrow_mut(strand)?.0.clear();
            Ok(())
        }

        fn flatten<'s>(
            &self,
            _this: Instance<'v, '_, Fixture>,
            _strand: &mut Strand<'v, 's>,
            _sink: &mut DictViewSink<'v, '_>,
        ) -> Result<'v, 's, ()> {
            panic!("mutation must never flatten the owner")
        }
    }

    #[test]
    fn mutations_reach_owner_and_handle_instances_and_nil() {
        with_builder(async |vm| {
            let ty = vm.register_type::<Fixture>();
            vm.enter_with_slots::<4, _>(
                async move |strand, [mut owner, mut view, mut out, mut bound]| {
                    ty.create(
                        strand,
                        Fixture(vec![
                            (1, Some(10)),
                            (1, Some(20)),
                            (1, Some(30)),
                            (2, None),
                            (3, Some(40)),
                            (3, Some(50)),
                        ]),
                        &mut owner,
                    );
                    let cast = ty.cast(&owner).unwrap();
                    with_instance!(strand, cast, |native| {
                        strand.dict_view(native, Projection::<true>, &mut view);
                        let pop = Sym::well_known(sym::POP);
                        method!(strand, &view, pop, 1_i64, &mut out).unwrap();
                        assert_eq!(out.as_int(strand), Some(10));
                        assert_eq!(native.borrow_unwrap().0.len(), 5);
                        method!(strand, &view, pop, 1_i64, -1_i64, &mut out).unwrap();
                        assert_eq!(out.as_int(strand), Some(30));
                        view.get(strand, pop, &mut bound).unwrap();
                        call!(strand, &bound, 2_i64, &mut out).unwrap();
                        assert!(out.is_nil());
                        let delete = Sym::well_known(sym::DELETE);
                        view.get(strand, delete, &mut bound).unwrap();
                        call!(strand, &bound, 3_i64, &mut out).unwrap();
                        assert!(out.to_bool(strand).unwrap());
                        assert_eq!(native.borrow_unwrap().0, vec![(1, Some(20))]);
                        method!(strand, &view, delete, 3_i64, &mut out).unwrap();
                        assert!(!out.to_bool(strand).unwrap());
                        let clear = Sym::well_known(sym::CLEAR);
                        view.get(strand, clear, &mut bound).unwrap();
                        call!(strand, &bound, &mut out).unwrap();
                        assert!(native.borrow_unwrap().0.is_empty());
                        let err = method!(strand, &view, pop, 1_i64, &mut out).unwrap_err();
                        assert_eq!(err.kind(), ErrorKind::Index);
                        let default = Sym::well_known(sym::DEFAULT);
                        method!(strand, &view, pop, 1_i64, default: 99_i64, &mut out).unwrap();
                        assert_eq!(out.as_int(strand), Some(99));
                    });
                },
            )
            .await;
        });
    }

    #[test]
    fn invalid_fallbacks_do_not_mutate_and_readonly_views_reject_mutation() {
        with_builder(async |vm| {
            let ty = vm.register_type::<Fixture>();
            vm.enter_with_slots::<3, _>(async move |strand, [mut owner, mut view, mut out]| {
                ty.create(strand, Fixture(vec![(1, Some(10))]), &mut owner);
                let cast = ty.cast(&owner).unwrap();
                with_instance!(strand, cast, |native| {
                    strand.dict_view(native, Projection::<true>, &mut view);
                    let default = Sym::well_known(sym::DEFAULT);
                    let otherwise = Sym::well_known(sym::ELSE);
                    let err = method!(strand, &view, Sym::well_known(sym::POP), 1_i64, default: 5_i64, otherwise: 6_i64, &mut out).unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::UnexpectedKey);
                    assert_eq!(native.borrow_unwrap().0, vec![(1, Some(10))]);
                    strand.dict_view(native, Projection::<false>, &mut view);
                    for method in [sym::DELETE, sym::POP] {
                        let err = method!(strand, &view, Sym::well_known(method), 1_i64, &mut out).unwrap_err();
                        assert_eq!(err.kind(), ErrorKind::Immutable);
                    }
                    let err = method!(strand, &view, Sym::well_known(sym::CLEAR), &mut out).unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Immutable);
                    assert_eq!(native.borrow_unwrap().0, vec![(1, Some(10))]);
                });
            }).await;
        });
    }
}
