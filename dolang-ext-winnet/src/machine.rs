use dolang::runtime::{
    Instance, Object, Output, State, Strand, object::TypeBuilder, unpack, value::Nil,
    vm::ModuleBuilder,
};
use dolang_ext_shell::ResultExt;
use dolang_vfs_winnet::machine;

use crate::global::Global;

pub(crate) struct MachineInfo;

pub(crate) struct ServerType;

pub(crate) struct MachineInfoAnnex<'v> {
    global: State<'v, Global<'v>>,
    info: machine::Info,
}

fn make_info<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    info: machine::Info,
    out: impl Output<'v>,
) {
    global.types.machine_info.create_with_annex(
        strand,
        MachineInfo,
        MachineInfoAnnex { global, info },
        out,
    );
}

impl<'v> Object<'v> for ServerType {
    const NAME: &'v str = "ServerType";
    const MODULE: &'v str = "winnet";
    type Annex = machine::ServerType;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("int", |this, strand, out| {
            Output::set(strand, out, u64::from(this.annex().bits()));
            Ok(())
        })
    }
}

impl<'v> Object<'v> for MachineInfo {
    const NAME: &'v str = "MachineInfo";
    const MODULE: &'v str = "winnet";
    type Annex = MachineInfoAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().info.name());
                Ok(())
            })
            .get("domain", |this, strand, out| {
                Output::set(strand, out, this.annex().info.domain());
                Ok(())
            })
            .get("version_major", |this, strand, out| {
                Output::set(strand, out, this.annex().info.version_major());
                Ok(())
            })
            .get("version_minor", |this, strand, out| {
                Output::set(strand, out, this.annex().info.version_minor());
                Ok(())
            })
            .get("comment", |this, strand, out| {
                match this.annex().info.comment() {
                    Some(comment) => Output::set(strand, out, comment),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .get("server_started", |this, strand, out| {
                Output::set(strand, out, this.annex().info.server_started());
                Ok(())
            })
            .get("server_type", |this, strand, out| {
                let annex = this.annex();
                let server_type = annex.info.server_type();
                annex.global.types.server_type.create_with_annex(
                    strand,
                    ServerType,
                    server_type,
                    out,
                );
                Ok(())
            })
            .get("workstation", |this, strand, out| {
                let has = role(&this, machine::ServerType::WORKSTATION);
                Output::set(strand, out, has);
                Ok(())
            })
            .get("server", |this, strand, out| {
                let has = role(&this, machine::ServerType::SERVER);
                Output::set(strand, out, has);
                Ok(())
            })
            .get("domain_controller", |this, strand, out| {
                let has = role(&this, machine::ServerType::DOMAIN_CTRL);
                Output::set(strand, out, has);
                Ok(())
            })
            .get("backup_domain_controller", |this, strand, out| {
                let has = role(&this, machine::ServerType::DOMAIN_BAKCTRL);
                Output::set(strand, out, has);
                Ok(())
            })
    }
}

/// Whether the machine advertises a role.
fn role<'v>(this: &Instance<'v, '_, MachineInfo>, role: machine::ServerType) -> bool {
    this.annex().info.server_type().contains(role)
}

pub(crate) fn configure_module<'v, 'a>(
    module: ModuleBuilder<'v, 'a>,
    global: State<'v, Global<'v>>,
) -> ModuleBuilder<'v, 'a> {
    module
        .value("MachineInfo", global.types.machine_info)
        .value("ServerType", global.types.server_type)
        .function("machine_info", async move |strand, args, out| {
            unpack!(strand, args =>);
            let info = machine::info(&dolang_ext_shell::vfs(strand))
                .await
                .into_sys(strand)?;
            make_info(strand, global, info, out);
            Ok(())
        })
}
