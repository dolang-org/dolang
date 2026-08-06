//! `winscm.Service` — an open handle to a specific service.

use dolang::runtime::{
    Arg, Error, Object, Result, Slot, State, Strand, call, method,
    object::{FlagsTypeExt, TypeBuilder},
    unpack,
};
use dolang_ext_shell::ResultExt;
use dolang_vfs_winscm::ServiceConfigUpdate;
use dolang_winterop::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    SACL_SECURITY_INFORMATION,
};

use crate::{
    config::{Config, ConfigAnnex},
    convert,
    global::Global,
    manager::{expect_str, expect_str_iterable},
    status,
};

pub(crate) struct Service(pub(crate) Option<dolang_vfs_winscm::Service>);

/// Wraps a freshly opened/created [`dolang_vfs_winscm::Service`] into a
/// `winscm.Service`. With a trailing block, the block is called with the
/// handle and the service is auto-closed afterward; without one, the
/// handle is returned directly and the caller owns it. Mirrors
/// `dolang-ext-winreg::key::finish_open`.
pub(crate) async fn finish_open_service<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    inner: dolang_vfs_winscm::Service,
    block: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if let Some(block) = block {
        strand
            .with_slots(async move |strand, [mut handle, mut tmp]| {
                global
                    .types
                    .service
                    .create(strand, Service(Some(inner)), &mut handle);
                let result = call!(strand, block, out, &handle).await;
                let _ = method!(strand, &handle, global.syms.close, &mut tmp).await;
                result
            })
            .await
    } else {
        global
            .types
            .service
            .create(strand, Service(Some(inner)), out);
        Ok(())
    }
}

/// Builds the `owner`/`group`/`dacl`/`sacl` security-information mask from
/// `Service.sec_desc()`'s boolean keyword arguments. Owner/group/dacl
/// default `true`, sacl defaults `false` (needs an explicit opt-in). Copied
/// from `dolang-ext-winreg::key::sec_desc_mask` — not shared via
/// `dolang-ext-shell`, since it isn't exposed there either.
fn sec_desc_mask<'v, 's>(
    strand: &mut Strand<'v, 's>,
    owner: Option<Slot<'v, '_>>,
    group: Option<Slot<'v, '_>>,
    dacl: Option<Slot<'v, '_>>,
    sacl: Option<Slot<'v, '_>>,
) -> Result<'v, 's, u32> {
    fn selected<'v, 's>(
        strand: &mut Strand<'v, 's>,
        value: Option<Slot<'v, '_>>,
        default: bool,
    ) -> Result<'v, 's, bool> {
        value
            .map(|value| {
                value.as_bool(strand).ok_or_else(|| {
                    Error::type_error(strand, "security descriptor component: expected Bool")
                })
            })
            .transpose()
            .map(|value| value.unwrap_or(default))
    }
    let mut mask = 0;
    if selected(strand, owner, true)? {
        mask |= OWNER_SECURITY_INFORMATION;
    }
    if selected(strand, group, true)? {
        mask |= GROUP_SECURITY_INFORMATION;
    }
    if selected(strand, dacl, true)? {
        mask |= DACL_SECURITY_INFORMATION;
    }
    if selected(strand, sacl, false)? {
        mask |= SACL_SECURITY_INFORMATION;
    }
    Ok(mask)
}

