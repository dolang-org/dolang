//! `winscm.Services` — a snapshot iterator over every service matching an
//! `enumerate_services` filter, fetched once by [`crate::manager`]'s
//! `enumerate_services` method.
//!
//! Deliberately **not** random-access (no indexing, no destructuring), same
//! rationale as `dolang-ext-winreg::subkeys::SubKeys`: the number of
//! services on a real machine isn't bounded by anything this extension
//! enforces, so this only promises forward iteration (plus `.len` as a
//! hint) — not an `Array`-like contract that invites callers to assume
//! cheap indexing. The whole snapshot is still fetched eagerly in one
//! round trip today; if that ever needs to become paginated, this is the
//! type that would change, not its API surface.

use dolang::runtime::{
    Instance, Object, Output, Result, Slot, State, Strand, object::TypeBuilder, value::TypeObject,
};
use dolang_vfs_winscm::ServiceInfo;

use crate::{
    global::Global,
    service_info::{ServiceEntry, ServiceEntryAnnex},
};

pub(crate) struct Services {
    index: usize,
}

pub(crate) struct ServicesAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) entries: Vec<ServiceInfo>,
}

impl Services {
    pub(crate) fn new() -> Self {
        Self { index: 0 }
    }
}

impl<'v> Object<'v> for Services {
    const NAME: &'v str = "Services";
    const MODULE: &'v str = "winscm";
    type Annex = ServicesAnnex<'v>;
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
        let Some(entry) = annex.entries.get(borrow.index) else {
            return Ok(false);
        };
        annex.global.types.service_entry.create_with_annex(
            strand,
            ServiceEntry,
            ServiceEntryAnnex {
                global: annex.global,
                name: entry.name.clone(),
                display_name: entry.display_name.clone(),
                status: entry.status,
            },
            out,
        );
        borrow.index += 1;
        Ok(true)
    }
}
