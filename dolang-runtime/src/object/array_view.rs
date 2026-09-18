//! Lazy array-like projections over native objects.

use std::{cell::Cell, ops::ControlFlow};

use crate::value::fmt::Format;

use dolang_bytecode::Variadic;

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit},
    object::{
        BoundMethod, array, index, iter,
        native::{Instance, Object, Type as NativeType, Unpack as NativeUnpack, UnpackItem},
        protocol::{GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, members},
        range,
    },
    sig::{Unpack, UnpackKeyKind},
    strand::Strand,
    sym,
    sym::Sym,
    unpack,
    value::{Input, InputBy, Output, Slot, Slots, TypeObject, Value, private::Sealed},
    vm::Vm,
};

/// Implements a lazy array-like projection over a native object.
///
/// Implement this trait on a marker type. Different marker types may expose
/// different views of the same [`Object`]. Methods take `&self` (rather than
/// being purely associated functions on a zero-sized marker) so a view can
/// be parametrized by a runtime value if needed — e.g. a view scoped to one
/// sub-range or filtered by some predicate captured at construction time.
pub trait ArrayLike<'v>: 'v {
    type Object: Object<'v>;

    const MODULE: &'v str;
    const NAME: &'v str;

    fn len(&self, this: Instance<'v, '_, Self::Object>, strand: &mut Strand<'v, '_>) -> usize;

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;

    fn set<'a, 's>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        _index: usize,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    fn push<'a, 's>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        _values: &mut [Slot<'v, 'a>],
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    fn insert<'a, 's>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        _index: usize,
        _values: &mut [Slot<'v, 'a>],
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    fn pop<'a, 's>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        _index: usize,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    fn delete<'s>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &mut Strand<'v, 's>,
        _index: usize,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    fn clear<'s>(
        &self,
        _this: Instance<'v, '_, Self::Object>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }
}

/// Input wrapper that creates a lazy array view of a native object.
pub struct ArrayView<'v, 'a, I: ArrayLike<'v>> {
    owner: Instance<'v, 'a, I::Object>,
    // `Option` so `input_take` can steal `view` out rather than requiring
    // `I: Clone`. Using an `ArrayView` more than once (it's meant to be
    // constructed and immediately handed to `Output::set`/`Value::from_input`)
    // is a programming error, not something to recover from.
    view: Option<I>,
}

impl<'v, 'a, I: ArrayLike<'v>> ArrayView<'v, 'a, I> {
    pub fn new(owner: Instance<'v, 'a, I::Object>, view: I) -> Self {
        Self {
            owner,
            view: Some(view),
        }
    }

    /// Implements [`Object::index`] directly without exposing a view object.
    pub fn index<'s>(
        owner: Instance<'v, '_, I::Object>,
        view: I,
        strand: &mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let ty = owner.ty(strand.vm());
        let owner = Value::from_input(strand, owner);
        index_from(&owner, &Glue { view, ty }, strand, index, out)
    }

    /// Implements [`Object::iter`] directly without exposing a view object.
    pub fn iter<'s>(
        owner: Instance<'v, '_, I::Object>,
        view: I,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let parent = create_view(owner, view, strand);
        strand.builtin_types().array_view_iter.create(
            strand,
            Iter {
                parent,
                index: 0.into(),
            },
            out,
        );
        Ok(())
    }

    /// Implements [`Object::spread`] directly without exposing a view object.
    pub fn spread<'s>(
        owner: Instance<'v, '_, I::Object>,
        view: I,
        strand: &mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let ty = owner.ty(strand.vm());
        let owner = Value::from_input(strand, owner);
        spread_from(&owner, &Glue { view, ty }, strand, context, sink)
    }

    /// Implements [`Object::unpack`] directly without exposing a view object.
    pub fn unpack<'s>(
        owner: Instance<'v, '_, I::Object>,
        view: I,
        strand: &mut Strand<'v, 's>,
        unpack: NativeUnpack<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let parent = create_view(owner, view, strand);
        unpack_native(strand, parent, unpack)
    }
}

