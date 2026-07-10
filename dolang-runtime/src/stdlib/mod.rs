use std::hash::{DefaultHasher, Hasher};

use crate::{arg::Arg, error::Error, unpack, value::Output, vm::Builder};

mod property;
mod strand;

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let property_types = property::register(builder);
    let bc = builder.singletons();

    // Core types
    let value = bc.value.dup();
    let type_obj = bc.type_obj.dup();
    let int = bc.int.dup();
    let float = bc.float.dup();
    let bool = bc.bool.dup();
    let nil = bc.nil.dup();
    let str = bc.str.dup();
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
    let args = bc.args.dup();

    // Iterator protocol types (from former std.iter)
    let iter_type = bc.input_iter.dup();
    let sink_type = bc.output_iter.dup();
    let iterable_type = bc.iterable.dup();
    let sinkable_type = bc.sinkable.dup();
    let getter_type = bc.getter.dup();
    let setter_type = bc.setter.dup();
    let nulliter = bc.nulliter.dup();
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
        .value("value", &value)
        .value("type", &type_obj)
        .value("int", &int)
        .value("float", &float)
        .value("bool", &bool)
        .value("Nil", &nil)
        .value("str", &str)
        .value("sym", &sym)
        .value("array", &array)
        .value("dict", &dict)
        .value("set", &set)
        .value("tuple", &tuple)
        .value("func", &func)
        .value("range", &range)
        .value("getter", property_types.getter)
        .value("setter", property_types.setter)
        .value("module", &module)
        .value("record", &record)
        .value("bin", &bin)
        .value("args", &args)
        // Iterator protocol types
        .value("Iterable", &iterable_type)
        .value("Sinkable", &sinkable_type)
        .value("Getter", &getter_type)
        .value("Setter", &setter_type)
        .value("Iter", &iter_type)
        .value("Sink", &sink_type)
        .value("nulliter", &nulliter)
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
        // Core functions
        .function("arg", async move |strand, args, out| {
            let ([value], _) = unpack!(strand, args, 1, 0)?;
            let str = value.to_arg(strand)?;
            Output::set(strand, out, str.as_str());
            Ok(())
        })
        .function("dbg", async move |strand, args, out| {
            let ([value], _) = unpack!(strand, args, 1, 0)?;
            let debug = value.to_debug(strand)?;
            Output::set(strand, out, debug.as_str());
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
    strand::configure(builder);
}
