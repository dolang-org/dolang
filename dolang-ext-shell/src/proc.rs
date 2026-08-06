use dolang::{
    compile::Compiler,
    runtime::{
        Error, Instance, Object, Output, Result, Slot, State, Strand, Value,
        object::{TypeBuilder, fmt},
        strand::Redirect,
        unpack,
        value::{TypeObject, View},
        vm::Builder,
    },
};
use dolang_vfs::Signal;

use crate::{
    global::Global,
    io_mode::{ValueEncoding, encode_value, strip_line_ending},
    local::TerminationPolicy,
    time::coerce_duration,
};

fn parse_signal<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, Signal> {
    parse_signal_with_number(strand, value, false)
}

fn parse_signal_with_number<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    allow_number: bool,
) -> Result<'v, 's, Signal> {
    if let Some(signal) = value.as_int(strand) {
        if !allow_number {
            return Err(Error::value(
                strand,
                "numeric signals are only valid in per-launch process policies",
            ));
        }
        let signal = i32::try_from(signal)
            .ok()
            .filter(|signal| *signal > 0)
            .ok_or_else(|| Error::value(strand, "signal must be a positive 32-bit integer"))?;
        return Ok(Signal::Number(signal));
    }
    let Some(signal) = value.as_sym(strand) else {
        return Err(Error::type_error(strand, "signal must be a Sym"));
    };
    let signal = signal.as_str(strand.vm());
    let signal = signal.strip_prefix("SIG").unwrap_or(signal);
    match signal {
        "HUP" => Ok(Signal::Hup),
        "INT" => Ok(Signal::Int),
        "QUIT" => Ok(Signal::Quit),
        "ILL" => Ok(Signal::Ill),
        "TRAP" => Ok(Signal::Trap),
        "ABRT" | "IOT" => Ok(Signal::Abrt),
        "EMT" => Ok(Signal::Emt),
        "FPE" => Ok(Signal::Fpe),
        "KILL" => Ok(Signal::Kill),
        "BUS" => Ok(Signal::Bus),
        "SEGV" => Ok(Signal::Segv),
        "SYS" | "UNUSED" => Ok(Signal::Sys),
        "PIPE" => Ok(Signal::Pipe),
        "ALRM" => Ok(Signal::Alrm),
        "TERM" => Ok(Signal::Term),
        "URG" => Ok(Signal::Urg),
        "STOP" => Ok(Signal::Stop),
        "TSTP" => Ok(Signal::Tstp),
        "CONT" => Ok(Signal::Cont),
        "CHLD" | "CLD" => Ok(Signal::Chld),
        "TTIN" => Ok(Signal::Ttin),
        "TTOU" => Ok(Signal::Ttou),
        "IO" | "POLL" => Ok(Signal::Io),
        "XCPU" => Ok(Signal::Xcpu),
        "XFSZ" => Ok(Signal::Xfsz),
        "VTALRM" => Ok(Signal::Vtalrm),
        "PROF" => Ok(Signal::Prof),
        "WINCH" => Ok(Signal::Winch),
        "INFO" => Ok(Signal::Info),
        "USR1" => Ok(Signal::Usr1),
        "USR2" => Ok(Signal::Usr2),
        "STKFLT" => Ok(Signal::Stkflt),
        "PWR" => Ok(Signal::Pwr),
        "THR" => Ok(Signal::Thr),
        "LIBRT" => Ok(Signal::Librt),
        _ => Err(Error::value(
            strand,
            "unknown signal name; expected an uppercase symbol such as :TERM:",
        )),
    }
}

fn apply_policy_values<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    mut policy: TerminationPolicy,
    signal: Option<&Value<'v>>,
    grace: Option<&Value<'v>>,
    force: Option<&Value<'v>>,
) -> Result<'v, 's, TerminationPolicy> {
    if let Some(signal) = signal {
        policy.signal = parse_signal(strand, signal)?;
    }
    if let Some(grace) = grace {
        policy.grace = parse_grace(strand, global, grace)?;
    }
    if let Some(force) = force {
        policy.force = force
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "force must be a Bool"))?;
    }
    Ok(policy)
}

fn parse_grace<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, std::time::Duration> {
    if global.types.duration.cast(value).is_none() && value.as_f64(strand).is_none() {
        return Err(Error::type_error(
            strand,
            "termination grace must be a Duration or Float",
        ));
    }
    coerce_duration(strand, global, value, "termination grace")
}