fn create_view<'v, I: ArrayLike<'v>>(
    owner: Instance<'v, '_, I::Object>,
    view: I,
    strand: &Strand<'v, '_>,
) -> GcObj<'v, View<'v>> {
    let ty = owner.ty(strand.vm());
    let owner = Value::from_input(strand, owner);
    GcObj::new(
        strand.vm().arena(),
        strand.builtin_types().array_view,
        View {
            owner,
            glue: Box::new(Glue { view, ty }),
        },
    )
}

impl<'v, I: ArrayLike<'v>> Input<'v> for ArrayView<'v, '_, I> {
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: Sealed) -> InputBy<'v, 'a> {
        let ty = self.owner.ty(vm);
        let owner = Value::from_input(vm, self.owner);
        let view = self.view.take().expect("ArrayView used more than once");
        let value = GcObj::new(
            vm.arena(),
            vm.builtin_types().array_view,
            View {
                owner,
                glue: Box::new(Glue { view, ty }),
            },
        );
        InputBy::Value(Value::from_object(value), None)
    }
}

trait ArrayViewGlue<'v>: 'v {
    fn module(&self) -> &'v str;
    fn name(&self) -> &'v str;
    fn len(&self, owner: &Value<'v>, strand: &mut Strand<'v, '_>) -> usize;
    fn get<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;
    fn set<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;
    fn push<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        values: &mut [Slot<'v, 'a>],
    ) -> Result<'v, 's, ()>;
    fn insert<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        values: &mut [Slot<'v, 'a>],
    ) -> Result<'v, 's, ()>;
    fn pop<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;
    fn delete<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        index: usize,
    ) -> Result<'v, 's, ()>;
    fn clear<'s>(&self, owner: &Value<'v>, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()>;
}

struct Glue<'v, I: ArrayLike<'v>> {
    view: I,
    ty: NativeType<'v, I::Object>,
}

impl<'v, I: ArrayLike<'v>> ArrayViewGlue<'v> for Glue<'v, I> {
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
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        self.view.get(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            index,
            out,
        )
    }
    fn set<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        self.view.set(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            index,
            value,
        )
    }
    fn push<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        values: &mut [Slot<'v, 'a>],
    ) -> Result<'v, 's, ()> {
        self.view.push(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            values,
        )
    }
    fn insert<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        values: &mut [Slot<'v, 'a>],
    ) -> Result<'v, 's, ()> {
        self.view.insert(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            index,
            values,
        )
    }
    fn pop<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        self.view.pop(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            index,
            out,
        )
    }
    fn delete<'s>(
        &self,
        owner: &Value<'v>,
        strand: &mut Strand<'v, 's>,
        index: usize,
    ) -> Result<'v, 's, ()> {
        self.view.delete(
            Instance::from_native_value(owner, strand, self.ty),
            strand,
            index,
        )
    }
    fn clear<'s>(&self, owner: &Value<'v>, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        self.view
            .clear(Instance::from_native_value(owner, strand, self.ty), strand)
    }
}

pub(crate) struct View<'v> {
    owner: Value<'v>,
    glue: Box<dyn ArrayViewGlue<'v> + 'v>,
}

/// Holds a strong reference to the parent [`View`] rather than duplicating
/// its `owner`/glue. `View` is immutable (`Collect::IMMUTABLE`), so reading
/// through `parent` needs no runtime borrow check — see its `Deref` impl.
/// `index` is a `Cell` so `Iter` itself can be declared immutable too (it's
/// only ever mutated in place, never replaced), giving the same check-free
/// `.get()` access as `View`.
pub(crate) struct Iter<'v> {
    parent: GcObj<'v, View<'v>>,
    index: Cell<usize>,
}

unsafe impl<'v> Collect for View<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ViewAnnex;
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.owner.accept(visit)
    }
    fn clear(&mut self) {
        self.owner.clear()
    }
}

#[derive(Default)]
pub(crate) struct ViewAnnex(Cell<bool>);

