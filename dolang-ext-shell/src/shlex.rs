//! Shell quoting and string splitting utilities.
//!
//! This module provides functions for quoting strings for shell safety,
//! joining arguments with proper quoting, and splitting shell command lines.

use std::mem;

use dolang::{
    compile::Compiler,
    runtime::{
        Error, Instance, Object, Output, Result, Slot, Strand,
        error::ResultExt,
        object::{Mut, TypeBuilder},
        unpack,
        value::{PinStr, TypeObject},
        vm::Builder,
    },
};
use shlex::Shlex;

// Transmuted to `'static` lifetime with the string kept alive by a GC slot
pub(crate) struct Iter<'v> {
    shlex: Shlex<'static>,
    _pin: PinStr<'v, 'static>,
}

impl<'v> Object<'v> for Iter<'v> {
    const NAME: &'v str = "Iter";
    const MODULE: &'v str = "shlex";
    // Slot 0: string being iterated
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;

        // Get the next token
        match borrow.shlex.next() {
            Some(token) => {
                Output::set(strand, out, token.as_str());
                Ok(true)
            }
            None => {
                // Check if there was an error
                if borrow.shlex.had_error {
                    return Err(Error::runtime(strand, "parse error"));
                }
                Ok(false)
            }
        }
    }
}

pub(crate) fn configure_compiler<'a>(_compiler: &mut Compiler<'a>) {
    // Not added to prelude
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>) {
    let iter = builder.register_type::<Iter>();

    builder
        .module("shlex")
        .function("quote", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let s = arg.to_arg(strand)?;
            let quoted = shlex::try_quote(&s).into_do(strand)?;
            Output::set(strand, out, quoted.as_ref());
            Ok(())
        })
        .function("split", async move |strand, args, mut out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;

            // Input must be a string
            let s = arg
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "expected `str`"))?;

            // SAFETY: the string will be kept alive as long as this iterator object exists.
            let pin = unsafe { s.pin().into_static_unchecked() };
            let shlex = Shlex::new(unsafe { mem::transmute::<&str, &'static str>(&*pin) });
            iter.create(strand, Iter { shlex, _pin: pin }, &mut out);
            Output::set(
                strand,
                Mut::slot_mut::<0>(&mut iter.downcast(&out).unwrap().borrow_mut_unwrap()),
                arg,
            );
            Ok(())
        })
        .function_with_slots(
            "join",
            async move |strand, args, out, [mut iter, mut item]| {
                let ([iterable], []) = unpack!(strand, args, 1, 0)?;

                // Collect all items from iterable
                let mut items = Vec::new();
                iterable.iter(strand, &mut iter).await?;
                while iter.next(strand, &mut item).await? {
                    let s = item.to_arg(strand)?;
                    items.push(s);
                }

                // Join with quoting
                let joined = shlex::try_join(items.iter().map(|s| s.as_str())).into_do(strand)?;
                Output::set(strand, out, joined.as_str());
                Ok(())
            },
        )
        .commit();
}
