use std::hash::Hasher;

use crate::{
    error::Error,
    object::{
        array::Array, class, dict::Dict, float, int, pack, record::Record, structure::Struct, tuple,
    },
    unpack,
    value::StrEmbryo,
    vm::{AllocExt, Builder},
    with_str,
};

pub(crate) mod fmt;
pub(crate) mod io;

pub(crate) mod inspect;
mod math;
mod strand;

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let base = builder.sym("base");
    let fmt = fmt::register(builder);
    let member_scopes = class::register_member_scopes(builder);
    let member_scopes = builder.register_state::<class::MemberScopeTag>(member_scopes);
    let bc = builder.singletons();

    // Core types
    let value = &bc.value;
    let type_obj = &bc.type_obj;
    let int = &bc.int;
    let float = &bc.float;
    let bool = &bc.bool;
    let nil = &bc.nil;
    let str = &bc.str;
    let strbuf = &bc.strbuf;
    let sym = &bc.sym;
    let array = &bc.array;
    let dict = &bc.dict;
    let set = &bc.set;
    let tuple = &bc.tuple;
    let func = &bc.func;
    let range = &bc.range;
    let module = &bc.module;
    let pack_class = &bc.pack;
    let record = &bc.record;
    let structure = &bc.structure;
    let bin = &bc.bin;
    let binbuf = &bc.binbuf;

    // Iterator protocol types (from former std.iter)
    let iter_type = &bc.input_iter;
    let sink_type = &bc.output_iter;
    let iterable_type = &bc.iterable;
    let sinkable_type = &bc.sinkable;
    let getter = &bc.getter;
    let setter = &bc.setter;
    let null = &bc.null;
    let null_type = &bc.null_type;
    let error_sink_stop = &bc.error_sink_stop;
    let error_iter_stop = &bc.error_iter_stop;

    // Error types (from former std.error)
    let error = &bc.error;
    let error_unsupported = &bc.error_unsupported;
    let error_immutable = &bc.error_immutable;
    let error_concurrency = &bc.error_concurrency;
    let error_type = &bc.error_type;
    let error_value = &bc.error_value;
    let error_data = &bc.error_data;
    let error_shape = &bc.error_shape;
    let error_shapeless = &bc.error_shapeless;
    let error_missing_item = &bc.error_missing_item;
    let error_unexpected_item = &bc.error_unexpected_item;
    let error_state = &bc.error_state;
    let error_index = &bc.error_index;
    let error_field = &bc.error_field;
    let error_unexpected_pos = &bc.error_unexpected_pos;
    let error_unexpected_key = &bc.error_unexpected_key;
    let error_missing_pos = &bc.error_missing_pos;
    let error_missing_key = &bc.error_missing_key;
    let error_overflow = &bc.error_overflow;
    let error_zerodiv = &bc.error_zerodiv;
    let error_cyclic_import = &bc.error_cyclic_import;
    let error_import = &bc.error_import;
    let error_compile = &bc.error_compile;
    let error_runtime = &bc.error_runtime;
    let error_abort = &bc.error_abort;
    let error_canceled = &bc.error_canceled;
    let error_timed_out = &bc.error_timed_out;

    builder
        .module("std")
        // Core types
        .value("Value", value)
        .value("Type", type_obj)
        .value("Int", int)
        .value("Float", float)
        .value("Bool", bool)
        .value("Nil", nil)
        .value("Str", str)
        .value("StrBuf", strbuf)
        .value("Sym", sym)
        .value("Array", array)
        // A type-only split of `Array`; the same class at runtime
        .value("BaseArray", array)
        .value("Dict", dict)
        // A type-only split of `Dict` for the type checker; the same class at runtime
        .value("BaseDict", dict)
        .value("Set", set)
        .value("Tuple", tuple)
        .value("Func", func)
        .value("Range", range)
        .value("getter", getter)
        .value("setter", setter)
        .value("class", member_scopes.class)
        .value("static", member_scopes.statik)
        .function("fields", async move |strand, args, out| {
            unpack!(strand, args => value);
            crate::object::field_proxy::create(strand, &value, out)
        })
        .function("final", async move |strand, args, out| {
            unpack!(strand, args => class);
            class::make_final(strand, &class)?;
            strand.store(out, class);
            Ok(())
        })
        .function("mixable", async move |strand, args, out| {
            unpack!(strand, args => class);
            class::make_mixable(strand, &class)?;
            strand.store(out, class);
            Ok(())
        })
        .value("Module", module)
        .value("Pack", pack_class)
        .value("Record", record)
        // A type-only split of `Record`; the same class at runtime
        .value("BaseRecord", record)
        .value("Struct", structure)
        // A type-only split of `Struct`; the same class at runtime
        .value("BaseStruct", structure)
        .value("Bin", bin)
        .value("BinBuf", binbuf)
        .value("FmtSpec", fmt.types.spec)
        .value("FmtValue", fmt.types.value)
        .value("FmtParam", fmt.types.param)
        .value("Fmt", fmt.types.fmt)
        // Iterator protocol types
        .value("Iterable", iterable_type)
        .value("Sinkable", sinkable_type)
        .value("Iter", iter_type)
        .value("Sink", sink_type)
        .value("Null", null_type)
        .value("null", null)
        .value("SinkStop", error_sink_stop)
        .value("IterStop", error_iter_stop)
        // Error types
        .value("Error", error)
        .value("UnsupportedError", error_unsupported)
        .value("ImmutableError", error_immutable)
        .value("ConcurrencyError", error_concurrency)
        .value("TypeError", error_type)
        .value("ValueError", error_value)
        .value("DataError", error_data)
        .value("ShapeError", error_shape)
        .value("ShapelessError", error_shapeless)
        .value("MissingItemError", error_missing_item)
        .value("UnexpectedItemError", error_unexpected_item)
        .value("StateError", error_state)
        .value("IndexError", error_index)
        .value("FieldError", error_field)
        .value("UnexpectedPosError", error_unexpected_pos)
        .value("UnexpectedKeyError", error_unexpected_key)
        .value("MissingPosError", error_missing_pos)
        .value("MissingKeyError", error_missing_key)
        .value("OverflowError", error_overflow)
        .value("ZeroDivError", error_zerodiv)
        .value("CyclicImportError", error_cyclic_import)
        .value("ImportError", error_import)
        .value("CompileError", error_compile)
        .value("RuntimeError", error_runtime)
        .value("AbortError", error_abort)
        .value("CanceledError", error_canceled)
        .value("TimedOutError", error_timed_out)
        // Collection factories
        .function("array", async move |strand, args, out| {
            Array::from_args(strand, args, out)
        })
        .function("dict", async move |strand, args, out| {
            Dict::from_args(strand, args, out)
        })
        .function("tuple", async move |strand, args, out| {
            tuple::from_args(strand, args, out)
        })
        .function("pack", async move |strand, args, out| {
            pack::Pack::from_args(strand, args, out);
            Ok(())
        })
        .function("record", async move |strand, args, out| {
            Record::from_args(strand, args, out);
            Ok(())
        })
        .function("struct", async move |strand, args, out| {
            Struct::from_args(strand, args, out)
        })
        .function("type", async move |strand, args, out| {
            unpack!(strand, args => obj, ?ty);
            if let Some(ty) = ty {
                let result = obj.is_instance_of(strand, &ty);
                strand.store(out, result);
            } else {
                obj.op_type(strand, out);
            }
            Ok(())
        })
        .function("int", async move |strand, args, out| {
            unpack!(strand, args => value, ?:base);
            let base = base
                .map(|base| {
                    let base = base.to_int(strand)?;
                    if !(2..=36).contains(&base) {
                        return Err(Error::value(strand, "int: base must be between 2 and 36"));
                    }
                    Ok(base as u32)
                })
                .transpose()?;
            let value = int::coerce(&value, strand, base)?;
            strand.store(out, value);
            Ok(())
        })
        .function("float", async move |strand, args, out| {
            unpack!(strand, args => value);
            let value = float::coerce(&value, strand)?;
            strand.store(out, value);
            Ok(())
        })
        .function("bool", async move |strand, args, out| {
            unpack!(strand, args => value);
            let value = value.to_bool(strand)?;
            strand.store(out, value);
            Ok(())
        })
        .function("str", async move |strand, args, out| {
            unpack!(strand, args => value);
            let mut format = StrEmbryo::new();
            value.display(strand, &mut format)?;
            format.finish(strand, out);
            Ok(())
        })
        .function("sym", async move |strand, args, out| {
            unpack!(strand, args => value);
            if value.as_sym(strand).is_some() {
                strand.store(out, value);
                Ok(())
            } else if let Some(value) = value.as_str(strand) {
                with_str!(strand, value, |value| strand.intern_sym(value, out));
                Ok(())
            } else {
                Err(Error::type_error(strand, "sym: expected Str or Sym"))
            }
        })
        // Core functions
        .function("verbatim", async move |strand, args, out| {
            unpack!(strand, args => value);
            let mut format = StrEmbryo::new();
            value.verbatim(strand, &mut format)?;
            format.finish(strand, out);
            Ok(())
        })
        .function("dbg", async move |strand, args, out| {
            unpack!(strand, args => value);
            let mut format = StrEmbryo::new();
            value.debug(strand, &mut format)?;
            format.finish(strand, out);
            Ok(())
        })
        .function("hash", async move |strand, args, out| {
            unpack!(strand, args => *values);
            let mut hasher = strand.vm().hasher();
            for (i, slot) in values.enumerate() {
                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_trap()?;
                }
                slot.hash(strand, &mut hasher)?;
            }
            strand.store(out, hasher.finish());
            Ok(())
        })
        .commit();

    inspect::configure(builder);
    io::configure(builder);
    math::configure(builder);
    strand::configure(builder);
}
