//! `winscm.ScManager` — an open handle to the Service Control Manager
//! database, and the `winscm.open` entry point that bootstraps one.

use dolang::runtime::{
    Args, Error, Object, Result, Slot, State, Strand, call, method,
    object::{FlagsTypeExt, TypeBuilder},
    unpack,
    vm::Builder,
};
use dolang_ext_shell::ResultExt;
use dolang_vfs_winscm::{CreateServiceOptions, ErrorControl, ServiceStateFilter, StartType};

use crate::{
    access_mask::{ManagerAccessMask, ServiceAccessMask},
    convert,
    flags::ServiceType,
    global::Global,
    service::finish_open_service,
};

pub(crate) struct ScManager(pub(crate) Option<dolang_vfs_winscm::ScManager>);

/// Parses the `access:` keyword argument of `winscm.open`, accepting an
/// existing `winscm.ManagerAccessMask` instance, a bare symbol, or an
/// iterable of symbols whose bits are OR'd together. Defaults to
/// `:SC_MANAGER_CONNECT:` when omitted.
async fn manager_access_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
) -> Result<'v, 's, ManagerAccessMask> {
    match slot {
        Some(slot) => global.types.manager_access_mask.coerce(strand, &slot).await,
        None => Ok(ManagerAccessMask::SC_MANAGER_CONNECT),
    }
}

/// Parses the `access:` keyword argument of `ScManager.open_service`/
/// `.create_service`. Defaults to `:SERVICE_QUERY_STATUS:` when omitted.
async fn service_access_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
) -> Result<'v, 's, ServiceAccessMask> {
    match slot {
        Some(slot) => global.types.service_access_mask.coerce(strand, &slot).await,
        None => Ok(ServiceAccessMask::SERVICE_QUERY_STATUS),
    }
}

/// Parses a `service_type:` keyword argument (`ScManager.create_service`/
/// `.enumerate_services`). `default` is returned when the argument is
/// omitted.
async fn service_type_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
    default: ServiceType,
) -> Result<'v, 's, ServiceType> {
    match slot {
        Some(slot) => global.types.service_type.coerce(strand, &slot).await,
        None => Ok(default),
    }
}

pub(crate) fn expect_str<'v, 's>(
    strand: &mut Strand<'v, 's>,
    slot: Slot<'v, '_>,
    what: &str,
) -> Result<'v, 's, String> {
    slot.as_str(strand)
        .map(|s| s.to_string())
        .ok_or_else(|| Error::type_error(strand, format!("{what}: expected Str")))
}

pub(crate) async fn expect_str_iterable<'v, 's>(
    strand: &mut Strand<'v, 's>,
    slot: Slot<'v, '_>,
    what: &str,
) -> Result<'v, 's, Vec<String>> {
    strand
        .with_slots(async move |strand, [mut iter, mut item]| {
            slot.iter(strand, &mut iter).await?;
            let mut values = Vec::new();
            while iter.next(strand, &mut item).await? {
                values.push(expect_str(strand, Slot::reborrow(&mut item), what)?);
            }
            Ok(values)
        })
        .await
}

/// Wraps a freshly opened [`dolang_vfs_winscm::ScManager`] into a
/// `winscm.ScManager`. With a trailing block, the block is called with the
/// handle and the manager is auto-closed afterward; without one, the
/// handle is returned directly and the caller owns it. Mirrors
/// `dolang-ext-winreg::key::finish_open`.
async fn finish_open_manager<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    inner: dolang_vfs_winscm::ScManager,
    block: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if let Some(block) = block {
        strand
            .with_slots(async move |strand, [mut handle, mut tmp]| {
                global
                    .types
                    .manager
                    .create(strand, ScManager(Some(inner)), &mut handle);
                let result = call!(strand, block, out, &handle).await;
                let _ = method!(strand, &handle, global.syms.close, &mut tmp).await;
                result
            })
            .await
    } else {
        global
            .types
            .manager
            .create(strand, ScManager(Some(inner)), out);
        Ok(())
    }
}

/// `winscm.open` — bootstraps a [`ScManager`] handle.
async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let access_sym = global.syms.access;
    let ([], [block, access]) = unpack!(strand, args, 0, 1, access_sym = None)?;
    let access = manager_access_from_value(strand, global, access).await?;
    let vfs = dolang_ext_shell::vfs(strand);
    let inner = dolang_vfs_winscm::ScManager::open(&vfs, access.into())
        .await
        .into_sys(strand)?;
    finish_open_manager(strand, global, inner, block, out).await
}

