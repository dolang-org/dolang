//! `winscm.Services` — a live forward iterator over matching services.
//!
//! Deliberately **not** random-access (no indexing, no destructuring), same
//! rationale as `dolang-ext-winreg::subkeys::SubKeys`: the number of
//! services on a real machine isn't bounded by anything this extension
//! enforces, so this only promises forward iteration. Entries are fetched in
//! pages as iteration advances.

use dolang::runtime::{
    AllocExt, Instance, Object, Result, Slot, State, Strand, object::TypeBuilder, value::TypeObject,
};
use dolang_ext_shell::ResultExt;

use crate::{
    global::Global,
    service_info::{ServiceEntry, ServiceEntryAnnex},
};

pub(crate) struct Services(pub(crate) dolang_vfs_winscm::Services);

pub(crate) struct ServicesAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for Services {
    const NAME: &'v str = "Services";
    const MODULE: &'v str = "winscm";
    type Annex = ServicesAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
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
        let Some(entry) = entry else {
            return Ok(false);
        };
        annex.global.types.service_entry.create_with_annex(
            strand,
            ServiceEntry,
            ServiceEntryAnnex {
                global: annex.global,
                name: entry.name,
                display_name: entry.display_name,
                status: entry.status,
            },
            out,
        );
        Ok(true)
    }
}