impl gc::Annex for ViewAnnex {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&self) {}
}

struct MutationGuard<'a>(&'a Cell<bool>);

impl<'a> MutationGuard<'a> {
    fn try_new(busy: &'a Cell<bool>) -> Option<Self> {
        (!busy.replace(true)).then_some(Self(busy))
    }
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn mutation_guard<'a, 'v, 's>(
    busy: &'a Cell<bool>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, MutationGuard<'a>> {
    MutationGuard::try_new(busy).ok_or_else(|| Error::concurrency(strand))
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.parent.accept(visit)
    }
    fn clear(&mut self) {
        // `Iter` is immutable, so it can't be the thing responsible for
        // breaking a reference cycle running through `parent` — some other,
        // mutable link in the cycle has to do that instead.
    }
}

fn debug<'v, 's>(
    module: &str,
    name: &str,
    strand: &mut Strand<'v, 's>,
    w: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    crate::fmt!(strand, w, "<{module}.{name}>")
}

fn normalize<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    len: usize,
) -> Result<'v, 's, usize> {
    let index = value.to_i64(strand).map_err(|_| Error::index(strand))?;
    index::element(len, index).ok_or_else(|| Error::index(strand))
}

fn positional<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
) -> Result<'v, 's, Vec<Slot<'v, 'a>>> {
    args.map(|arg| match arg {
        Arg::Pos(slot) => Ok(slot),
        Arg::Key(key, _) => Err(Error::unexpected_key(strand, key)),
    })
    .collect()
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
            .downcast_ref(strand.builtin_types().array_view)
            .is_some_and(|other| this.as_header() == other.into_raw().cast());
        Ok(Value::from_bool(equal))
    }
    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let view = this.get();
        index_from(&view.owner, &*view.glue, strand, index, out)
    }
    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let _guard = mutation_guard(&this.annex().0, strand)?;
        let view = this.get();
        let len = view.glue.len(&view.owner, strand);
        let index = normalize(strand, &index, len)?;
        view.glue.set(&view.owner, strand, index, value)
    }
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let view = this.get();
                let len = view.glue.len(&view.owner, strand);
                Output::set(strand, out, len);
                Ok(())
            }
            sym::PUSH | sym::INSERT | sym::POP | sym::DELETE | sym::CLEAR => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => iter::iterable_get(strand, &this, field, out),
        }
    }
    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::PUSH => {
                let mut values = positional(strand, args)?;
                let _guard = mutation_guard(&this.annex().0, strand)?;
                let view = this.get();
                view.glue.push(&view.owner, strand, values.as_mut_slice())
            }
            sym::INSERT => {
                let mut values = positional(strand, args)?;
                if values.is_empty() {
                    return Err(Error::missing_positional(strand, 0));
                }
                let index = values.remove(0);
                if values.is_empty() {
                    return Err(Error::missing_positional(strand, 1));
                }
                let _guard = mutation_guard(&this.annex().0, strand)?;
                let view = this.get();
                let len = view.glue.len(&view.owner, strand);
                let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                let index = index::position(len, index).ok_or_else(|| Error::index(strand))?;
                view.glue
                    .insert(&view.owner, strand, index, values.as_mut_slice())
            }
            sym::POP => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([], [index, default, or_else]) =
                    unpack!(strand, args, 0, 1, default = None, else_key = None)?;
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let guard = mutation_guard(&this.annex().0, strand)?;
                let view = this.get();
                let len = view.glue.len(&view.owner, strand);
                let index = match index {
                    Some(index) => {
                        let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                        index::element(len, index)
                    }
                    None => len.checked_sub(1),
                };
                if let Some(index) = index {
                    return view.glue.pop(&view.owner, strand, index, out);
                }
                drop(guard);
                if let Some(mut default) = default {
                    out.store(default.take());
                } else if let Some(or_else) = or_else {
                    call!(strand, or_else, out).await?;
                } else {
                    return Err(Error::index(strand));
                }
                Ok(())
            }
            sym::DELETE => {
                let ([index], []) = unpack!(strand, args, 1, 0)?;
                let _guard = mutation_guard(&this.annex().0, strand)?;
                let view = this.get();
                let len = view.glue.len(&view.owner, strand);
                let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                if let Some(index) = index::element(len, index) {
                    view.glue.delete(&view.owner, strand, index)?;
                    Output::set(strand, out, true);
                } else {
                    Output::set(strand, out, false);
                }
                Ok(())
            }
            sym::CLEAR => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let _guard = mutation_guard(&this.annex().0, strand)?;
                let view = this.get();
                view.glue.clear(&view.owner, strand)
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "array view len is a field, not a method",
            )),
            _ => iter::iterable_mcall(strand, &this, method, args, out).await,
        }
    }
    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.builtin_types().array_view_iter.create(
            strand,
            Iter {
                parent: this.to_strong(),
                index: Cell::new(0),
            },
            out,
        );
        Ok(())
    }
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let view = this.get();
        spread_from(&view.owner, &*view.glue, strand, context, sink)
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let view = this.get();
        let consumed = unpack_from(strand, sig, &mut out, &view.owner, &*view.glue, 0)?;
        if sig.variadic == Variadic::Capture {
            strand.builtin_types().array_view_iter.create(
                strand,
                Iter {
                    parent: this.to_strong(),
                    index: Cell::new(consumed),
                },
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
        Output::set(strand, out, &strand.singletons().array_view)
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let view = &*this.get().parent;
        debug(view.glue.module(), view.glue.name(), strand, w)
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
        let iter = this.get();
        let view = &*iter.parent;
        let index = iter.index.get();
        if index >= view.glue.len(&view.owner, strand) {
            return Ok(false);
        }
        view.glue.get(&view.owner, strand, index, out)?;
        iter.index.set(index + 1);
        Ok(true)
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let iter = this.get();
        let view = &*iter.parent;
        let consumed = unpack_from(
            strand,
            sig,
            &mut out,
            &view.owner,
            &*view.glue,
            iter.index.get(),
        )?;
        iter.index.set(iter.index.get() + consumed);
        if sig.variadic == Variadic::Capture {
            Output::set(strand, out.at(sig.len() - 1), &this);
        }
        Ok(())
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
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }
}

