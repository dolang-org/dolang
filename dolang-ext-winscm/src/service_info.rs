//! `winscm.ServiceInfo` — one entry of a [`crate::services::Services`]
//! snapshot.

use dolang::runtime::{AllocExt, Object, State, object::TypeBuilder};
use dolang_vfs_winscm::ServiceStatus;

use crate::{global::Global, status};

pub(crate) struct ServiceEntry;

pub(crate) struct ServiceEntryAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) name: String,
    pub(crate) display_name: Option<String>,
    pub(crate) status: ServiceStatus,
}

impl<'v> Object<'v> for ServiceEntry {
    const NAME: &'v str = "ServiceInfo";
    const MODULE: &'v str = "winscm";
    type Annex = ServiceEntryAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                strand.store(out, this.annex().name.as_str());
                Ok(())
            })
            .get("display_name", |this, strand, out| {
                match this.annex().display_name.as_deref() {
                    Some(display_name) => strand.store(out, display_name),
                    None => strand.store(out, dolang::runtime::value::Nil),
                }
                Ok(())
            })
            .get("status", |this, strand, out| {
                let annex = this.annex();
                status::create_status(strand, annex.global, annex.status, out);
                Ok(())
            })
    }
}