impl<'v> Object<'v> for Service {
    const NAME: &'v str = "Service";
    const MODULE: &'v str = "winscm";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let owner = builder.sym("owner");
        let group = builder.sym("group");
        let dacl = builder.sym("dacl");
        let sacl = builder.sym("sacl");
        builder
            .method("delete", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let service = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                service.delete().await.into_sys(strand)
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let inner = this.borrow_mut(strand)?.0.take();
                match inner {
                    Some(inner) => inner.close().await.into_sys(strand),
                    None => Ok(()),
                }
            })
            .method("start", async move |this, strand, args, _out| {
                let ([], [], rest) = unpack!(strand, args, 0, 0, ...)?;
                let mut start_args = Vec::with_capacity(rest.len());
                for arg in rest {
                    match arg {
                        Arg::Pos(value) => {
                            start_args.push(expect_str(strand, value, "start argument")?);
                        }
                        Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                    }
                }
                let borrow = this.borrow(strand)?;
                let service = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                service.start_with_args(&start_args).await.into_sys(strand)
            })
            .method("control", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let ([control], []) = unpack!(strand, args, 1, 0)?;
                let control = convert::service_control_from_sym(strand, global, control)?;
                let status = {
                    let borrow = this.borrow(strand)?;
                    let service = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                    service.control(control).await.into_sys(strand)?
                };
                status::create_status(strand, global, status, out);
                Ok(())
            })
            .method("query_status", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let status = {
                    let borrow = this.borrow(strand)?;
                    let service = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                    service.query_status().await.into_sys(strand)?
                };
                status::create_status(strand, global, status, out);
                Ok(())
            })
            .method("config", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let config = {
                    let borrow = this.borrow(strand)?;
                    let service = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                    service.config().await.into_sys(strand)?
                };
                global.types.config.create_with_annex(
                    strand,
                    Config,
                    ConfigAnnex { global, config },
                    out,
                );
                Ok(())
            })
            .method("set_config", async move |this, strand, args, _out| {
                let global = strand.state::<Global<'v>>();
                let service_type = global.syms.service_type;
                let start_type = global.syms.start_type;
                let error_control = global.syms.error_control;
                let binary_path = global.syms.binary_path;
                let load_order_group = global.syms.load_order_group;
                let dependencies = global.syms.dependencies;
                let service_start_name = global.syms.service_start_name;
                let password = global.syms.password;
                let display_name = global.syms.display_name;
                let (
                    [],
                    [
                        service_type,
                        start_type,
                        error_control,
                        binary_path,
                        load_order_group,
                        dependencies,
                        service_start_name,
                        password,
                        display_name,
                    ],
                ) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    service_type = None,
                    start_type = None,
                    error_control = None,
                    binary_path = None,
                    load_order_group = None,
                    dependencies = None,
                    service_start_name = None,
                    password = None,
                    display_name = None
                )?;
                let service_type = match service_type {
                    Some(value) => Some(global.types.service_type.coerce(strand, &value).await?),
                    None => None,
                };
                let start_type = match start_type {
                    Some(value) => Some(convert::start_type_from_sym(
                        strand,
                        global,
                        Some(value),
                        dolang_vfs_winscm::StartType::DEMAND_START,
                    )?),
                    None => None,
                };
                let error_control = match error_control {
                    Some(value) => Some(convert::error_control_from_sym(
                        strand,
                        global,
                        Some(value),
                        dolang_vfs_winscm::ErrorControl::NORMAL,
                    )?),
                    None => None,
                };
                let dependencies = match dependencies {
                    Some(value) => Some(expect_str_iterable(strand, value, "dependencies").await?),
                    None => None,
                };
                let update = ServiceConfigUpdate {
                    service_type: service_type.map(Into::into),
                    start_type,
                    error_control,
                    binary_path: binary_path
                        .map(|value| expect_str(strand, value, "binary_path"))
                        .transpose()?,
                    load_order_group: load_order_group
                        .map(|value| expect_str(strand, value, "load_order_group"))
                        .transpose()?,
                    dependencies,
                    service_start_name: service_start_name
                        .map(|value| expect_str(strand, value, "service_start_name"))
                        .transpose()?,
                    password: password
                        .map(|value| expect_str(strand, value, "password"))
                        .transpose()?,
                    display_name: display_name
                        .map(|value| expect_str(strand, value, "display_name"))
                        .transpose()?,
                };
                let borrow = this.borrow(strand)?;
                let service = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                service.set_config(update).await.into_sys(strand)
            })
            .method(
                "wait_for_status_change",
                async move |this, strand, args, out| {
                    let global = strand.state::<Global<'v>>();
                    let ([mask], []) = unpack!(strand, args, 1, 0)?;
                    let mask = global.types.notify_mask.coerce(strand, &mask).await?;
                    let status = {
                        let borrow = this.borrow(strand)?;
                        let service = borrow
                            .0
                            .as_ref()
                            .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                        service
                            .wait_for_status_change(mask.into())
                            .await
                            .into_sys(strand)?
                    };
                    status::create_status(strand, global, status, out);
                    Ok(())
                },
            )
            .method("sec_desc", async move |this, strand, args, out| {
                let ([], [owner_arg, group_arg, dacl_arg, sacl_arg]) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    owner = None,
                    group = None,
                    dacl = None,
                    sacl = None
                )?;
                let mask = sec_desc_mask(strand, owner_arg, group_arg, dacl_arg, sacl_arg)?;
                let descriptor = {
                    let borrow = this.borrow(strand)?;
                    let service = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                    service.sec_desc(mask).await.into_sys(strand)?
                };
                dolang_ext_shell::create_sec_desc(strand, descriptor, out);
                Ok(())
            })
            .method("set_sec_desc", async move |this, strand, args, _out| {
                let ([descriptor], []) = unpack!(strand, args, 1, 0)?;
                let descriptor = dolang_ext_shell::sec_desc_from_value(strand, &descriptor)?;
                let borrow = this.borrow(strand)?;
                let service = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "service is closed"))?;
                service.set_sec_desc(&descriptor).await.into_sys(strand)
            })
    }
}
