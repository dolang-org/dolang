use dolang::runtime::{
    Sym, Type,
    object::{FlagLike, Flags},
    vm::{Builder, Stateful},
};

use crate::{
    access_mask::{ManagerAccessMask, ServiceAccessMask},
    config::Config,
    flags::{NotifyMask, ServiceControlsAccepted, ServiceType},
    manager::ScManager,
    service::Service,
    service_info::ServiceEntry,
    services::Services,
    status::Status,
};

pub(crate) struct Types<'v> {
    pub(crate) manager: Type<'v, ScManager>,
    pub(crate) service: Type<'v, Service>,
    pub(crate) config: Type<'v, Config>,
    pub(crate) status: Type<'v, Status>,
    pub(crate) service_entry: Type<'v, ServiceEntry>,
    pub(crate) services: Type<'v, Services>,
    pub(crate) manager_access_mask: Type<'v, Flags<ManagerAccessMask>>,
    pub(crate) service_access_mask: Type<'v, Flags<ServiceAccessMask>>,
    pub(crate) service_type: Type<'v, Flags<ServiceType>>,
    pub(crate) notify_mask: Type<'v, Flags<NotifyMask>>,
    pub(crate) controls_accepted: Type<'v, Flags<ServiceControlsAccepted>>,
}

/// Symbols for the `:UPPER_CASE:` constants this extension accepts/reports
/// in place of dedicated enum types, plus method/keyword names.
///
/// Several symbols are deliberately shared between otherwise-distinct wire
/// types because they name the same real-world concept: `ServiceState` and
/// `NotifyMask` both use e.g. `:STOPPED:`/`:RUNNING:` (one reports the
/// current state, the other selects which transitions to be notified
/// about); `ServiceControl::STOP` and `ServiceControlsAccepted::STOP` both
/// use `:STOP:` (one requests the action, the other reports whether a
/// service currently accepts it). One interned `Sym` per name is correct
/// and sufficient in each case.
pub(crate) struct Syms<'v> {
    // Start type (discrete)
    pub(crate) boot_start: Sym<'v, 'v>,
    pub(crate) system_start: Sym<'v, 'v>,
    pub(crate) auto_start: Sym<'v, 'v>,
    pub(crate) demand_start: Sym<'v, 'v>,
    pub(crate) disabled: Sym<'v, 'v>,
    // Error control (discrete)
    pub(crate) ignore: Sym<'v, 'v>,
    pub(crate) normal: Sym<'v, 'v>,
    pub(crate) severe: Sym<'v, 'v>,
    pub(crate) critical: Sym<'v, 'v>,
    // Control codes (discrete, `ServiceControl`) — `:STOP:` is also used by
    // `ServiceControlsAccepted`'s own `Flags` type, which interns it again
    // independently (interning is idempotent, so this is the same `Sym`).
    pub(crate) stop: Sym<'v, 'v>,
    pub(crate) pause: Sym<'v, 'v>,
    pub(crate) continue_: Sym<'v, 'v>,
    pub(crate) interrogate: Sym<'v, 'v>,
    // Service state (discrete, `ServiceState`) — shared with `NotifyMask`'s
    // own `Flags` type for the names they have in common, same idempotent
    // re-interning as above.
    pub(crate) stopped: Sym<'v, 'v>,
    pub(crate) start_pending: Sym<'v, 'v>,
    pub(crate) stop_pending: Sym<'v, 'v>,
    pub(crate) running: Sym<'v, 'v>,
    pub(crate) continue_pending: Sym<'v, 'v>,
    pub(crate) pause_pending: Sym<'v, 'v>,
    pub(crate) paused: Sym<'v, 'v>,
    // Service state filter (discrete, enumerate_services)
    pub(crate) active: Sym<'v, 'v>,
    pub(crate) inactive: Sym<'v, 'v>,
    pub(crate) all: Sym<'v, 'v>,
    // Method/keyword names
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) access: Sym<'v, 'v>,
    pub(crate) service_type: Sym<'v, 'v>,
    pub(crate) start_type: Sym<'v, 'v>,
    pub(crate) error_control: Sym<'v, 'v>,
    pub(crate) state_filter: Sym<'v, 'v>,
    pub(crate) binary_path: Sym<'v, 'v>,
    pub(crate) load_order_group: Sym<'v, 'v>,
    pub(crate) dependencies: Sym<'v, 'v>,
    pub(crate) service_start_name: Sym<'v, 'v>,
    pub(crate) password: Sym<'v, 'v>,
    pub(crate) display_name: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let manager = builder.register_type::<ScManager>();
        let service = builder.register_type::<Service>();
        let config = builder.register_type::<Config>();
        let status = builder.register_type::<Status>();
        let service_entry = builder.register_type::<ServiceEntry>();
        let services = builder.register_type::<Services>();
        let manager_access_mask = ManagerAccessMask::register_type(builder);
        let service_access_mask = ServiceAccessMask::register_type(builder);
        let service_type = ServiceType::register_type(builder);
        let notify_mask = NotifyMask::register_type(builder);
        let controls_accepted = ServiceControlsAccepted::register_type(builder);

        Self {
            types: Types {
                manager,
                service,
                config,
                status,
                service_entry,
                services,
                manager_access_mask,
                service_access_mask,
                service_type,
                notify_mask,
                controls_accepted,
            },
            syms: Syms {
                boot_start: builder.sym("BOOT_START"),
                system_start: builder.sym("SYSTEM_START"),
                auto_start: builder.sym("AUTO_START"),
                demand_start: builder.sym("DEMAND_START"),
                disabled: builder.sym("DISABLED"),
                ignore: builder.sym("IGNORE"),
                normal: builder.sym("NORMAL"),
                severe: builder.sym("SEVERE"),
                critical: builder.sym("CRITICAL"),
                stop: builder.sym("STOP"),
                pause: builder.sym("PAUSE"),
                continue_: builder.sym("CONTINUE"),
                interrogate: builder.sym("INTERROGATE"),
                stopped: builder.sym("STOPPED"),
                start_pending: builder.sym("START_PENDING"),
                stop_pending: builder.sym("STOP_PENDING"),
                running: builder.sym("RUNNING"),
                continue_pending: builder.sym("CONTINUE_PENDING"),
                pause_pending: builder.sym("PAUSE_PENDING"),
                paused: builder.sym("PAUSED"),
                active: builder.sym("ACTIVE"),
                inactive: builder.sym("INACTIVE"),
                all: builder.sym("ALL"),
                close: builder.sym("close"),
                access: builder.sym("access"),
                service_type: builder.sym("service_type"),
                start_type: builder.sym("start_type"),
                error_control: builder.sym("error_control"),
                state_filter: builder.sym("state_filter"),
                binary_path: builder.sym("binary_path"),
                load_order_group: builder.sym("load_order_group"),
                dependencies: builder.sym("dependencies"),
                service_start_name: builder.sym("service_start_name"),
                password: builder.sym("password"),
                display_name: builder.sym("display_name"),
            },
        }
    }
}