fn index_from<'v, 's>(
    owner: &Value<'v>,
    glue: &(dyn ArrayViewGlue<'v> + '_),
    strand: &mut Strand<'v, 's>,
    index: &Value<'v>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let len = glue.len(owner, strand);
    if let Some(slice) = range::slice(index, strand, len)? {
        let indices: Box<dyn Iterator<Item = usize>> = match slice {
            range::Slice::Contiguous { start, end } => {
                if start > end {
                    return Err(Error::index(strand));
                }
                Box::new(start..end)
            }
            range::Slice::Stepped(indices) => Box::new(indices.into_iter()),
        };
        let mut array = array::Array::new();
        for index in indices {
            let mut value = Value::NIL;
            glue.get(owner, strand, index, Slot::new(&mut value))?;
            array.inner.push(value);
        }
        strand.builtin_types().array.create(strand, array, out);
        return Ok(());
    }
    let index = normalize(strand, index, len)?;
    glue.get(owner, strand, index, out)
}

fn spread_from<'v, 's>(
    owner: &Value<'v>,
    glue: &(dyn ArrayViewGlue<'v> + '_),
    strand: &mut Strand<'v, 's>,
    _context: SpreadContext,
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    let len = glue.len(owner, strand);
    let mut value = Value::NIL;
    for index in 0..len {
        glue.get(owner, strand, index, Slot::new(&mut value))?;
        sink.positional(strand, Slot::new(&mut value))?;
    }
    Ok(())
}

