//! `winreg.Values` — a snapshot iterator over every value under a key,
//! fetched once by [`crate::key`]'s `values` method.
//!
//! Deliberately **not** random-access (no indexing, no destructuring): a
//! registry key is third-party-controlled and its value count isn't
//! bounded by anything this extension enforces, so this only promises
//! forward iteration (plus `.len` as a hint, same as Windows itself
//! reports it) — not an `Array`-like contract that invites callers to
//! assume cheap indexing. The whole snapshot is still fetched eagerly in
//! one round trip today; if that ever needs to become paginated, this is
//! the type that would change, not its API surface.

use dolang::runtime::{
    Instance, Object, Output, Result, Slot, State, Strand, object::TypeBuilder, value::TypeObject,
};
use dolang_vfs_winreg::Value;

use crate::{
    global::Global,
    value_entry::{ValueEntry, ValueEntryAnnex},
};

pub(crate) struct Values {
    index: usize,
}

pub(crate) struct ValuesAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) entries: Vec<(String, Value)>,
}

impl Values {
    pub(crate) fn new() -> Self {
        Self { index: 0 }
    }
}

impl<'v> Object<'v> for Values {
    const NAME: &'v str = "Values";
    const MODULE: &'v str = "winreg";
    type Annex = ValuesAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            .get("len", |this, strand, out| {
                Output::set(strand, out, this.annex().entries.len());
                Ok(())
            })
    }

    async fn iter<'a, 's>(
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
        let annex = this.annex();
        let Some((name, value)) = annex.entries.get(borrow.index) else {
            return Ok(false);
        };
        annex.global.types.value_entry.create_with_annex(
            strand,
            ValueEntry,
            ValueEntryAnnex {
                global: annex.global,
                name: name.clone(),
                value: value.clone(),
            },
            out,
        );
        borrow.index += 1;
        Ok(true)
    }
}
