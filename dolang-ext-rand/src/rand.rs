use dolang::runtime::{State, vm::Register};

use crate::{
    global::Global,
    ops::{self, Source},
};

pub(crate) fn configure<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("rand")
        .value("Rng", global.types.rng)
        .function("int", async move |strand, args, out| {
            ops::int(strand, Source::Thread, args, out)
        })
        .function("float", async move |strand, args, out| {
            ops::float(strand, Source::Thread, args, out)
        })
        .function("bool", async move |strand, args, out| {
            ops::bool(strand, Source::Thread, args, out)
        })
        .function("bin", async move |strand, args, out| {
            ops::bin(strand, Source::Thread, args, out)
        })
        .function("str", async move |strand, args, out| {
            ops::str(strand, &global, Source::Thread, args, out)
        })
        .function("pick", async move |strand, args, out| {
            ops::pick(strand, &global, Source::Thread, args, out)
        })
        .function("sample", async move |strand, args, out| {
            ops::sample(strand, &global, Source::Thread, args, out)
        })
        .function("shuffle", async move |strand, args, _out| {
            ops::shuffle(strand, Source::Thread, args)
        })
        .commit();
}
