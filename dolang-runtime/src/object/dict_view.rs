//! Lazy dictionary-like projections over native objects.

use std::{fmt, marker::PhantomData, ops::ControlFlow, ptr};

use dolang_bytecode::Variadic;

use crate::{
    arg::Args,
    error::{Error, Result, ResultExt as _},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod, dict,
        native::{Instance, Object, Spread, SpreadContext, Unpack, UnpackItem},
        protocol::{GcObj, Protocol, Recv},
    },
    sig,
    strand::Strand,
    sym,
    sym::Sym,
    value::{Input, InputBy, Output, Slot, Slots, TypeObject, Value, private::Sealed},
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
pub trait DictLike<'v>: 'v {
    type Object: Object<'v>;

    const MODULE: &'v str;
    const NAME: &'v str;

    fn len(this: Instance<'v, '_, Self::Object>, strand: &mut Strand<'v, '_>) -> usize;

    /// Writes the first value for `key` and returns whether it was present.
    fn get<'a, 's>(
        this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool>;

    fn set<'a, 's>(
        _this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        _key: Slot<'v, 'a>,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    fn flatten<'s>(
        this: Instance<'v, '_, Self::Object>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()>;
}

/// Input wrapper that creates a dictionary view of a native object.
pub struct DictView<'v, 'a, I: DictLike<'v>> {
    owner: Instance<'v, 'a, I::Object>,
    marker: PhantomData<I>,
}

impl<'v, 'a, I: DictLike<'v>> DictView<'v, 'a, I> {
    pub fn new(owner: Instance<'v, 'a, I::Object>) -> Self {
        Self {
            owner,
            marker: PhantomData,
        }
    }

    /// Implements [`Object::input`] directly without constructing a view object.
    pub fn input<'s>(
        owner: Instance<'v, '_, I::Object>,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten::<I>(owner, strand)?;
        strand
            .builtin_types()
            .dict_view_iter
            .create(strand, Iter { pairs, index: 0 }, out);
        Ok(())
    }

    /// Implements [`Object::spread`] directly without constructing a view object.
    pub fn spread<'s>(
        owner: Instance<'v, '_, I::Object>,
        strand: &mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten::<I>(owner, strand)?;
        spread_pairs(strand, pairs, context, sink)
    }

    /// Implements [`Object::unpack`] directly without constructing a view object.
    pub fn unpack<'s>(
        owner: Instance<'v, '_, I::Object>,
        strand: &mut Strand<'v, 's>,
        unpack: Unpack<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let pairs = flatten::<I>(owner, strand)?;
        unpack_pairs(strand, pairs, unpack)
    }
}

