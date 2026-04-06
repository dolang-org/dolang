pub(crate) mod arg;
pub(crate) mod array;
pub(crate) mod backtrace;
pub(crate) mod bin;
pub(crate) mod channel;
pub(crate) mod class;
pub(crate) mod dict;
pub(crate) mod error;
pub(crate) mod float;
pub(crate) mod function;
pub(crate) mod index;
pub(crate) mod int;
pub(crate) mod iter;
pub(crate) mod kv;
pub(crate) mod module;
pub mod native;
pub(crate) mod protocol;
pub(crate) mod range;
pub(crate) mod record;
pub(crate) mod set;
pub(crate) mod str;
pub(crate) mod strand;
pub(crate) mod sym;
pub(crate) mod tuple;
pub(crate) mod types;

use std::{fmt, ops::ControlFlow, ptr::NonNull};

use crate::{
    arg::Args,
    error::{ErrorKind, Result, ResultExt},
    gc::{self, Collect, arena::Visit},
    object::protocol::Recv,
    strand::Strand,
    sym::Sym,
    value::{Input, Slot, Value},
    vm::Vm,
};

use dolang_util::alias;
use protocol::{GcObj, Protocol, TypeHandle};

type VtblEntry = (NonNull<()>, unsafe fn(NonNull<()>));

pub(crate) struct TypeTable<'v> {
    entries: Vec<VtblEntry>,
    _phantom: std::marker::PhantomData<&'v ()>,
}

impl<'v> TypeTable<'v> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Register any vtbl-like value and get a `'v`-lived reference to it.
    ///
    /// The value must be `repr(C)` with `arena::Vtbl` as its (transitive) first field so that
    /// the GC header's vtbl pointer remains valid.
    #[inline(never)]
    pub(crate) fn register<V: 'v>(&mut self, vtbl: V) -> NonNull<V> {
        let ptr = alias::Box::into_non_null(alias::Box::new(vtbl));
        self.entries.push((ptr.cast(), |ptr| unsafe {
            drop(alias::Box::<V>::from_non_null(ptr.cast()))
        }));
        // Safety: the allocation lives until TypeTable is dropped (within 'v).
        ptr
    }

    /// Register a protocol vtbl for type `T` and return a type-safe handle.
    pub(crate) fn register_type_handle<T>(&mut self) -> TypeHandle<'v, T>
    where
        T: ?Sized + gc::Boxable<protocol::Header> + Protocol<'v>,
    {
        let registered = self.register(protocol::Vtbl::new::<T>());
        // Safety: the vtbl was created for T.
        unsafe { TypeHandle::new(registered) }
    }
}

impl<'v> Drop for TypeTable<'v> {
    fn drop(&mut self) {
        for (ptr, free) in self.entries.drain(..) {
            unsafe { free(ptr) }
        }
    }
}

