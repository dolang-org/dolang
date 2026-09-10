use std::hash::{DefaultHasher, Hasher};

use crate::{
    arg::Arg,
    error::Error,
    object::{array::Array, class, dict::Dict, float, int, record::Record, tuple},
    unpack,
    value::{Output, StrEmbryo, Value},
    vm::Builder,
};

pub(crate) mod fmt;
mod math;
mod property;
mod strand;

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let property_types = property::register(builder);
    let fmt = fmt::register(builder);
    let member_scopes = class::register_member_scopes(builder);
    let member_scopes = builder.register_state(member_scopes);
    let bc = builder.singletons();

    // Core types
    let value = bc.value.dup();
    let num = bc.num.dup();
    let type_obj = bc.type_obj.dup();
    let int = bc.int.dup();
    let float = bc.float.dup();
    let bool = bc.bool.dup();
    let nil = bc.nil.dup();
    let str = bc.str.dup();
    let strbuf = bc.strbuf.dup();
    let sym = bc.sym.dup();
    let array = bc.array.dup();
    let dict = bc.dict.dup();
    let set = bc.set.dup();
    let tuple = bc.tuple.dup();
    let func = bc.func.dup();
    let range = bc.range.dup();
    let module = bc.module.dup();
    let record = bc.record.dup();
    let bin = bc.bin.dup();
    let binbuf = bc.binbuf.dup();
    let args = bc.args.dup();

    // Iterator protocol types (from former std.iter)
    let iter_type = bc.input_iter.dup();
    let sink_type = bc.output_iter.dup();
    let iterable_type = bc.iterable.dup();
    let sinkable_type = bc.sinkable.dup();
    let getter_type = bc.getter.dup();
    let setter_type = bc.setter.dup();
    let null = bc.null.dup();
    let null_type = bc.null_type.dup();
    let error_sink_stop = bc.error_sink_stop.dup();
    let error_iter_stop = bc.error_iter_stop.dup();

    // Error types (from former std.error)
    let error = bc.error.dup();
    let error_unsupported = bc.error_unsupported.dup();
    let error_immutable = bc.error_immutable.dup();
    let error_concurrency = bc.error_concurrency.dup();
    let error_type = bc.error_type.dup();
    let error_value = bc.error_value.dup();
    let error_state = bc.error_state.dup();
    let error_index = bc.error_index.dup();
    let error_field = bc.error_field.dup();
    let error_unexpected_pos = bc.error_unexpected_pos.dup();
    let error_unexpected_key = bc.error_unexpected_key.dup();
    let error_missing_pos = bc.error_missing_pos.dup();
    let error_missing_key = bc.error_missing_key.dup();
    let error_overflow = bc.error_overflow.dup();
    let error_zerodiv = bc.error_zerodiv.dup();
    let error_cyclic_import = bc.error_cyclic_import.dup();
    let error_import = bc.error_import.dup();
    let error_compile = bc.error_compile.dup();
    let error_bytecode = bc.error_bytecode.dup();
    let error_runtime = bc.error_runtime.dup();
    let error_abort = bc.error_abort.dup();
    let error_canceled = bc.error_canceled.dup();
    let error_timed_out = bc.error_timed_out.dup();

    builder
        .module("std")
        // Core types
        .value("Value", &value)
        .value("Num", &num)
        .value("Type", &type_obj)
        .value("Int", &int)
        .value("Float", &float)
        .value("Bool", &bool)
        .value("Nil", &nil)
        .value("Str", &str)
        .value("StrBuf", &strbuf)
        .value("Sym", &sym)
        .value("Array", &array)
        .value("Dict", &dict)
        .value("Set", &set)
        .value("Tuple", &tuple)
        .value("Func", &func)
        .value("Range", &range)
        .value("getter", property_types.getter)
        .value("setter", property_types.setter)
        .value("class", member_scopes.class)
        .value("static", member_scopes.statik)
        .value("Module", &module)
        .value("Record", &record)
        .value("Bin", &bin)
        .value("BinBuf", &binbuf)
        .value("Args", &args)
        .value("FmtSpec", fmt.types.spec)
        .value("FmtValue", fmt.types.value)
        .value("FmtParam", fmt.types.param)
        .value("Fmt", fmt.types.fmt)
        // Iterator protocol types
        .value("Iterable", &iterable_type)
        .value("Sinkable", &sinkable_type)
        .value("Getter", &getter_type)
        .value("Setter", &setter_type)
        .value("Iter", &iter_type)
        .value("Sink", &sink_type)
        .value("Null", &null_type)
        .value("null", &null)
        .value("SinkStop", &error_sink_stop)
        .value("IterStop", &error_iter_stop)
        // Error types
        .value("Error", &error)
        .value("UnsupportedError", &error_unsupported)
        .value("ImmutableError", &error_immutable)
        .value("ConcurrencyError", &error_concurrency)
        .value("TypeError", &error_type)
        .value("ValueError", &error_value)
        .value("StateError", &error_state)
        .value("IndexError", &error_index)
        .value("FieldError", &error_field)
        .value("UnexpectedPosError", &error_unexpected_pos)
        .value("UnexpectedKeyError", &error_unexpected_key)
        .value("MissingPosError", &error_missing_pos)
        .value("MissingKeyError", &error_missing_key)
        .value("OverflowError", &error_overflow)
        .value("ZeroDivError", &error_zerodiv)
        .value("CyclicImportError", &error_cyclic_import)
        .value("ImportError", &error_import)
        .value("CompileError", &error_compile)
        .value("BytecodeError", &error_bytecode)
        .value("RuntimeError", &error_runtime)
        .value("AbortError", &error_abort)
        .value("CanceledError", &error_canceled)
        .value("TimedOutError", &error_timed_out)
        // Collection factories
        .function("array", async move |strand, args, out| {
            let array = Array::from_args(strand, args)?;
            strand.builtin_types().array.create(strand, array, out);
            Ok(())
        })
        .function("dict", async move |strand, args, out| {
            let dict = Dict::from_args(strand, args)?;
            strand.builtin_types().dict.create(strand, dict, out);
            Ok(())
        })
        .function("tuple", async move |strand, args, mut out| {
            let values = tuple::from_args(strand, args)?;
            out.store(Value::from_object(tuple::tuple(strand.vm(), values)));
            Ok(())
        })
        .function("record", async move |strand, args, out| {
            let record = Record::from_args(strand, args)?;
            strand.builtin_types().record.create(strand, record, out);
            Ok(())
        })
        .function("type", async move |strand, args, out| {
            let ([obj], [ty]) = unpack!(strand, args, 1, 1)?;
            if let Some(ty) = ty {
                let result = obj.is_instance_of(strand, &ty);
                Output::set(strand, out, result);
            } else {
                obj.op_type(strand, out);
            }
            Ok(())
        })
        .function("int", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let value = int::coerce(&value, strand)?;
            Output::set(strand, out, value);
            Ok(())
        })
        .function("float", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let value = float::coerce(&value, strand)?;
            Output::set(strand, out, value);
            Ok(())
        })
        .function("bool", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let value = value.op_bool(strand);
            Output::set(strand, out, value);
            Ok(())
        })
        .function("str", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let mut format = StrEmbryo::new();
            value.display(strand, &mut format)?;
            format.finish(strand, out);
            Ok(())
        })
        .function("sym", async move |strand, args, mut out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            if value.as_sym(strand).is_some() {
                Output::set(strand, out, value);
                Ok(())
            } else if let Some(value) = value.as_str_raw(strand) {
                strand.sym_gc();
                out.store(Value::from_object(strand.sym_register_obj(value)));
                Ok(())
            } else {
                Err(Error::type_error(strand, "sym: expected Str or Sym"))
            }
        })
        // Core functions
        .function("verbatim", async move |strand, args, out| {
            let ([value], _) = unpack!(strand, args, 1, 0)?;
            let mut format = StrEmbryo::new();
            value.verbatim(strand, &mut format)?;
            format.finish(strand, out);
            Ok(())
        })
        .function("dbg", async move |strand, args, out| {
            let ([value], _) = unpack!(strand, args, 1, 0)?;
            let mut format = StrEmbryo::new();
            value.debug(strand, &mut format)?;
            format.finish(strand, out);
            Ok(())
        })
        .function("hash", async move |strand, args, out| {
            let mut hasher = DefaultHasher::new();
            for (i, arg) in args.enumerate() {
                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_trap()?;
                }
                let slot = match arg {
                    Arg::Pos(s) => s,
                    Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
                };
                slot.op_hash(strand, &mut hasher)?;
            }
            Output::set(strand, out, hasher.finish());
            Ok(())
        })
        .commit();

    math::configure(builder);
    strand::configure(builder);
}
