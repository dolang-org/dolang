//! Immutable snapshot returned by `winscm.Service.config()`.

use dolang::runtime::{
    Object, Output, State,
    object::{FlagsTypeExt, TypeBuilder},
    value::{AsTuple, Nil},
};
use dolang_vfs_winscm::ServiceConfig;

use crate::{convert, flags::ServiceType, global::Global};

pub(crate) struct Config;

pub(crate) struct ConfigAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) config: ServiceConfig,
}

impl<'v> Object<'v> for Config {
    const NAME: &'v str = "ServiceConfig";
    const MODULE: &'v str = "winscm";
    type Annex = ConfigAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("service_type", |this, strand, out| {
                let annex = this.annex();
                annex.global.types.service_type.create_flags(
                    strand,
                    ServiceType::from(annex.config.service_type),
                    out,
                );
                Ok(())
            })
            .get("start_type", |this, strand, out| {
                let annex = this.annex();
                convert::start_type_to_sym(strand, annex.global, annex.config.start_type, out)
            })
            .get("error_control", |this, strand, out| {
                let annex = this.annex();
                convert::error_control_to_sym(strand, annex.global, annex.config.error_control, out)
            })
            .get("binary_path", |this, strand, out| {
                Output::set(strand, out, this.annex().config.binary_path.as_str());
                Ok(())
            })
            .get("load_order_group", |this, strand, out| {
                match this.annex().config.load_order_group.as_deref() {
                    Some(group) => Output::set(strand, out, group),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .get("tag_id", |this, strand, out| {
                Output::set(strand, out, i128::from(this.annex().config.tag_id));
                Ok(())
            })
            .get("dependencies", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    AsTuple::new(this.annex().config.dependencies.iter().map(String::as_str)),
                );
                Ok(())
            })
            .get("service_start_name", |this, strand, out| {
                Output::set(strand, out, this.annex().config.service_start_name.as_str());
                Ok(())
            })
            .get("display_name", |this, strand, out| {
                Output::set(strand, out, this.annex().config.display_name.as_str());
                Ok(())
            })
    }
}