pub(crate) struct BuiltinTypes<'v> {
    pub(crate) arg_iter: TypeHandle<'v, arg::ArgIter<'v>>,
    pub(crate) pos_arg_iter: TypeHandle<'v, arg::PosArgIter<'v>>,
    pub(crate) array_iter: TypeHandle<'v, array::Iter<'v>>,
    pub(crate) array_sink: TypeHandle<'v, array::Sink<'v>>,
    pub(crate) array_pairs: TypeHandle<'v, array::Pairs<'v>>,
    pub(crate) array: TypeHandle<'v, array::Array<'v>>,
    pub(crate) backtrace: TypeHandle<'v, backtrace::Backtrace<'v>>,
    pub(crate) backtrace_iter: TypeHandle<'v, backtrace::Iter<'v>>,
    pub(crate) backtrace_frame: TypeHandle<'v, backtrace::Frame<'v>>,
    pub(crate) bin: TypeHandle<'v, [u8]>,
    pub(crate) bin_split: TypeHandle<'v, bin::Split<'v>>,
    pub(crate) bin_class: TypeHandle<'v, bin::Class>,
    pub(crate) bound_method: TypeHandle<'v, BoundMethod<'v>>,
    pub(crate) error: TypeHandle<'v, error::Boxed<'v>>,
    pub(crate) f64: TypeHandle<'v, f64>,
    pub(crate) function: TypeHandle<'v, function::Function<'v>>,
    pub(crate) native_function: TypeHandle<'v, function::NativeFunction<'v>>,
    pub(crate) i64: TypeHandle<'v, i64>,
    pub(crate) dict_iter: TypeHandle<'v, dict::Iter<'v>>,
    pub(crate) dict_keys: TypeHandle<'v, kv::Keys<'v, dict::Dict<'v>>>,
    pub(crate) dict_values: TypeHandle<'v, kv::Values<'v, dict::Dict<'v>>>,
    pub(crate) dict_key_values: TypeHandle<'v, kv::KeyValues<'v, dict::Dict<'v>>>,
    pub(crate) dict_unpack: TypeHandle<'v, dict::Unpack<'v>>,
    pub(crate) dict: TypeHandle<'v, dict::Dict<'v>>,
    pub(crate) set_iter: TypeHandle<'v, set::Iter<'v>>,
    pub(crate) set: TypeHandle<'v, set::Set<'v>>,
    pub(crate) strand_handle: TypeHandle<'v, strand::Handle<'v>>,
    pub(crate) module: TypeHandle<'v, module::Module<'v>>,
    pub(crate) module_iter: TypeHandle<'v, module::Iter<'v>>,
    pub(crate) native_module: TypeHandle<'v, module::Native<'v>>,
    pub(crate) namespace: TypeHandle<'v, module::Namespace<'v>>,
    pub(crate) str: TypeHandle<'v, str>,
    pub(crate) str_split: TypeHandle<'v, str::Split<'v>>,
    pub(crate) sym: TypeHandle<'v, sym::SymObj>,
    pub(crate) verbatim_f64: TypeHandle<'v, float::Verbatim>,
    pub(crate) verbatim_i64: TypeHandle<'v, int::Verbatim>,
    pub(crate) range: TypeHandle<'v, range::Range<'v>>,
    pub(crate) range_iter: TypeHandle<'v, range::Iter<'v>>,
    pub(crate) record: TypeHandle<'v, record::Record<'v>>,
    pub(crate) record_class: TypeHandle<'v, record::Class>,
    pub(crate) record_iter: TypeHandle<'v, record::Iter<'v>>,
    pub(crate) record_keys: TypeHandle<'v, kv::Keys<'v, record::Record<'v>>>,
    pub(crate) record_values: TypeHandle<'v, kv::Values<'v, record::Record<'v>>>,
    pub(crate) record_key_values: TypeHandle<'v, kv::KeyValues<'v, record::Record<'v>>>,
    pub(crate) record_unpack: TypeHandle<'v, record::Unpack<'v>>,
    pub(crate) channel_recv: TypeHandle<'v, channel::Receiver<'v>>,
    pub(crate) channel_send: TypeHandle<'v, channel::Sender<'v>>,
    pub(crate) class_object: TypeHandle<'v, class::ClassObject<'v>>,
    pub(crate) class_instance: TypeHandle<'v, class::ClassInstance<'v>>,
    pub(crate) tuple: TypeHandle<'v, [Value<'v>]>,
    pub(crate) tuple_iter: TypeHandle<'v, tuple::Iter<'v>>,
    pub(crate) tuple_pairs: TypeHandle<'v, tuple::Pairs<'v>>,

    // Builtin type object vtbls
    pub(crate) value_type: TypeHandle<'v, types::Value>,
    pub(crate) type_type: TypeHandle<'v, types::Type>,
    pub(crate) iterable: TypeHandle<'v, iter::Iterable>,
    pub(crate) sinkable: TypeHandle<'v, iter::Sinkable>,
    pub(crate) input_iter: TypeHandle<'v, iter::Iter>,
    pub(crate) output_iter: TypeHandle<'v, iter::Sink>,
    pub(crate) null: TypeHandle<'v, iter::Null>,
    pub(crate) chain_iter: TypeHandle<'v, iter::Chain<'v>>,
    pub(crate) zip_iter: TypeHandle<'v, iter::Zip<'v>>,
    pub(crate) map_iter: TypeHandle<'v, iter::Map<'v>>,
    pub(crate) filter_iter: TypeHandle<'v, iter::Filter<'v>>,
    pub(crate) map_type: TypeHandle<'v, iter::MapType>,
    pub(crate) filter_type: TypeHandle<'v, iter::FilterType>,
    pub(crate) nil_type: TypeHandle<'v, types::NilType>,
    pub(crate) int_type: TypeHandle<'v, int::Int>,
    pub(crate) float_type: TypeHandle<'v, float::Float>,
    pub(crate) bool_type: TypeHandle<'v, types::Bool>,
    pub(crate) str_type: TypeHandle<'v, str::Type>,
    pub(crate) sym_type: TypeHandle<'v, sym::Type>,
    pub(crate) array_type: TypeHandle<'v, array::Type>,
    pub(crate) dict_type: TypeHandle<'v, dict::Type>,
    pub(crate) set_type: TypeHandle<'v, set::Type>,
    pub(crate) tuple_type: TypeHandle<'v, tuple::Type>,
    pub(crate) func_type: TypeHandle<'v, function::Type>,
    pub(crate) range_type: TypeHandle<'v, range::Type>,
    pub(crate) backtrace_type: TypeHandle<'v, backtrace::Type>,
    pub(crate) error_type: TypeHandle<'v, error::Type>,
    pub(crate) error_variant_type: TypeHandle<'v, error::VariantType>,
    pub(crate) module_type: TypeHandle<'v, module::Type>,
    pub(crate) strand_type: TypeHandle<'v, strand::Type>,
    pub(crate) args_type: TypeHandle<'v, types::ArgsType>,
}