fn unpack_native<'v, 's>(
    strand: &mut Strand<'v, 's>,
    parent: GcObj<'v, View<'v>>,
    mut unpack: NativeUnpack<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut position = 0usize;
    for item in unpack.iter() {
        match item {
            UnpackItem::Pos { slot, default } => {
                let view = &*parent;
                if position < view.glue.len(&view.owner, strand) {
                    view.glue.get(&view.owner, strand, position, slot)?;
                } else if let Some(default) = default {
                    Output::set(strand, slot, default);
                } else {
                    return Err(Error::missing_positional(strand, position));
                }
                position += 1;
            }
            UnpackItem::SymKey { key, slot, default } => {
                if let Some(default) = default {
                    Output::set(strand, slot, default);
                } else {
                    return Err(Error::missing_key(strand, key));
                }
            }
            UnpackItem::ConstKey { key, slot, default } => {
                if let Some(default) = default {
                    Output::set(strand, slot, default);
                } else {
                    return Err(Error::missing_key(strand, key));
                }
            }
            UnpackItem::Rest { slot } => {
                strand.builtin_types().array_view_iter.create(
                    strand,
                    Iter {
                        parent: parent.clone(),
                        index: position.into(),
                    },
                    slot,
                );
            }
        }
    }
    let view = &*parent;
    if unpack.exhaustive() && position < view.glue.len(&view.owner, strand) {
        return Err(Error::unexpected_positional(strand, position));
    }
    Ok(())
}

fn unpack_from<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sig: &Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    owner: &Value<'v>,
    glue: &(dyn ArrayViewGlue<'v> + '_),
    start: usize,
) -> Result<'v, 's, usize> {
    let len = glue.len(owner, strand).saturating_sub(start);
    let pos_count = sig.required + sig.optional.len();
    if sig.required > len {
        return Err(Error::missing_positional(strand, sig.required));
    }
    if pos_count < len && sig.variadic == Variadic::NONE {
        return Err(Error::unexpected_positional(strand, sig.required));
    }
    let min = pos_count.min(len);
    for i in 0..min {
        glue.get(owner, strand, start + i, out.at(i))?;
    }
    if len < pos_count {
        for (i, default) in sig.optional[(len - sig.required)..].iter().enumerate() {
            out.at(min + i).store(default.dup());
        }
    }
    for (i, key) in sig.keys.iter().enumerate() {
        if let Some(default) = &key.default {
            out.at(min + i).store(default.dup());
        } else {
            return Err(match &key.kind {
                UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
            });
        }
    }
    Ok(min + sig.keys.len())
}