pub(crate) fn parse_policy_dict<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
    base: TerminationPolicy,
    allow_signal: bool,
) -> Result<'v, 's, TerminationPolicy> {
    let View::Dict(dict) = value.view(strand) else {
        return Err(Error::type_error(strand, "policy must be a Dict"));
    };
    let mut policy = base;
    let mut pairs = dict.pairs();
    strand.with_slots_sync(|strand, [mut key, mut value]| {
        while pairs.next(strand, &mut key, &mut value)? {
            let key = key
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "policy keys must be symbols"))?;
            if key == global.syms.signal {
                if !allow_signal {
                    return Err(Error::value(
                        strand,
                        "signal cannot be overridden for a Windows process",
                    ));
                }
                policy.signal = parse_signal_with_number(strand, &value, true)?;
            } else if key == global.syms.grace {
                policy.grace = parse_grace(strand, global, &value)?;
            } else if key == global.syms.force {
                policy.force = value
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "policy force must be a Bool"))?;
            } else {
                return Err(Error::value(
                    strand,
                    format!("unknown process policy key: {}", key.as_str(strand.vm())),
                ));
            }
        }
        Ok(policy)
    })
}

pub(crate) fn vfs_policy(policy: &TerminationPolicy) -> dolang_vfs::TerminationPolicy {
    dolang_vfs::TerminationPolicy {
        signal: policy.signal,
        grace: policy.grace,
        force: policy.force,
    }
}

/// Capture output from a subprocess.
pub(crate) struct Capture(String);

impl Capture {
    pub(crate) fn new() -> Self {
        Self(String::new())
    }

    pub(crate) fn append(&mut self, value: &str) {
        self.0.push_str(value);
    }
}

impl Default for Capture {
    fn default() -> Self {
        Self::new()
    }
}

impl<'v> Object<'v> for Capture {
    const NAME: &'v str = "Capture";
    const MODULE: &'v str = "proc";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Sink)
    }

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<capture>")
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let local = global.local.get(strand);
        let bytes = encode_value(
            strand,
            &value,
            local.io_mode(),
            ValueEncoding::Display,
            local.target().operating_system,
        )?;
        let value = std::str::from_utf8(&bytes)
            .map_err(|_| Error::runtime(strand, "sub: captured invalid UTF-8"))?;
        let mut capture = this.borrow_mut(strand)?;
        capture.append(value);
        Ok(())
    }
}

pub(crate) fn configure_compiler<'a>(compiler: &mut Compiler<'a>) {
    compiler
        .prelude()
        .import_items("proc")
        .items(["sub"])
        .commit()
        .import_module_with_name("proc.run", "run");
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let capture_ty = global.types.capture;
    let trim = builder.sym("trim");

    builder
        .module("proc")
        .function("with_policy", async move |strand, args, out| {
            let signal_sym = global.syms.signal;
            let grace_sym = global.syms.grace;
            let force_sym = global.syms.force;
            let ([func], [signal, grace, force], rest) = unpack!(
                strand,
                args,
                1,
                0,
                signal_sym = None,
                grace_sym = None,
                force_sym = None,
                ...
            )?;
            let old_policy = global.local.get(strand).termination_policy();
            let policy = apply_policy_values(
                strand,
                global,
                old_policy.clone(),
                signal.as_deref(),
                grace.as_deref(),
                force.as_deref(),
            )?;
            global.local.get(strand).replace_termination_policy(policy);
            let result = func.call(strand, rest, out).await;
            global
                .local
                .get(strand)
                .replace_termination_policy(old_policy);
            result
        })
        .function_with_slots("sub", async move |strand, args, out, [mut cap, tmp]| {
            let ([func], [trim], rest) = unpack!(strand, args, 1, 0, trim = None, ...)?;
            let trim = trim.map(|v| v.to_bool(strand)).unwrap_or(true);
            capture_ty.create(strand, Capture::new(), &mut cap);
            Redirect::new(strand)
                .output(&cap)
                .enter(async move |strand| func.call(strand, rest, tmp).await)
                .await?;
            capture_ty
                .cast(&cap)
                .unwrap()
                .enter_sync(strand, |strand, inst| {
                    let capture = inst.borrow(strand)?;
                    let mut value = capture.0.as_str();
                    if trim {
                        value = strip_line_ending(value)
                    }
                    Output::set(strand, out, value);
                    Ok(())
                })
        })
        .value("Error", global.types.proc_error)
        .commit();
}
