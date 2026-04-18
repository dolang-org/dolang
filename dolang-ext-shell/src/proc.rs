use std::fmt;

use dolang::{
    compile::Compiler,
    runtime::{
        Error, Instance, Object, Output, Result, Slot, State, Strand,
        error::ResultExt,
        object::TypeBuilder,
        strand::Redirect,
        unpack,
        value::{Singleton, TypeObject},
        vm::Builder,
    },
};

use crate::global::Global;
use crate::local::ChannelMode;

/// Capture output from a subprocess.
pub(crate) struct Capture(String);

impl Capture {
    pub(crate) fn new() -> Self {
        Self(String::new())
    }
}

impl Default for Capture {
    fn default() -> Self {
        Self::new()
    }
}

impl<'v> Object<'v> for Capture {
    const NAME: &'v str = "Capture";
    const MODULE: &'v str = "proc";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Sink)
    }

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<capture>").into_do(strand)
    }

    async fn output<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut capture = this.borrow_mut(strand)?;
        if let Some(str) = value.as_str(strand) {
            strand.access(|x| capture.0.push_str(str.as_str(x)));
        } else {
            capture.0.push_str(&value.to_string(strand)?);
        }
        capture.0.push('\n');
        Ok(())
    }
}

pub(crate) fn configure_compiler<'a>(compiler: &mut Compiler<'a>) {
    compiler
        .prelude()
        .import_items("proc")
        .items(["sub"])
        .commit()
        .import_module_with_name("proc.run", "run");
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let capture_ty = builder.register_type::<Capture>();
    let trim = builder.sym("trim");

    builder
        .module("proc")
        .function("io_mode", async move |strand, args, out| {
            let ([mode, func], [], rest) = unpack!(strand, args, 2, 0, ...)?;
            let mode = match mode.as_sym(strand) {
                Some(sym) if sym == global.syms.line => ChannelMode::Line,
                Some(sym) if sym == global.syms.chunk => ChannelMode::Chunk,
                _ => return Err(Error::value(strand, "mode must be :line: or :chunk:")),
            };
            let old_mode = {
                let local = global.local.get(strand);
                let old_mode = local.channel_mode();
                local.set_channel_mode(mode);
                old_mode
            };
            let res = func.call(strand, rest, out).await;
            global.local.get(strand).set_channel_mode(old_mode);
            res
        })
        .function("mute", async move |strand, args, out| {
            let ([func], [], rest) = unpack!(strand, args, 1, 0, ...)?;
            Redirect::new(strand)
                .output(Singleton::IterNull)
                .enter(async move |strand| func.call(strand, rest, out).await)
                .await
        })
        .function_with_slots("sub", async move |strand, args, out, [mut cap, tmp]| {
            let ([func], [trim], rest) = unpack!(strand, args, 1, 0, trim = None, ...)?;
            let trim = trim.map(|v| v.to_bool(strand)).unwrap_or(true);
            capture_ty.create(strand, Capture::new(), &mut cap);
            Redirect::new(strand)
                .output(&cap)
                .enter(async move |strand| func.call(strand, rest, tmp).await)
                .await?;
            let capture = capture_ty.downcast(&cap).unwrap().borrow(strand)?;
            let mut value = capture.0.as_str();
            if trim {
                value = value.trim_end_matches(['\r', '\n'])
            }
            Output::set(strand, out, value);
            Ok(())
        })
        .value("Error", global.types.proc_error)
        .commit();
}
