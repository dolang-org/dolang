use std::{borrow::Cow, marker::PhantomData, ops::ControlFlow, ptr::NonNull};

use crate::value::fmt::Format;

use dolang_util::alias;

use crate::{
    Program,
    arg::Args,
    error::Result,
    frame::{CallFrame, Upvars},
    gc::{Annex, Collect, Gc, arena::Visit},
    object::protocol::members,
    strand::{Pinned, Strand},
    sym::{self, Sym},
    value::{Output, Slot, Value, ValueCell},
    vm::Vm,
};

use super::{
    BoundMethod,
    protocol::{
        Inspect, Protocol, Recv, abstract_call, abstract_init, instance_get_fallback,
        recv_special_mcall,
    },
};

/// Type-erased native function. The closure `F` is stored behind a `NonNull<()>`
/// with manual call/free glue, so there is only one `Protocol` impl and one `Vtbl`
/// registration shared across all native functions.
pub(crate) struct NativeFunction<'v> {
    call: for<'a, 's> unsafe fn(
        closure: NonNull<()>,
        bound: &'a Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Pinned<'v, 's, 'a, ()>,
    closure: NonNull<()>,
    free: unsafe fn(NonNull<()>),
    module: &'v str,
    name: &'v str,
    with_frame: bool,
    /// The value a bound function receives; written only at construction and by
    /// `clear`.
    pub(crate) bound: ValueCell<'v>,
    phantom: PhantomData<&'v mut &'v ()>,
}

unsafe impl<'v> crate::gc::ImmState for NativeFunction<'v> {
    const CYCLIC: bool = true;
}

impl Annex for NativeFunction<'_> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.bound.accept(visit)
    }

    fn clear(&self) {
        unsafe { self.bound.slot() }.store(Value::NIL)
    }
}

pub(crate) type NativeFunctionObj<'v> = crate::gc::Imm<NativeFunction<'v>>;

impl<'v> Drop for NativeFunction<'v> {
    fn drop(&mut self) {
        unsafe { (self.free)(self.closure) }
    }
}

/// Drop glue: runs the destructor for `F` then frees the allocation.
unsafe fn free_glue<F>(ptr: NonNull<()>) {
    unsafe { drop(alias::Box::<F>::from_non_null(ptr.cast())) }
}

/// Call glue: dereferences the erased closure pointer as `&F` and calls it,
/// wrapping the result in a pinned future.
unsafe fn native_call_glue<'v, 'a, 's, F>(
    closure: NonNull<()>,
    _bound: &Value<'v>,
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Pinned<'v, 's, 'a, ()>
where
    F: for<'b, 'r> AsyncFn(&mut Strand<'v, 'r>, Args<'v, 'b>, Slot<'v, 'b>) -> Result<'v, 'r, ()>
        + 'v,
{
    let f = unsafe { closure.cast::<F>().as_ref() };
    strand.pin_future_call(async move |strand| f(strand, args, out).await)
}

unsafe fn bound_native_call_glue<'v, 'a, 's, F>(
    closure: NonNull<()>,
    bound: &'a Value<'v>,
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Pinned<'v, 's, 'a, ()>
where
    F: for<'b, 'r> AsyncFn(
            &mut Strand<'v, 'r>,
            &Value<'v>,
            Args<'v, 'b>,
            Slot<'v, 'b>,
        ) -> Result<'v, 'r, ()>
        + 'v,
{
    let f = unsafe { closure.cast::<F>().as_ref() };
    strand.pin_future_call(async move |strand| f(strand, bound, args, out).await)
}

impl<'v> NativeFunction<'v> {
    pub(crate) fn new<F>(func: F, module: &'v str, name: &'v str) -> Self
    where
        F: for<'a, 's> AsyncFn(
                &mut Strand<'v, 's>,
                Args<'v, 'a>,
                Slot<'v, 'a>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        Self::with_frame(func, module, name, true)
    }

    pub(crate) fn without_frame<F>(func: F, module: &'v str, name: &'v str) -> Self
    where
        F: for<'a, 's> AsyncFn(
                &mut Strand<'v, 's>,
                Args<'v, 'a>,
                Slot<'v, 'a>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        Self::with_frame(func, module, name, false)
    }

