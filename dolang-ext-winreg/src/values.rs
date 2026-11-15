//! `winreg.Values` — a live forward iterator over values under a key.
//!
//! Deliberately **not** random-access (no indexing, no destructuring): a
//! registry key is third-party-controlled and its value count isn't
//! bounded by anything this extension enforces, so this only promises
//! forward iteration (plus `.len` captured when enumeration opens) — not an
//! `Array`-like contract that invites callers to assume cheap indexing.
//! Entries are fetched in pages as iteration advances.

use dolang::runtime::{
    AllocExt, Instance, Object, Result, Slot, State, Strand, object::TypeBuilder, value::TypeObject,
};
use dolang_ext_shell::ResultExt;

use crate::{
    global::Global,
    value_entry::{ValueEntry, ValueEntryAnnex},
};

pub(crate) struct Values(pub(crate) dolang_vfs_winreg::Values);

pub(crate) struct ValuesAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
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
                let borrow = this.borrow(strand)?;
                strand.store(out, borrow.0.len());
                Ok(())
            })
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let entry = this
            .borrow_mut(strand)?
            .0
            .next_entry()
            .await
            .into_sys(strand)?;
        let annex = this.annex();
        let Some((name, value)) = entry else {
            return Ok(false);
        };
        annex.global.types.value_entry.create_with_annex(
            strand,
            ValueEntry,
            ValueEntryAnnex {
                global: annex.global,
                name,
                value,
            },
            out,
        );
        Ok(true)
    }
}
