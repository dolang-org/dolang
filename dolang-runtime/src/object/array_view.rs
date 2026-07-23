//! Lazy array-like projections over native objects.

use std::{marker::PhantomData, ops::ControlFlow, ptr};

use dolang_bytecode::Variadic;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        array, index, iter,
        native::{Instance, Object},
        protocol::{Protocol, Recv, Spread, SpreadContext},
        range,
    },
    sig::{Unpack, UnpackKeyKind},
    strand::Strand,
    sym,
    sym::Sym,
    value::{Input, InputBy, Output, Slot, Slots, TypeObject, Value, private::Sealed},
    vm::Vm,
};

/// Implements a lazy array-like projection over a native object.
///
/// Implement this trait on a marker type. Different marker types may expose
/// different views of the same [`Object`].
pub trait ArrayLike<'v>: 'v {
    type Object: Object<'v>;

    const MODULE: &'v str;
    const NAME: &'v str;

    fn len(this: Instance<'v, '_, Self::Object>, strand: &mut Strand<'v, '_>) -> usize;

    fn get<'a, 's>(
        this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;

    fn set<'a, 's>(
        _this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        _index: usize,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }
}

/// Input wrapper that creates a lazy array view of a native object.
pub struct ArrayView<'v, 'a, I: ArrayLike<'v>> {
    owner: Instance<'v, 'a, I::Object>,
    marker: PhantomData<I>,
}

impl<'v, 'a, I: ArrayLike<'v>> ArrayView<'v, 'a, I> {
    pub fn new(owner: Instance<'v, 'a, I::Object>) -> Self {
        Self {
            owner,
            marker: PhantomData,
        }
    }
}

impl<'v, I: ArrayLike<'v>> Input<'v> for ArrayView<'v, '_, I> {
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: Sealed) -> InputBy<'v, 'a> {
        let owner = Value::from_input(vm, self.owner);
        let value = crate::object::protocol::GcObj::new(
            vm.arena(),
            vm.builtin_types().array_view,
            View {
                owner,
                glue: Box::new(Glue::<I>(PhantomData)),
            },
        );
        InputBy::Value(Value::from_object(value), None)
    }
}

trait ArrayViewGlue<'v>: 'v {
    fn clone_box(&self) -> Box<dyn ArrayViewGlue<'v> + 'v>;
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
}

struct Glue<I>(PhantomData<I>);

impl<'v, I: ArrayLike<'v>> ArrayViewGlue<'v> for Glue<I> {
    fn clone_box(&self) -> Box<dyn ArrayViewGlue<'v> + 'v> {
        Box::new(Self(PhantomData))
    }
    fn module(&self) -> &'v str {
        I::MODULE
    }
    fn name(&self) -> &'v str {
        I::NAME
    }
    fn len(&self, owner: &Value<'v>, strand: &mut Strand<'v, '_>) -> usize {
        // SAFETY: ArrayView::input_take pairs this glue with an I::Object value.
        I::len(unsafe { Instance::from_value_unchecked(owner) }, strand)
    }
    fn get<'a, 's>(
        &self,
        owner: &Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // SAFETY: ArrayView::input_take pairs this glue with an I::Object value.
        I::get(
            unsafe { Instance::from_value_unchecked(owner) },
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
        // SAFETY: ArrayView::input_take pairs this glue with an I::Object value.
        I::set(
            unsafe { Instance::from_value_unchecked(owner) },
            strand,
            index,
            value,
        )
    }
}

pub(crate) struct View<'v> {
    owner: Value<'v>,
    glue: Box<dyn ArrayViewGlue<'v> + 'v>,
}

pub(crate) struct Iter<'v> {
    owner: Value<'v>,
    glue: Box<dyn ArrayViewGlue<'v> + 'v>,
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
        self.owner.accept(visit)
    }
    fn clear(&mut self) {
        self.owner.clear()
    }
}

fn debug<'v, 's>(
    module: &str,
    name: &str,
    strand: &mut Strand<'v, 's>,
    w: &mut dyn crate::value::Format<'v>,
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
        w: &mut dyn crate::value::Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        debug(borrow.glue.module(), borrow.glue.name(), strand, w)
    }
    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let borrow = this.borrow(strand).expect("conflicting borrow");
        borrow.glue.len(&borrow.owner, strand) != 0
    }
    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let equal = other
            .downcast_ref(strand.builtin_types().array_view)
            .is_some_and(|other| {
                ptr::eq(this.as_header().as_ptr(), other.into_raw().cast().as_ptr())
            });
        Ok(Value::from_bool(equal))
    }
    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let len = borrow.glue.len(&borrow.owner, strand);
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
                borrow
                    .glue
                    .get(&borrow.owner, strand, index, Slot::new(&mut value))?;
                array.inner.push(value);
            }
            strand.builtin_types().array.create(strand, array, out);
            return Ok(());
        }
        let index = normalize(strand, index, len)?;
        borrow.glue.get(&borrow.owner, strand, index, out)
    }
    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let len = borrow.glue.len(&borrow.owner, strand);
        let index = normalize(strand, &index, len)?;
        borrow.glue.set(&borrow.owner, strand, index, value)
    }
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::LEN {
            let borrow = this.borrow(strand)?;
            let len = borrow.glue.len(&borrow.owner, strand);
            Output::set(strand, out, len);
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
            return Err(Error::type_error(
                strand,
                "array view len is a field, not a method",
            ));
        }
        iter::iterable_mcall(strand, &this, method, args, out).await
    }
    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        strand.builtin_types().array_view_iter.create(
            strand,
            Iter {
                owner: borrow.owner.dup(),
                glue: borrow.glue.clone_box(),
                index: 0,
            },
            out,
        );
        Ok(())
    }
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let len = borrow.glue.len(&borrow.owner, strand);
        let mut value = Value::NIL;
        for index in 0..len {
            borrow
                .glue
                .get(&borrow.owner, strand, index, Slot::new(&mut value))?;
            sink.positional(strand, Slot::new(&mut value))?;
        }
        Ok(())
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let consumed = unpack_from(strand, sig, &mut out, &borrow.owner, &*borrow.glue, 0)?;
        if sig.variadic == Variadic::Capture {
            strand.builtin_types().array_view_iter.create(
                strand,
                Iter {
                    owner: borrow.owner.dup(),
                    glue: borrow.glue.clone_box(),
                    index: consumed,
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
        Output::set(strand, out, TypeObject::Value)
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn crate::value::Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        debug(borrow.glue.module(), borrow.glue.name(), strand, w)
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
        let mut borrow = this.borrow_mut(strand)?;
        if borrow.index >= borrow.glue.len(&borrow.owner, strand) {
            return Ok(false);
        }
        let index = borrow.index;
        borrow.glue.get(&borrow.owner, strand, index, out)?;
        borrow.index += 1;
        Ok(true)
    }
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let consumed = unpack_from(
            strand,
            sig,
            &mut out,
            &borrow.owner,
            &*borrow.glue,
            borrow.index,
        )?;
        borrow.index += consumed;
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
    if pos_count < len && sig.variadic == Variadic::None {
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