    fn with_frame<F>(func: F, module: &'v str, name: &'v str, with_frame: bool) -> Self
    where
        F: for<'a, 's> AsyncFn(
                &mut Strand<'v, 's>,
                Args<'v, 'a>,
                Slot<'v, 'a>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        Self {
            closure: alias::Box::into_non_null(alias::Box::new(func)).cast(),
            call: native_call_glue::<F>,
            free: free_glue::<F>,
            module,
            name,
            with_frame,
            bound: ValueCell::default(),
            phantom: PhantomData,
        }
    }

    pub(crate) fn bound<F>(func: F) -> Self
    where
        F: for<'a, 's> AsyncFn(
                &mut Strand<'v, 's>,
                &Value<'v>,
                Args<'v, 'a>,
                Slot<'v, 'a>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        Self {
            closure: alias::Box::into_non_null(alias::Box::new(func)).cast(),
            call: bound_native_call_glue::<F>,
            free: free_glue::<F>,
            module: "<native>",
            name: "<bound>",
            with_frame: true,
            bound: ValueCell::default(),
            phantom: PhantomData,
        }
    }

    pub(crate) fn with_bound(mut self, bound: ValueCell<'v>) -> Self {
        self.bound = bound;
        self
    }

    async fn call_with_frame<'a, 's>(
        &'a self,
        bound: &'a Value<'v>,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if self.with_frame {
            Strand::async_for_native_frame(
                strand,
                Cow::Borrowed(self.module),
                Cow::Borrowed(self.name),
                None,
                async move |strand| {
                    unsafe { (self.call)(self.closure, bound, strand, args, out) }.await
                },
            )
            .await
        } else {
            unsafe { (self.call)(self.closure, bound, strand, args, out) }.await
        }
    }
}

impl<'v> Protocol<'v> for NativeFunctionObj<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().func);
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        crate::fmt!(strand, w, "{}.{}", borrow.module, borrow.name)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        crate::fmt!(strand, w, "<{}.{}>", borrow.module, borrow.name)
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        this.get()
            // Safety: `bound` is written only at construction and by `clear`, which
            // runs only once the function is unreachable
            .call_with_frame(unsafe { this.annex().bound.as_ref() }, strand, args, out)
            .await
    }
}

pub(crate) struct Function<'v> {
    pub(crate) module: Gc<'v, Program<'v>>,
    pub(crate) upvars: Option<Gc<'v, Upvars<'v>>>,
    pub(crate) id: usize,
}

unsafe impl<'v> crate::gc::ImmState for Function<'v> {
    const CYCLIC: bool = true;
}

impl<'v> crate::gc::Annex for Function<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.module.accept(visit)?;
        if let Some(upvars) = self.upvars.as_ref() {
            upvars.accept(visit)?;
        }
        ControlFlow::Continue(())
    }
}

pub(crate) type FunctionObj<'v> = crate::gc::Imm<Function<'v>>;

impl<'v> Function<'v> {
    pub(crate) fn new(
        module: Gc<'v, Program<'v>>,
        upvars: Option<Gc<'v, Upvars<'v>>>,
        id: usize,
    ) -> Self {
        Self { module, upvars, id }
    }

    async fn call_with_frame<'a, 's>(
        &'a self,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unsafe {
            let frame = CallFrame::new(
                self.module.clone(),
                self.id,
                self.upvars.clone(),
                Some(strand.fp),
            );
            frame.unpack_unchecked(strand, args)?;
            strand.vm().run(strand, &frame, out).await
        }
    }
}

impl<'v> Protocol<'v> for FunctionObj<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().func)
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        this.get().call_with_frame(strand, args, out).await
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        let program = &borrow.module;
        let module = program.module_name();
        let name = program
            .funcdebugs()
            .get(borrow.id)
            .map(|d| &program.debug_strtab()[d.name.clone()]);
        match (module, name) {
            (None, None) => crate::fmt!(strand, w, "?.?"),
            (None, Some(name)) => crate::fmt!(strand, w, "{name}"),
            (Some(module), None) if borrow.id == 0 => crate::fmt!(strand, w, "{module}"),
            (Some(module), None) => crate::fmt!(strand, w, "{module}.?"),
            (Some(module), Some(name)) => crate::fmt!(strand, w, "{module}.{name}"),
        }
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<")?;
        Self::op_display(this, strand, w)?;
        crate::fmt!(strand, w, ">")
    }
}

// ── Function Class ──────────────────────────────────────────────

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
        crate::fmt!(strand, w, "<type std.Func>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            is_final: false,
            members: members![],
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
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
            sym::INIT_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => abstract_init(strand, &this, args),
            _ => recv_special_mcall(strand, &this, method, args, out).await,
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "Func"))
    }
}
