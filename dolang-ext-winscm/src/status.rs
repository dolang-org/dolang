//! `winscm.Status` — a service's status, as reported by `Service.control`,
//! `.query_status`, `.wait_for_status_change`, and `ServiceInfo.status`.

use dolang::runtime::{
    Object, Output, State, Strand,
    object::{FlagsTypeExt, TypeBuilder},
};
use dolang_vfs_winscm::ServiceStatus;

use crate::{
    convert,
    flags::{ServiceControlsAccepted, ServiceType},
    global::Global,
};

pub(crate) struct Status;

pub(crate) struct StatusAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) status: ServiceStatus,
}

/// Builds a `winscm.Status` from a raw [`ServiceStatus`]. Shared by
/// `Service.control`/`.query_status`/`.wait_for_status_change` and
/// `ServiceInfo`'s lazily-constructed `status` field.
pub(crate) fn create_status<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    status: ServiceStatus,
    out: impl Output<'v>,
) {
    global
        .types
        .status
        .create_with_annex(strand, Status, StatusAnnex { global, status }, out);
}

impl<'v> Object<'v> for Status {
    const NAME: &'v str = "Status";
    const MODULE: &'v str = "winscm";
    type Annex = StatusAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("service_type", |this, strand, out| {
                let annex = this.annex();
                annex.global.types.service_type.create_flags(
                    strand,
                    ServiceType::from(annex.status.service_type),
                    out,
                );
                Ok(())
            })
            .get("current_state", |this, strand, out| {
                let annex = this.annex();
                convert::service_state_to_sym(strand, annex.global, annex.status.current_state, out)
            })
            .get("controls_accepted", |this, strand, out| {
                let annex = this.annex();
                annex.global.types.controls_accepted.create_flags(
                    strand,
                    ServiceControlsAccepted::from(annex.status.controls_accepted),
                    out,
                );
                Ok(())
            })
            .get("win32_exit_code", |this, strand, out| {
                let annex = this.annex();
                Output::set(strand, out, i128::from(annex.status.win32_exit_code));
                Ok(())
            })
            .get("service_specific_exit_code", |this, strand, out| {
                let annex = this.annex();
                Output::set(
                    strand,
                    out,
                    i128::from(annex.status.service_specific_exit_code),
                );
                Ok(())
            })
            .get("check_point", |this, strand, out| {
                let annex = this.annex();
                Output::set(strand, out, i128::from(annex.status.check_point));
                Ok(())
            })
            .get("wait_hint", |this, strand, out| {
                let annex = this.annex();
                Output::set(strand, out, i128::from(annex.status.wait_hint));
                Ok(())
            })
            .get("process_id", |this, strand, out| {
                let annex = this.annex();
                Output::set(strand, out, i128::from(annex.status.process_id));
                Ok(())
            })
    }
}