impl<'v> BuiltinTypes<'v> {
    pub(crate) fn new(types: &mut TypeTable<'v>) -> Self {
        Self {
            arg_iter: types.register_type_handle(),
            pos_arg_iter: types.register_type_handle(),
            array_iter: types.register_type_handle(),
            array_sink: types.register_type_handle(),
            array_pairs: types.register_type_handle(),
            array: types.register_type_handle(),
            backtrace: types.register_type_handle(),
            backtrace_iter: types.register_type_handle(),
            backtrace_frame: types.register_type_handle(),
            bin: types.register_type_handle(),
            bin_class: types.register_type_handle(),
            bin_split: types.register_type_handle(),
            bound_method: types.register_type_handle(),
            channel_recv: types.register_type_handle(),
            channel_send: types.register_type_handle(),
            class_object: types.register_type_handle(),
            class_instance: types.register_type_handle(),
            dict_iter: types.register_type_handle(),
            dict_keys: types.register_type_handle(),
            dict_values: types.register_type_handle(),
            dict_key_values: types.register_type_handle(),
            dict: types.register_type_handle(),
            dict_unpack: types.register_type_handle(),
            set_iter: types.register_type_handle(),
            set: types.register_type_handle(),
            strand_handle: types.register_type_handle(),
            error: types.register_type_handle(),
            f64: types.register_type_handle(),
            function: types.register_type_handle(),
            native_function: types.register_type_handle(),
            i64: types.register_type_handle(),
            module_iter: types.register_type_handle(),
            module: types.register_type_handle(),
            native_module: types.register_type_handle(),
            namespace: types.register_type_handle(),
            range_iter: types.register_type_handle(),
            range: types.register_type_handle(),
            record: types.register_type_handle(),
            record_class: types.register_type_handle(),
            record_iter: types.register_type_handle(),
            record_keys: types.register_type_handle(),
            record_values: types.register_type_handle(),
            record_key_values: types.register_type_handle(),
            record_unpack: types.register_type_handle(),
            str_split: types.register_type_handle(),
            str: types.register_type_handle(),
            sym: types.register_type_handle(),
            tuple_iter: types.register_type_handle(),
            tuple_pairs: types.register_type_handle(),
            tuple: types.register_type_handle(),
            verbatim_f64: types.register_type_handle(),
            verbatim_i64: types.register_type_handle(),
            value_type: types.register_type_handle(),
            type_type: types.register_type_handle(),
            iterable: types.register_type_handle(),
            sinkable: types.register_type_handle(),
            int_type: types.register_type_handle(),
            float_type: types.register_type_handle(),
            bool_type: types.register_type_handle(),
            str_type: types.register_type_handle(),
            sym_type: types.register_type_handle(),
            array_type: types.register_type_handle(),
            dict_type: types.register_type_handle(),
            set_type: types.register_type_handle(),
            tuple_type: types.register_type_handle(),
            func_type: types.register_type_handle(),
            range_type: types.register_type_handle(),
            backtrace_type: types.register_type_handle(),
            error_type: types.register_type_handle(),
            error_variant_type: types.register_type_handle(),
            module_type: types.register_type_handle(),
            strand_type: types.register_type_handle(),
            args_type: types.register_type_handle(),
            input_iter: types.register_type_handle(),
            output_iter: types.register_type_handle(),
            null: types.register_type_handle(),
            chain_iter: types.register_type_handle(),
            zip_iter: types.register_type_handle(),
            map_iter: types.register_type_handle(),
            filter_iter: types.register_type_handle(),
            map_type: types.register_type_handle(),
            filter_type: types.register_type_handle(),
            nil_type: types.register_type_handle(),
        }
    }
}

