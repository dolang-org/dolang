//! Symbol <-> enum conversions between Do `:SYMBOL:` values and
//! `dolang_vfs_winscm`'s wire types.
//!
//! Bitmask types (`ServiceAccess`, `ServiceType`, `NotifyMask`,
//! `ServiceControlsAccepted`) are handled as `Flags<F>` native types (see
//! `crate::access_mask`/`crate::flags`) rather than here. Discrete types
//! (`StartType`, `ErrorControl`, `ServiceControl`, `ServiceState`,
//! `ServiceStateFilter`) are a bare symbol both ways and stay as manual
//! conversions below.

use dolang::runtime::{Error, Output, Result, Slot, State, Strand};
use dolang_vfs_winscm::{
    ErrorControl, ServiceControl, ServiceState, ServiceStateFilter, StartType,
};

use crate::global::Global;

pub(crate) fn start_type_from_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
    default: StartType,
) -> Result<'v, 's, StartType> {
    let Some(slot) = slot else {
        return Ok(default);
    };
    let sym = slot
        .as_sym(strand.vm())
        .ok_or_else(|| Error::type_error(strand, "start_type: expected a symbol"))?;
    let syms = &global.syms;
    if sym == syms.boot_start {
        Ok(StartType::BOOT_START)
    } else if sym == syms.system_start {
        Ok(StartType::SYSTEM_START)
    } else if sym == syms.auto_start {
        Ok(StartType::AUTO_START)
    } else if sym == syms.demand_start {
        Ok(StartType::DEMAND_START)
    } else if sym == syms.disabled {
        Ok(StartType::DISABLED)
    } else {
        Err(Error::value(
            strand,
            "start_type: expected :BOOT_START:, :SYSTEM_START:, :AUTO_START:, :DEMAND_START:, or :DISABLED:",
        ))
    }
}

pub(crate) fn error_control_from_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
    default: ErrorControl,
) -> Result<'v, 's, ErrorControl> {
    let Some(slot) = slot else {
        return Ok(default);
    };
    let sym = slot
        .as_sym(strand.vm())
        .ok_or_else(|| Error::type_error(strand, "error_control: expected a symbol"))?;
    let syms = &global.syms;
    if sym == syms.ignore {
        Ok(ErrorControl::IGNORE)
    } else if sym == syms.normal {
        Ok(ErrorControl::NORMAL)
    } else if sym == syms.severe {
        Ok(ErrorControl::SEVERE)
    } else if sym == syms.critical {
        Ok(ErrorControl::CRITICAL)
    } else {
        Err(Error::value(
            strand,
            "error_control: expected :IGNORE:, :NORMAL:, :SEVERE:, or :CRITICAL:",
        ))
    }
}

pub(crate) fn service_control_from_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Slot<'v, '_>,
) -> Result<'v, 's, ServiceControl> {
    let sym = slot
        .as_sym(strand.vm())
        .ok_or_else(|| Error::type_error(strand, "control: expected a symbol"))?;
    let syms = &global.syms;
    if sym == syms.stop {
        Ok(ServiceControl::STOP)
    } else if sym == syms.pause {
        Ok(ServiceControl::PAUSE)
    } else if sym == syms.continue_ {
        Ok(ServiceControl::CONTINUE)
    } else if sym == syms.interrogate {
        Ok(ServiceControl::INTERROGATE)
    } else {
        Err(Error::value(
            strand,
            "control: expected :STOP:, :PAUSE:, :CONTINUE:, or :INTERROGATE:",
        ))
    }
}

pub(crate) fn service_state_filter_from_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
    default: ServiceStateFilter,
) -> Result<'v, 's, ServiceStateFilter> {
    let Some(slot) = slot else {
        return Ok(default);
    };
    let sym = slot
        .as_sym(strand.vm())
        .ok_or_else(|| Error::type_error(strand, "state_filter: expected a symbol"))?;
    let syms = &global.syms;
    if sym == syms.active {
        Ok(ServiceStateFilter::ACTIVE)
    } else if sym == syms.inactive {
        Ok(ServiceStateFilter::INACTIVE)
    } else if sym == syms.all {
        Ok(ServiceStateFilter::ALL)
    } else {
        Err(Error::value(
            strand,
            "state_filter: expected :ACTIVE:, :INACTIVE:, or :ALL:",
        ))
    }
}

pub(crate) fn service_state_to_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    state: ServiceState,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let syms = &global.syms;
    let sym = if state == ServiceState::STOPPED {
        syms.stopped
    } else if state == ServiceState::START_PENDING {
        syms.start_pending
    } else if state == ServiceState::STOP_PENDING {
        syms.stop_pending
    } else if state == ServiceState::RUNNING {
        syms.running
    } else if state == ServiceState::CONTINUE_PENDING {
        syms.continue_pending
    } else if state == ServiceState::PAUSE_PENDING {
        syms.pause_pending
    } else if state == ServiceState::PAUSED {
        syms.paused
    } else {
        // Defensive: a live SCM is not something this extension controls,
        // so fall back to the raw code rather than erroring on an
        // unrecognized state, same as `dolang-ext-winreg::value_entry`'s
        // `kind_to_do` does for an unrecognized `REG_*` kind.
        Output::set(strand, out, i128::from(state.0));
        return Ok(());
    };
    Output::set(strand, out, sym);
    Ok(())
}

pub(crate) fn start_type_to_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: StartType,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let syms = &global.syms;
    let sym = if value == StartType::BOOT_START {
        syms.boot_start
    } else if value == StartType::SYSTEM_START {
        syms.system_start
    } else if value == StartType::AUTO_START {
        syms.auto_start
    } else if value == StartType::DEMAND_START {
        syms.demand_start
    } else if value == StartType::DISABLED {
        syms.disabled
    } else {
        Output::set(strand, out, i128::from(value.0));
        return Ok(());
    };
    Output::set(strand, out, sym);
    Ok(())
}

pub(crate) fn error_control_to_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: ErrorControl,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let syms = &global.syms;
    let sym = if value == ErrorControl::IGNORE {
        syms.ignore
    } else if value == ErrorControl::NORMAL {
        syms.normal
    } else if value == ErrorControl::SEVERE {
        syms.severe
    } else if value == ErrorControl::CRITICAL {
        syms.critical
    } else {
        Output::set(strand, out, i128::from(value.0));
        return Ok(());
    };
    Output::set(strand, out, sym);
    Ok(())
}