impl<'v, I: DictLike<'v>> Input<'v> for DictView<'v, '_, I> {
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: Sealed) -> InputBy<'v, 'a> {
        let owner = Value::from_input(vm, self.owner);
        let value = GcObj::new(
            vm.arena(),
            vm.builtin_types().dict_view,
            View {
                owner,
                glue: Box::new(Glue::<I>(PhantomData)),
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
}

struct Glue<I>(PhantomData<I>);

impl<'v, I: DictLike<'v>> DictViewGlue<'v> for Glue<I> {
    fn module(&self) -> &'v str {
        I::MODULE
    }
    fn name(&self) -> &'v str {
        I::NAME
    }
    fn len(&self, owner: &Value<'v>, strand: &mut Strand<'v, '_>) -> usize {
        I::len(unsafe { Instance::from_value_unchecked(owner) }, strand)
    }
    fn get<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        I::get(
            unsafe { Instance::from_value_unchecked(owner) },
            strand,
            key,
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
        I::set(
            unsafe { Instance::from_value_unchecked(owner) },
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
        I::flatten(
            unsafe { Instance::from_value_unchecked(owner) },
            strand,
            &mut DictViewSink { pairs },
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
    owner: Instance<'v, '_, I::Object>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, Vec<(Value<'v>, Value<'v>)>> {
    let expected = I::len(owner, strand);
    let mut pairs = Vec::with_capacity(expected);
    I::flatten(owner, strand, &mut DictViewSink { pairs: &mut pairs })?;
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
        let hash = super::kv::hash(strand, &key)?;
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
    pairs: Vec<(Value<'v>, Value<'v>)>,
    context: SpreadContext,
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    for (mut key, mut value) in pairs {
        if context == SpreadContext::Pairs {
            sink.keyed(strand, Slot::new(&mut key), Slot::new(&mut value))?;
        } else {
            let mut pair = Value::from_object(super::tuple::tuple(strand, [key, value]));
            sink.positional(strand, Slot::new(&mut pair))?;
        }
    }
    Ok(())
}

fn unpack_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: Vec<(Value<'v>, Value<'v>)>,
    mut unpack: Unpack<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut consumed = vec![false; pairs.len()];
    let mut position = 0i64;
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
            UnpackItem::Rest { slot } => {
                let pairs = pairs
                    .iter()
                    .zip(&consumed)
                    .filter(|(_, consumed)| !**consumed)
                    .map(|((key, value), _)| (key.dup(), value.dup()))
                    .collect();
                strand.builtin_types().dict_view_iter.create(
                    strand,
                    Iter { pairs, index: 0 },
                    slot,
                );
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
            consumed[index] = true;
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
    if unpack.exhaustive() && consumed.iter().any(|consumed| !consumed) {
        return Err(Error::unexpected_key(
            strand,
            Sym::well_known(sym::ITEM_ERROR),
        ));
    }
    Ok(())
}

fn unpack_sig_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pairs: Vec<(Value<'v>, Value<'v>)>,
    sig: &sig::Unpack<'v, '_>,
    mut out: Slots<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut consumed = vec![false; pairs.len()];
    let pos_count = sig.required + sig.optional.len();
    for index in 0..pos_count {
        let index_value = i64::try_from(index).map_err(|_| Error::overflow(strand))?;
        let key = Value::from_i64(strand, index_value);
        if let Some((found, (_, value))) = pairs
            .iter()
            .enumerate()
            .find(|(found, pair)| !consumed[*found] && pair.0.op_eq(strand, &key).to_bool(strand))
        {
            consumed[found] = true;
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
            consumed[found] = true;
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
    if sig.variadic == Variadic::None && consumed.iter().any(|consumed| !consumed) {
        return Err(Error::unexpected_key(
            strand,
            Sym::well_known(sym::ITEM_ERROR),
        ));
    }
    if sig.variadic == Variadic::Capture {
        let pairs = pairs
            .into_iter()
            .zip(consumed)
            .filter_map(|(pair, consumed)| (!consumed).then_some(pair))
            .collect();
        strand.builtin_types().dict_view_iter.create(
            strand,
            Iter { pairs, index: 0 },
            out.at(pos_count + sig.keys.len()),
        );
    }
    Ok(())
}

fn debug<'v, 's>(
    module: &str,
    name: &str,
    strand: &mut Strand<'v, 's>,
    w: &mut dyn fmt::Write,
) -> Result<'v, 's, ()> {
    write!(w, "<{module}.{name}>").into_do(strand)
}

impl<'v> Protocol<'v> for View<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }
    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let view = this.borrow(strand)?;
        debug(view.glue.module(), view.glue.name(), strand, w)
    }
    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let view = this.borrow(strand).expect("conflicting borrow");
        view.glue.len(&view.owner, strand) != 0
    }
    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_bool(
            other
                .downcast_ref(strand.builtin_types().dict_view)
                .is_some_and(|other| {
                    ptr::eq(this.as_header().as_ptr(), other.into_raw().cast().as_ptr())
                }),
        ))
    }
    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let view = this.borrow(strand)?;
        if view.glue.get(&view.owner, strand, key, out)? {
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
        let view = this.borrow(strand)?;
        view.glue.set(&view.owner, strand, key, value)
    }
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::LEN {
            let view = this.borrow(strand)?;
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if method.tag() == sym::LEN {
            return Err(Error::type_error(
                strand,
                "dictionary view len is a field, not a method",
            ));
        }
        let pairs = {
            let view = this.borrow(strand)?;
            flatten_glue(&view, strand)?
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
        let pairs = {
            let view = this.borrow(strand)?;
            flatten_glue(&view, strand)?
        };
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
        let pairs = {
            let view = this.borrow(strand)?;
            flatten_glue(&view, strand)?
        };
        spread_pairs(strand, pairs, context, sink)
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pairs = {
            let view = this.borrow(strand)?;
            flatten_glue(&view, strand)?
        };
        unpack_sig_pairs(strand, pairs, sig, out)
    }
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, TypeObject::Value)
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<dictionary view iterator>").into_do(strand)
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
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pairs = {
            let mut iter = this.borrow_mut(strand)?;
            let pairs = iter.pairs[iter.index..]
                .iter()
                .map(|(key, value)| (key.dup(), value.dup()))
                .collect();
            iter.index = iter.pairs.len();
            pairs
        };
        unpack_sig_pairs(strand, pairs, sig, out)
    }
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut iter = this.borrow_mut(strand)?;
        let pairs = iter.pairs[iter.index..]
            .iter()
            .map(|(k, v)| (k.dup(), v.dup()))
            .collect();
        iter.index = iter.pairs.len();
        spread_pairs(strand, pairs, context, sink)
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