impl<'v> Object<'v> for ScManager {
    const NAME: &'v str = "ScManager";
    const MODULE: &'v str = "winscm";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("open_service", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let access_sym = global.syms.access;
                let ([name], [block, access]) = unpack!(strand, args, 1, 1, access_sym = None)?;
                let name = expect_str(strand, name, "name")?;
                let access = service_access_from_value(strand, global, access).await?;
                let inner = {
                    let borrow = this.borrow(strand)?;
                    let manager = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "manager is closed"))?;
                    manager
                        .open_service(&name, access.into())
                        .await
                        .into_sys(strand)?
                };
                finish_open_service(strand, global, inner, block, out).await
            })
            .method("create_service", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let service_type_sym = global.syms.service_type;
                let start_type_sym = global.syms.start_type;
                let error_control_sym = global.syms.error_control;
                let access_sym = global.syms.access;
                let load_order_group_sym = global.syms.load_order_group;
                let dependencies_sym = global.syms.dependencies;
                let service_start_name_sym = global.syms.service_start_name;
                let password_sym = global.syms.password;
                let (
                    [name, display_name, binary_path],
                    [
                        block,
                        service_type,
                        start_type,
                        error_control,
                        access,
                        load_order_group,
                        dependencies,
                        service_start_name,
                        password,
                    ],
                ) = unpack!(
                    strand,
                    args,
                    3,
                    1,
                    service_type_sym = None,
                    start_type_sym = None,
                    error_control_sym = None,
                    access_sym = None,
                    load_order_group_sym = None,
                    dependencies_sym = None,
                    service_start_name_sym = None,
                    password_sym = None
                )?;
                let name = expect_str(strand, name, "name")?;
                let display_name = expect_str(strand, display_name, "display_name")?;
                let binary_path = expect_str(strand, binary_path, "binary_path")?;
                let service_type = service_type_from_value(
                    strand,
                    global,
                    service_type,
                    ServiceType::WIN32_OWN_PROCESS,
                )
                .await?;
                let start_type = convert::start_type_from_sym(
                    strand,
                    global,
                    start_type,
                    StartType::DEMAND_START,
                )?;
                let error_control = convert::error_control_from_sym(
                    strand,
                    global,
                    error_control,
                    ErrorControl::NORMAL,
                )?;
                let access = service_access_from_value(strand, global, access).await?;
                let dependencies = match dependencies {
                    Some(value) => expect_str_iterable(strand, value, "dependencies").await?,
                    None => Vec::new(),
                };
                let options = CreateServiceOptions {
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
                };
                let inner = {
                    let borrow = this.borrow(strand)?;
                    let manager = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "manager is closed"))?;
                    manager
                        .create_service_with_options(
                            &name,
                            &display_name,
                            service_type.into(),
                            start_type,
                            error_control,
                            &binary_path,
                            options,
                            access.into(),
                        )
                        .await
                        .into_sys(strand)?
                };
                finish_open_service(strand, global, inner, block, out).await
            })
            .method(
                "enumerate_services",
                async move |this, strand, args, out| {
                    let global = strand.state::<Global<'v>>();
                    let service_type_sym = global.syms.service_type;
                    let state_filter_sym = global.syms.state_filter;
                    let ([], [service_type, state_filter]) = unpack!(
                        strand,
                        args,
                        0,
                        0,
                        service_type_sym = None,
                        state_filter_sym = None
                    )?;
                    let service_type =
                        service_type_from_value(strand, global, service_type, ServiceType::WIN32)
                            .await?;
                    let state_filter = convert::service_state_filter_from_sym(
                        strand,
                        global,
                        state_filter,
                        ServiceStateFilter::ALL,
                    )?;
                    let entries = {
                        let borrow = this.borrow(strand)?;
                        let manager = borrow
                            .0
                            .as_ref()
                            .ok_or_else(|| Error::state_error(strand, "manager is closed"))?;
                        manager
                            .enumerate_services(service_type.into(), state_filter)
                            .await
                            .into_sys(strand)?
                    };
                    global.types.services.create_with_annex(
                        strand,
                        crate::services::Services::new(),
                        crate::services::ServicesAnnex { global, entries },
                        out,
                    );
                    Ok(())
                },
            )
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let inner = this.borrow_mut(strand)?.0.take();
                match inner {
                    Some(inner) => inner.close().await.into_sys(strand),
                    None => Ok(()),
                }
            })
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("winscm")
        .value("ScManager", global.types.manager)
        .value("Service", global.types.service)
        .value("ServiceConfig", global.types.config)
        .value("Status", global.types.status)
        .value("ServiceInfo", global.types.service_entry)
        .value("ManagerAccessMask", global.types.manager_access_mask)
        .value("ServiceAccessMask", global.types.service_access_mask)
        .value("ServiceType", global.types.service_type)
        .value("NotifyMask", global.types.notify_mask)
        .value("ServiceControlsAccepted", global.types.controls_accepted)
        .function("open", async move |strand, args, out| {
            open(strand, global, args, out).await
        })
        .commit();
}