pub(crate) struct Singletons<'v> {
    // Singleton type objects (GC-allocated)
    pub(crate) value: Value<'v>,
    pub(crate) type_obj: Value<'v>,
    pub(crate) iterable: Value<'v>,
    pub(crate) sinkable: Value<'v>,
    pub(crate) input_iter: Value<'v>,
    pub(crate) output_iter: Value<'v>,
    pub(crate) nulliter: Value<'v>,
    pub(crate) map_iter: Value<'v>,
    pub(crate) filter_iter: Value<'v>,
    pub(crate) nil: Value<'v>,
    pub(crate) int: Value<'v>,
    pub(crate) float: Value<'v>,
    pub(crate) bool: Value<'v>,
    pub(crate) str: Value<'v>,
    pub(crate) sym: Value<'v>,
    pub(crate) array: Value<'v>,
    pub(crate) dict: Value<'v>,
    pub(crate) set: Value<'v>,
    pub(crate) record: Value<'v>,
    pub(crate) tuple: Value<'v>,
    pub(crate) bin: Value<'v>,
    pub(crate) func: Value<'v>,
    pub(crate) range: Value<'v>,
    pub(crate) backtrace: Value<'v>,
    pub(crate) error: Value<'v>,
    pub(crate) module: Value<'v>,
    pub(crate) strand: Value<'v>,
    pub(crate) args: Value<'v>,

    // Error variant classes
    pub(crate) error_unsupported: Value<'v>,
    pub(crate) error_immutable: Value<'v>,
    pub(crate) error_concurrency: Value<'v>,
    pub(crate) error_type: Value<'v>,
    pub(crate) error_value: Value<'v>,
    pub(crate) error_state: Value<'v>,
    pub(crate) error_index: Value<'v>,
    pub(crate) error_field: Value<'v>,
    pub(crate) error_unexpected_pos: Value<'v>,
    pub(crate) error_unexpected_key: Value<'v>,
    pub(crate) error_missing_pos: Value<'v>,
    pub(crate) error_missing_key: Value<'v>,
    pub(crate) error_overflow: Value<'v>,
    pub(crate) error_zerodiv: Value<'v>,
    pub(crate) error_sink_stop: Value<'v>,
    pub(crate) error_iter_stop: Value<'v>,
    pub(crate) error_cyclic_import: Value<'v>,
    pub(crate) error_import: Value<'v>,
    pub(crate) error_compile: Value<'v>,
    pub(crate) error_bytecode: Value<'v>,
    pub(crate) error_runtime: Value<'v>,
    pub(crate) error_interrupt: Value<'v>,
    pub(crate) error_canceled: Value<'v>,
}

