use std::time::Duration;

use dolang::runtime::{
    Error, Object, Output, Result, Slot, State, Strand, Value, object::TypeBuilder, unpack,
    value::Nil, vm::ModuleBuilder,
};
use dolang_ext_shell::ResultExt;
use dolang_vfs_winnet::policy;

use crate::global::Global;

pub(crate) struct AccountPolicy;

fn make_policy<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    policy: policy::Policy,
    out: impl Output<'v>,
) {
    global
        .types
        .account_policy
        .create_with_annex(strand, AccountPolicy, policy, out);
}

fn optional_duration<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<u64>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    match value {
        Some(value) => dolang_ext_time::duration(strand, Duration::from_secs(value), out),
        None => {
            Output::set(strand, out, Nil);
            Ok(())
        }
    }
}

fn integer<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, u32> {
    let value = value
        .as_int(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{name} must be an Int")))?;
    u32::try_from(value).map_err(|_| Error::value(strand, format!("{name} is out of range")))
}

fn duration<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, u64> {
    let value = dolang_ext_time::as_duration(strand, value)
        .ok_or_else(|| Error::type_error(strand, format!("{name} must be a time.Duration")))?;
    if value.subsec_nanos() != 0 {
        return Err(Error::value(
            strand,
            format!("{name} must be a whole number of seconds"),
        ));
    }
    if value.as_secs() >= u64::from(u32::MAX) {
        return Err(Error::value(strand, format!("{name} is out of range")));
    }
    Ok(value.as_secs())
}

impl<'v> Object<'v> for AccountPolicy {
    const NAME: &'v str = "AccountPolicy";
    const MODULE: &'v str = "winnet";
    type Annex = policy::Policy;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("min_password_length", |this, strand, out| {
                Output::set(strand, out, this.annex().min_password_length());
                Ok(())
            })
            .get("max_password_age", |this, strand, out| {
                optional_duration(strand, this.annex().max_password_age(), out)
            })
            .get("min_password_age", |this, strand, out| {
                dolang_ext_time::duration(
                    strand,
                    Duration::from_secs(this.annex().min_password_age()),
                    out,
                )
            })
            .get("force_logoff", |this, strand, out| {
                optional_duration(strand, this.annex().force_logoff(), out)
            })
            .get("password_history_length", |this, strand, out| {
                Output::set(strand, out, this.annex().password_history_length());
                Ok(())
            })
            .get("lockout_duration", |this, strand, out| {
                dolang_ext_time::duration(
                    strand,
                    Duration::from_secs(this.annex().lockout_duration()),
                    out,
                )
            })
            .get("lockout_observation_window", |this, strand, out| {
                dolang_ext_time::duration(
                    strand,
                    Duration::from_secs(this.annex().lockout_observation_window()),
                    out,
                )
            })
            .get("lockout_threshold", |this, strand, out| {
                Output::set(strand, out, this.annex().lockout_threshold());
                Ok(())
            })
    }
}

pub(crate) fn configure_module<'v, 'a>(
    module: ModuleBuilder<'v, 'a>,
    global: State<'v, Global<'v>>,
) -> ModuleBuilder<'v, 'a> {
    module
        .value("AccountPolicy", global.types.account_policy)
        .function("account_policy", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let policy = policy::get(&dolang_ext_shell::vfs(strand))
                .await
                .into_sys(strand)?;
            make_policy(strand, global, policy, out);
            Ok(())
        })
        .function("update_account_policy", async move |strand, args, out| {
            let min_password_length_sym = global.syms.min_password_length;
            let max_password_age_sym = global.syms.max_password_age;
            let min_password_age_sym = global.syms.min_password_age;
            let force_logoff_sym = global.syms.force_logoff;
            let password_history_length_sym = global.syms.password_history_length;
            let lockout_duration_sym = global.syms.lockout_duration;
            let lockout_observation_window_sym = global.syms.lockout_observation_window;
            let lockout_threshold_sym = global.syms.lockout_threshold;
            let (
                [],
                [
                    min_length,
                    max_age,
                    min_age,
                    force_logoff,
                    history,
                    lockout_duration,
                    observation,
                    threshold,
                ],
            ) = unpack!(
                strand,
                args,
                0,
                0,
                min_password_length_sym = None,
                max_password_age_sym = None,
                min_password_age_sym = None,
                force_logoff_sym = None,
                password_history_length_sym = None,
                lockout_duration_sym = None,
                lockout_observation_window_sym = None,
                lockout_threshold_sym = None
            )?;
            let mut update = policy::Update::default();
            if let Some(value) = min_length {
                update =
                    update.min_password_length(integer(strand, &value, "min_password_length")?);
            }
            if let Some(value) = max_age {
                update = update.max_password_age(if value.is_nil() {
                    None
                } else {
                    Some(duration(strand, &value, "max_password_age")?)
                });
            }
            if let Some(value) = min_age {
                update = update.min_password_age(duration(strand, &value, "min_password_age")?);
            }
            if let Some(value) = force_logoff {
                update = update.force_logoff(if value.is_nil() {
                    None
                } else {
                    Some(duration(strand, &value, "force_logoff")?)
                });
            }
            if let Some(value) = history {
                update = update.password_history_length(integer(
                    strand,
                    &value,
                    "password_history_length",
                )?);
            }
            if let Some(value) = lockout_duration {
                update = update.lockout_duration(duration(strand, &value, "lockout_duration")?);
            }
            if let Some(value) = observation {
                update = update.lockout_observation_window(duration(
                    strand,
                    &value,
                    "lockout_observation_window",
                )?);
            }
            if let Some(value) = threshold {
                update = update.lockout_threshold(integer(strand, &value, "lockout_threshold")?);
            }
            let policy = policy::update(&dolang_ext_shell::vfs(strand), update)
                .await
                .into_sys(strand)?;
            make_policy(strand, global, policy, out);
            Ok(())
        })
}
