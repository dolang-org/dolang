//! `winreg.SubKeys` — a snapshot iterator over every subkey name under a
//! key, fetched once by [`crate::key`]'s `subkeys` method.
//!
//! Deliberately **not** random-access (no indexing, no destructuring): a
//! registry key is third-party-controlled and may have an unbounded number
//! of subkeys, so this only promises forward iteration (plus `.len` as a
//! hint, same as Windows itself reports it) — not an `Array`-like contract
//! that invites callers to assume cheap indexing. The whole snapshot is
//! still fetched eagerly in one round trip today; if that ever needs to
//! become paginated, this is the type that would change, not its API
//! surface.

use dolang::runtime::{
    Instance, Object, Output, Result, Slot, Strand, object::TypeBuilder, value::TypeObject,
};

pub(crate) struct SubKeys {
    index: usize,
}

pub(crate) struct SubKeysAnnex {
    pub(crate) names: Vec<String>,
}

impl SubKeys {
    pub(crate) fn new() -> Self {
        Self { index: 0 }
    }
}

impl<'v> Object<'v> for SubKeys {
    const NAME: &'v str = "SubKeys";
    const MODULE: &'v str = "winreg";
    type Annex = SubKeysAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            .get("len", |this, strand, out| {
                Output::set(strand, out, this.annex().names.len());
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
        let Some(name) = annex.names.get(borrow.index) else {
            return Ok(false);
        };
        Output::set(strand, out, name.as_str());
        borrow.index += 1;
        Ok(true)
    }
}
