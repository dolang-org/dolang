//! `winreg.SubKeys` — a live forward iterator over subkey names.
//!
//! Deliberately **not** random-access (no indexing, no destructuring): a
//! registry key is third-party-controlled and may have an unbounded number
//! of subkeys, so this only promises forward iteration (plus `.len` as a
//! hint captured when enumeration opens) — not an `Array`-like contract that
//! invites callers to assume cheap indexing. Entries are fetched in pages as
//! iteration advances.

use dolang::runtime::{
    AllocExt, Instance, Object, Result, Slot, Strand, object::TypeBuilder, value::TypeObject,
};
use dolang_ext_shell::ResultExt;

pub(crate) struct SubKeys(pub(crate) dolang_vfs_winreg::SubKeys);

impl<'v> Object<'v> for SubKeys {
    const NAME: &'v str = "SubKeys";
    const MODULE: &'v str = "winreg";
    type Annex = ();
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
        let name = this
            .borrow_mut(strand)?
            .0
            .next_entry()
            .await
            .into_sys(strand)?;
        let Some(name) = name else {
            return Ok(false);
        };
        strand.store(out, name.as_str());
        Ok(true)
    }
}