/// Type object shared by every array view.
///
/// Views are a generic projection over extension-supplied glue, so the
/// Do-visible *name* of a view comes from its glue rather than from a
/// dedicated type. One shared type object is still what `op_type` needs to
/// return: `is_instance_of` dispatches `op_subtype` on the type object, so
/// answering `Value` (as this did before) made `type v Iterable` false even
/// though views follow the protocol and expose the surface.
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
        crate::fmt!(strand, w, "<type std.ArrayView>")
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use dolang_bytecode::Variadic;

    use crate::{
        error::ErrorKind,
        object::native::Object,
        sig,
        sym::Sym,
        test_support::with_builder,
        vm::{Builder, Stateful},
    };

    use super::*;

    // ── Fixture: a minimal `Object`/`ArrayLike` pair mirroring what an
    // extension crate would write, used to exercise the generic `unpack_from`/
    // `index_from` logic without a live `.dol` script.

    struct FixtureArray<'v>(Vec<Value<'v>>);

    impl<'v> Object<'v> for FixtureArray<'v> {
        const NAME: &'v str = "FixtureArray";
        const MODULE: &'v str = "test";
        type Annex = ();
        type Type = ();
        type TypeAnnex = ();
    }

    struct FixtureGlue;

    impl<'v> ArrayLike<'v> for FixtureGlue {
        type Object = FixtureArray<'v>;

        const MODULE: &'v str = "test";
        const NAME: &'v str = "FixtureArrayView";

        fn len(&self, this: Instance<'v, '_, Self::Object>, strand: &mut Strand<'v, '_>) -> usize {
            this.borrow(strand).unwrap().0.len()
        }

        fn get<'a, 's>(
            &self,
            this: Instance<'v, '_, Self::Object>,
            strand: &'a mut Strand<'v, 's>,
            index: usize,
            out: Slot<'v, 'a>,
        ) -> Result<'v, 's, ()> {
            let value = this.borrow(strand)?.0[index].dup();
            Output::set(strand, out, &value);
            Ok(())
        }
    }

    struct FixtureState<'v> {
        ty: crate::object::native::Type<'v, FixtureArray<'v>>,
    }

    struct FixtureStateTag;

    impl<'v> Stateful<'v> for FixtureState<'v> {
        type Tag = FixtureStateTag;
    }

    fn configure(vm: &mut Builder<'_>) {
        let ty = vm.register_type::<FixtureArray>();
        vm.register_state(FixtureState { ty });
    }

    /// Populates `out` (which must be a GC-rooted slot, e.g. from
    /// `Strand::with_slots_dynamic`) with a `FixtureArray` containing `items`. Values are
    /// never returned as bare, unrooted locals — every call site roots the owner for as
    /// long as it's needed before this function returns.
    fn make_owner<'v>(strand: &mut Strand<'v, '_>, items: &[i64], out: Slot<'v, '_>) {
        let ty = strand.vm().state::<FixtureState>().ty;
        let values = items.iter().map(|&i| Value::from_i64(strand, i)).collect();
        ty.create_with_annex(strand, FixtureArray(values), (), out);
    }

    fn with_fixture_vm<const N: usize, R: 'static>(
        body: impl for<'v, 's, 'b> AsyncFnOnce(&mut Strand<'v, 's>, [Slot<'v, 'b>; N]) -> R + 'static,
    ) -> R {
        with_builder(async move |vm| {
            configure(vm);
            vm.enter_with_slots::<N, _>(body).await
        })
    }

    #[test]
    fn mutation_guard_rejects_reentry_and_resets() {
        let busy = Cell::new(false);
        let guard = MutationGuard::try_new(&busy).unwrap();
        assert!(MutationGuard::try_new(&busy).is_none());
        drop(guard);
        assert!(MutationGuard::try_new(&busy).is_some());
    }

    #[test]
    fn unpack_from_errors_on_too_few_required_args() {
        with_fixture_vm(async |strand, [mut owner_slot]| {
            make_owner(strand, &[1, 2], Slot::reborrow(&mut owner_slot));
            let owner: &Value = &owner_slot;
            let glue = Glue {
                view: FixtureGlue,
                ty: strand.vm().state::<FixtureState>().ty,
            };
            let sig = sig::Unpack::new(3, vec![], vec![], Variadic::NONE);
            strand
                .with_slots_dynamic(3, async |strand, mut out| {
                    let err = unpack_from(strand, &sig, &mut out, owner, &glue, 0).unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::MissingPos);
                })
                .await;
        });
    }

    #[test]
    fn unpack_from_errors_on_too_many_args_without_variadic() {
        with_fixture_vm(async |strand, [mut owner_slot]| {
            make_owner(strand, &[1, 2, 3], Slot::reborrow(&mut owner_slot));
            let owner: &Value = &owner_slot;
            let glue = Glue {
                view: FixtureGlue,
                ty: strand.vm().state::<FixtureState>().ty,
            };
            let sig = sig::Unpack::new(1, vec![], vec![], Variadic::NONE);
            strand
                .with_slots_dynamic(1, async |strand, mut out| {
                    let err = unpack_from(strand, &sig, &mut out, owner, &glue, 0).unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::UnexpectedPos);
                })
                .await;
        });
    }

    #[test]
    fn unpack_from_variadic_captures_remainder() {
        with_fixture_vm(async |strand, [mut owner_slot]| {
            make_owner(strand, &[1, 2, 3], Slot::reborrow(&mut owner_slot));
            let owner: &Value = &owner_slot;
            let glue = Glue {
                view: FixtureGlue,
                ty: strand.vm().state::<FixtureState>().ty,
            };
            let sig = sig::Unpack::new(1, vec![], vec![], Variadic::Capture);
            strand
                .with_slots_dynamic(1, async |strand, mut out| {
                    // `unpack_from` itself only fills the required/optional/key
                    // slots; the variadic capture slot is filled by the caller
                    // (see `View::op_unpack`). Here we only assert it doesn't
                    // error and reports 1 item consumed.
                    let consumed = unpack_from(strand, &sig, &mut out, owner, &glue, 0).unwrap();
                    assert_eq!(consumed, 1);
                })
                .await;
        });
    }

    #[test]
    fn unpack_from_missing_key_without_default_errors() {
        with_fixture_vm(async |strand, [mut owner_slot]| {
            make_owner(strand, &[], Slot::reborrow(&mut owner_slot));
            let owner: &Value = &owner_slot;
            let glue = Glue {
                view: FixtureGlue,
                ty: strand.vm().state::<FixtureState>().ty,
            };
            let key_sym = Sym::well_known(sym::LEN);
            let sig = sig::Unpack::new(
                0,
                vec![],
                vec![sig::UnpackKey {
                    kind: sig::UnpackKeyKind::Sym(key_sym),
                    default: None,
                }],
                Variadic::NONE,
            );
            strand
                .with_slots_dynamic(1, async |strand, mut out| {
                    let err = unpack_from(strand, &sig, &mut out, owner, &glue, 0).unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::MissingKey);
                })
                .await;
        });
    }

    #[test]
    fn unpack_from_key_with_default_succeeds() {
        with_fixture_vm(async |strand, [mut owner_slot]| {
            make_owner(strand, &[], Slot::reborrow(&mut owner_slot));
            let owner: &Value = &owner_slot;
            let glue = Glue {
                view: FixtureGlue,
                ty: strand.vm().state::<FixtureState>().ty,
            };
            let key_sym = Sym::well_known(sym::LEN);
            // Constructed and consumed in the same expression below, immediately
            // embedded (by value) into `sig` — never a lingering, separately
            // rooted local.
            let sig = sig::Unpack::new(
                0,
                vec![],
                vec![sig::UnpackKey {
                    kind: sig::UnpackKeyKind::Sym(key_sym),
                    default: Some(Value::from_i64(strand, 42)),
                }],
                Variadic::NONE,
            );
            strand
                .with_slots_dynamic(1, async |strand, mut out| {
                    unpack_from(strand, &sig, &mut out, owner, &glue, 0).unwrap();
                    let filled = out.at(0).to_i64(strand).unwrap();
                    assert_eq!(filled, 42);
                })
                .await;
        });
    }

    #[test]
    fn index_from_errors_when_slice_start_exceeds_end() {
        with_fixture_vm(async |strand, [mut owner_slot]| {
            make_owner(strand, &[1, 2, 3, 4, 5], Slot::reborrow(&mut owner_slot));
            let owner: &Value = &owner_slot;
            let glue = Glue {
                view: FixtureGlue,
                ty: strand.vm().state::<FixtureState>().ty,
            };

            // A contiguous range whose resolved start (5) is past its resolved end
            // (2) — `Range::slice`/`index::position` each resolve fine in isolation,
            // but the pair is nonsensical as a slice. `start`/`end` are constructed
            // and consumed in the same expression, immediately embedded (by value)
            // into the `Range`, which becomes GC-scanned as soon as it's rooted in
            // `range_slot` below.
            strand
                .with_slots::<2, _>(async |strand, [mut range_slot, out_slot]| {
                    let range = range::Range::new(
                        Value::from_i64(strand, 5),
                        Value::from_i64(strand, 2),
                        Value::NIL,
                    );
                    strand.builtin_types().range.create(
                        strand,
                        range,
                        Slot::reborrow(&mut range_slot),
                    );
                    let range_value: &Value = &range_slot;
                    let err = index_from(owner, &glue, strand, range_value, out_slot).unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Index);
                })
                .await;
        });
    }
}
