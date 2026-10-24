use crate::vm::Builder;

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    // `Io` is type-only, so the module has no runtime items yet.
    builder.module("io").commit();
}
