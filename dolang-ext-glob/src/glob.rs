use std::{fmt, str::FromStr};

use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    error::ResultExt,
    object::TypeBuilder,
    unpack,
    value::{Str, View},
    vm::Builder,
};
use wax::{Glob as WaxGlob, Program as _};

use crate::global::Global;

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("glob")
        .value("Glob", global.types.glob)
        .function("matches", async move |strand, args, mut out| {
            let ([pattern, value], []) = unpack!(strand, args, 2, 0)?;
            let pattern = PatternArg::new(strand, global, &pattern)?;
            let value = value
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "value: expected `str`"))?;
            let matched = pattern.is_match(strand, value)?;
            Output::set(strand, &mut out, matched);
            Ok(())
        })
        .commit();
}

enum PatternArg<'v, 'a> {
    Glob(Instance<'v, 'a, Glob>),
    Str(String),
}

impl<'v, 'a> PatternArg<'v, 'a> {
    fn new<'s>(
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        value: &'a Value<'v>,
    ) -> Result<'v, 's, Self> {
        if let Some(glob) = global.types.glob.downcast(value) {
            Ok(Self::Glob(glob))
        } else {
            match value.view(strand.vm()) {
                View::Str(pattern) => Ok(Self::Str(pattern.into())),
                _ => Err(Error::type_error(strand, "pattern: expected Glob or str")),
            }
        }
    }

    fn is_match<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        value: Str<'v, '_>,
    ) -> Result<'v, 's, bool> {
        Ok(match self {
            Self::Glob(glob) => strand.access(|x| glob.annex().glob.is_match(value.as_str(x))),
            Self::Str(pattern) => {
                let glob = compile(pattern, strand)?;
                strand.access(|x| glob.is_match(value.as_str(x)))
            }
        })
    }
}

pub(crate) struct Glob;

pub(crate) struct GlobAnnex {
    glob: WaxGlob<'static>,
}

impl<'v> Object<'v> for Glob {
    const NAME: &'v str = "Glob";
    const MODULE: &'v str = "glob";
    type Annex = GlobAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([pattern], []) = unpack!(strand, args, 1, 0)?;
        let pattern: String = match pattern.view(strand.vm()) {
            View::Str(pattern) => pattern.into(),
            _ => return Err(Error::type_error(strand, "pattern: expected `str`")),
        };
        let glob = compile(&pattern, strand)?;
        this.create_with_annex(strand, Glob, GlobAnnex { glob }, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method("matches", async move |this, strand, args, mut out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let matched = match value.view(strand.vm()) {
                View::Str(value) => {
                    strand.access(|access| this.annex().glob.is_match(value.as_str(access)))
                }
                _ => return Err(Error::type_error(strand, "value: expected `str`")),
            };
            Output::set(strand, &mut out, matched);
            Ok(())
        })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().glob).into_do(strand)
    }
}

fn compile<'v, 's>(pattern: &str, strand: &mut Strand<'v, 's>) -> Result<'v, 's, WaxGlob<'static>> {
    WaxGlob::from_str(pattern).into_do(strand)
}