impl<'v> Singletons<'v> {
    pub(crate) fn new(
        arena: &crate::gc::arena::Arena<'v>,
        builtin_types: &BuiltinTypes<'v>,
    ) -> Self {
        macro_rules! v {
            ($vtbl:expr, $data:expr) => {
                Value::from_object(GcObj::new(arena, $vtbl, $data))
            };
        }

        Self {
            value: v!(builtin_types.value_type, types::Value),
            type_obj: v!(builtin_types.type_type, types::Type),
            iterable: v!(builtin_types.iterable, iter::Iterable),
            sinkable: v!(builtin_types.sinkable, iter::Sinkable),
            input_iter: v!(builtin_types.input_iter, iter::Iter),
            output_iter: v!(builtin_types.output_iter, iter::Sink),
            nulliter: v!(builtin_types.null, iter::Null),
            map_iter: v!(builtin_types.map_type, iter::MapType),
            filter_iter: v!(builtin_types.filter_type, iter::FilterType),
            nil: v!(builtin_types.nil_type, types::NilType),
            int: v!(builtin_types.int_type, int::Int),
            float: v!(builtin_types.float_type, float::Float),
            bool: v!(builtin_types.bool_type, types::Bool),
            str: v!(builtin_types.str_type, str::Type),
            sym: v!(builtin_types.sym_type, sym::Type),
            array: v!(builtin_types.array_type, array::Type),
            dict: v!(builtin_types.dict_type, dict::Type),
            set: v!(builtin_types.set_type, set::Type),
            record: v!(builtin_types.record_class, record::Class),
            tuple: v!(builtin_types.tuple_type, tuple::Type),
            bin: v!(builtin_types.bin_class, bin::Class),
            func: v!(builtin_types.func_type, function::Type),
            range: v!(builtin_types.range_type, range::Type),
            backtrace: v!(builtin_types.backtrace_type, backtrace::Type),
            error: v!(builtin_types.error_type, error::Type),
            module: v!(builtin_types.module_type, module::Type),
            strand: v!(builtin_types.strand_type, strand::Type),
            args: v!(builtin_types.args_type, types::ArgsType),
            error_unsupported: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Unsupported)
            ),
            error_immutable: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Immutable)
            ),
            error_concurrency: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Concurrency)
            ),
            error_type: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Type)
            ),
            error_value: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Value)
            ),
            error_state: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::State)
            ),
            error_index: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Index)
            ),
            error_field: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Field)
            ),
            error_unexpected_pos: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::UnexpectedPos)
            ),
            error_unexpected_key: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::UnexpectedKey)
            ),
            error_missing_pos: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::MissingPos)
            ),
            error_missing_key: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::MissingKey)
            ),
            error_overflow: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Overflow)
            ),
            error_zerodiv: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::ZeroDiv)
            ),
            error_sink_stop: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::SinkStop)
            ),
            error_iter_stop: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::IterStop)
            ),
            error_cyclic_import: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::CyclicImport)
            ),
            error_import: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Import)
            ),
            error_compile: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Compile)
            ),
            error_bytecode: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Bytecode)
            ),
            error_runtime: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Runtime)
            ),
            error_interrupt: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Interrupt)
            ),
            error_canceled: v!(
                builtin_types.error_variant_type,
                error::VariantType(ErrorKind::Canceled)
            ),
        }
    }
}

pub(crate) struct BoundMethod<'v> {
    rcvr: Value<'v>,
    method: protocol::GcObj<'v, sym::SymObj>,
}

unsafe impl<'v> Collect for BoundMethod<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.rcvr.accept(visit)?;
        self.method.accept(visit)
    }

    fn clear(&mut self) {
        self.rcvr = Value::NIL;
    }
}

impl<'v> Protocol<'v> for BoundMethod<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().func.dup())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let me = this.receiver.get();
        write!(w, "<bound method: {} receiver: ", me.method.name).into_do(strand)?;
        me.rcvr.op_debug(strand, w)?;
        write!(w, ">").into_do(strand)
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.receiver.get();
        let method = unsafe { Sym::from_tag(me.method.tag) };
        me.rcvr.op_mcall(strand, method, args, out).await
    }
}

impl<'v> BoundMethod<'v> {
    pub(crate) fn create(
        vm: &Vm<'v>,
        rcvr: impl Input<'v>,
        method: Sym<'v, '_>,
        mut out: Slot<'v, '_>,
    ) {
        out.store(Value::from_object(protocol::GcObj::new(
            vm.arena(),
            vm.builtin_types().bound_method,
            BoundMethod {
                rcvr: Value::from_input(vm, rcvr),
                method: vm.sym_obj(method),
            },
        )));
    }
}
