use std::{fmt, io, time::SystemTime};

use dolang::{
    compile::Compiler,
    runtime::{
        Error, Instance, Object, Output, Result, Slot, State, Strand, call, error::ResultExt,
        object::TypeBuilder, unpack, vm::Builder,
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::global::Global;

const NANOS_PER_SEC_I128: i128 = 1_000_000_000;
const NANOS_PER_SEC_F64: f64 = 1_000_000_000.0;

pub(crate) struct DateTime;

pub(crate) struct DateTimeAnnex {
    total_nanos: i128,
}

pub(crate) struct Duration;

pub(crate) struct DurationAnnex {
    total_nanos: i128,
}

impl DateTimeAnnex {
    fn from_total_nanos(total_nanos: i128) -> Self {
        Self { total_nanos }
    }

    fn total_nanos(&self) -> i128 {
        self.total_nanos
    }

    pub(crate) fn from_system_time(time: SystemTime) -> io::Result<Self> {
        match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => {
                let total_nanos = i128::from(duration.as_secs())
                    .checked_mul(NANOS_PER_SEC_I128)
                    .and_then(|secs| secs.checked_add(i128::from(duration.subsec_nanos())))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow")
                    })?;
                Ok(Self { total_nanos })
            }
            Err(err) => {
                let duration = err.duration();
                let total_nanos = i128::from(duration.as_secs())
                    .checked_mul(NANOS_PER_SEC_I128)
                    .and_then(|secs| secs.checked_add(i128::from(duration.subsec_nanos())))
                    .and_then(|total_nanos| total_nanos.checked_neg())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow")
                    })?;
                Ok(Self { total_nanos })
            }
        }
    }

    pub(crate) fn to_system_time(&self) -> io::Result<SystemTime> {
        if self.total_nanos >= 0 {
            let total_nanos = self.total_nanos as u128;
            let secs = total_nanos / (NANOS_PER_SEC_I128 as u128);
            let nanos = (total_nanos % (NANOS_PER_SEC_I128 as u128)) as u32;
            let secs = u64::try_from(secs)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow"))?;
            SystemTime::UNIX_EPOCH
                .checked_add(std::time::Duration::new(secs, nanos))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow"))
        } else {
            let total_nanos = self.total_nanos.unsigned_abs();
            let secs = total_nanos / (NANOS_PER_SEC_I128 as u128);
            let nanos = (total_nanos % (NANOS_PER_SEC_I128 as u128)) as u32;
            let secs = u64::try_from(secs)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow"))?;
            let duration = std::time::Duration::new(secs, nanos);
            SystemTime::UNIX_EPOCH
                .checked_sub(duration)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow"))
        }
    }
}

impl DurationAnnex {
    fn from_total_nanos(total_nanos: i128) -> Self {
        Self { total_nanos }
    }

    fn secs(&self) -> f64 {
        self.total_nanos as f64 / NANOS_PER_SEC_F64
    }

    fn write_seconds(&self, w: &mut dyn fmt::Write) -> fmt::Result {
        if self.total_nanos == 0 {
            return write!(w, "0s");
        }

        if self.total_nanos < 0 {
            write!(w, "-")?;
        }

        let abs_nanos = self.total_nanos.unsigned_abs();
        let secs = abs_nanos / (NANOS_PER_SEC_I128 as u128);
        let nanos = abs_nanos % (NANOS_PER_SEC_I128 as u128);

        if nanos == 0 {
            return write!(w, "{}s", secs);
        }

        let mut frac = format!("{:09}", nanos);
        while frac.ends_with('0') {
            frac.pop();
        }

        write!(w, "{}.{}s", secs, frac)
    }

    fn to_std_duration<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, std::time::Duration> {
        if self.total_nanos < 0 {
            return Err(Error::runtime(
                strand,
                "sleep duration must be non-negative",
            ));
        }
        let total_nanos = self.total_nanos as u128;
        let secs = total_nanos / (NANOS_PER_SEC_I128 as u128);
        let nanos = total_nanos % (NANOS_PER_SEC_I128 as u128);
        let secs = u64::try_from(secs).map_err(|_| Error::overflow(strand))?;
        Ok(std::time::Duration::new(secs, nanos as u32))
    }
}

fn float_seconds_to_nanos<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: f64,
    context: &str,
) -> Result<'v, 's, i128> {
    if !value.is_finite() {
        return Err(Error::type_error(
            strand,
            format!("{context}: expected finite float seconds"),
        ));
    }

    let scaled = value * NANOS_PER_SEC_F64;
    if !scaled.is_finite() || scaled < i128::MIN as f64 || scaled > i128::MAX as f64 {
        return Err(Error::overflow(strand));
    }

    Ok(scaled.round() as i128)
}

fn value_to_unix_seconds_nanos<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &dolang::runtime::Value<'v>,
    context: &str,
) -> Result<'v, 's, i128> {
    if let Some(value) = value.as_int(strand) {
        return value
            .checked_mul(NANOS_PER_SEC_I128)
            .ok_or_else(|| Error::overflow(strand));
    }

    if let Some(value) = value.as_f64(strand) {
        return float_seconds_to_nanos(strand, value, context);
    }

    Err(Error::type_error(
        strand,
        format!("{context}: expected int or float seconds"),
    ))
}

fn format_datetime_rfc3339<'v, 's>(
    strand: &mut Strand<'v, 's>,
    datetime: &DateTimeAnnex,
) -> Result<'v, 's, String> {
    let datetime = OffsetDateTime::from_unix_timestamp_nanos(datetime.total_nanos())
        .map_err(|_| Error::runtime(strand, "invalid DateTime"))?;
    datetime
        .format(&Rfc3339)
        .map_err(|err| Error::runtime(strand, err))
}

fn coerce_sleep_duration<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Slot<'v, '_>,
) -> Result<'v, 's, std::time::Duration> {
    if let Some(duration) = global.types.duration.downcast(&value) {
        return duration.annex().to_std_duration(strand);
    }

    if let Some(i) = value.as_int(strand) {
        if i < 0 {
            return Err(Error::runtime(
                strand,
                "sleep duration must be non-negative",
            ));
        }
        let secs = u64::try_from(i).map_err(|_| Error::overflow(strand))?;
        return Ok(std::time::Duration::from_secs(secs));
    }

    if let Some(f) = value.as_f64(strand) {
        if !f.is_finite() || f < 0.0 {
            return Err(Error::runtime(
                strand,
                "sleep duration must be a non-negative finite number",
            ));
        }
        return Ok(std::time::Duration::from_secs_f64(f));
    }

    Err(Error::type_error(
        strand,
        "sleep argument must be a Duration, integer, or float",
    ))
}

pub(crate) fn datetime_to_system_time<'v, 's>(
    strand: &mut Strand<'v, 's>,
    date_time: dolang::runtime::Type<'v, DateTime>,
    value: &dolang::runtime::Value<'v>,
) -> Result<'v, 's, SystemTime> {
    let datetime = date_time
        .downcast(value)
        .ok_or_else(|| Error::type_error(strand, "expected DateTime"))?;
    datetime.annex().to_system_time().into_do(strand)
}

pub(crate) fn create_datetime<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    total_nanos: i128,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let annex = DateTimeAnnex::from_total_nanos(total_nanos);
    global
        .types
        .date_time
        .create_with_annex(strand, DateTime, annex, out);
    Ok(())
}

impl<'v> Object<'v> for DateTime {
    const NAME: &'v str = "DateTime";
    const MODULE: &'v str = "time";
    type Annex = DateTimeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let nanos_sym = builder.sym("nanos");
        builder
            .get("unix_secs", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    this.annex().total_nanos() as f64 / NANOS_PER_SEC_F64,
                );
                Ok(())
            })
            .get("unix_nanos", |this, strand, out| {
                Output::set(strand, out, this.annex().total_nanos());
                Ok(())
            })
            .type_method("now", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = DateTimeAnnex::from_system_time(SystemTime::now()).into_do(strand)?;
                this.create_with_annex(strand, DateTime, annex, out);
                Ok(())
            })
            .type_method("from_unix", async move |this, strand, args, out| {
                let ([], [secs, nanos]) = unpack!(strand, args, 0, 1, nanos_sym = None)?;
                let nanos = nanos.map_or(Ok(None), |value| {
                    value
                        .as_int(strand)
                        .ok_or_else(|| Error::type_error(strand, "from_unix: expected int nanos"))
                        .map(Some)
                })?;
                let total_nanos = if let Some(secs) = secs {
                    let secs_nanos = value_to_unix_seconds_nanos(strand, &secs, "from_unix")?;
                    secs_nanos
                        .checked_add(nanos.unwrap_or(0))
                        .ok_or_else(|| Error::overflow(strand))?
                } else if let Some(nanos) = nanos {
                    nanos
                } else {
                    return Err(Error::type_error(
                        strand,
                        "from_unix: expected seconds, nanos, or both",
                    ));
                };
                this.create_with_annex(
                    strand,
                    DateTime,
                    DateTimeAnnex::from_total_nanos(total_nanos),
                    out,
                );
                Ok(())
            })
            .type_method("parse_rfc3339", async move |this, strand, args, out| {
                let ([text], []) = unpack!(strand, args, 1, 0)?;
                let text = text
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "parse_rfc3339: expected string"))?
                    .to_string();
                let datetime = OffsetDateTime::parse(&text, &Rfc3339)
                    .map_err(|err| Error::runtime(strand, err))?;
                let annex = DateTimeAnnex::from_total_nanos(datetime.unix_timestamp_nanos());
                this.create_with_annex(strand, DateTime, annex, out);
                Ok(())
            })
            .method("rfc3339", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let formatted = format_datetime_rfc3339(strand, &this.annex())?;
                Output::set(strand, out, formatted.as_str());
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let formatted = format_datetime_rfc3339(strand, &this.annex())?;
        write!(w, "{}", formatted).into_do(strand)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<DateTime ").into_do(strand)?;
        Self::display(this, strand, w)?;
        write!(w, ">").into_do(strand)
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.date_time.downcast(other) {
            Ok(this.annex().total_nanos == other.annex().total_nanos)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.date_time.downcast(other) {
            Ok(this.annex().total_nanos < other.annex().total_nanos)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn sub<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.date_time.downcast(other) {
            let left = this.annex().total_nanos;
            let right = other.annex().total_nanos;
            global.types.duration.create_with_annex(
                strand,
                Duration,
                DurationAnnex::from_total_nanos(left - right),
                out,
            );
            Ok(())
        } else {
            Err(Error::not_supported(strand))
        }
    }
}

impl<'v> Object<'v> for Duration {
    const NAME: &'v str = "Duration";
    const MODULE: &'v str = "time";
    type Annex = DurationAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("secs", |this, strand, out| {
                Output::set(strand, out, this.annex().secs());
                Ok(())
            })
            .get("nanos", |this, strand, out| {
                Output::set(strand, out, this.annex().total_nanos);
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        this.annex().write_seconds(w).into_do(strand)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<Duration ").into_do(strand)?;
        Self::display(this, strand, w)?;
        write!(w, ">").into_do(strand)
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.duration.downcast(other) {
            Ok(this.annex().total_nanos == other.annex().total_nanos)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.duration.downcast(other) {
            Ok(this.annex().total_nanos < other.annex().total_nanos)
        } else {
            Err(Error::not_supported(strand))
        }
    }
}

pub(crate) fn configure_compiler<'a>(_compiler: &mut Compiler<'a>) {}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("time")
        .function("sleep", async move |strand, args, _out| {
            let ([duration], []) = unpack!(strand, args, 1, 0)?;
            let duration = coerce_sleep_duration(strand, global, duration)?;
            tokio::time::sleep(duration).await;
            Ok(())
        })
        .function("timeout", async move |strand, args, out| {
            let ([duration, block], []) = unpack!(strand, args, 2, 0)?;
            let duration = coerce_sleep_duration(strand, global, duration)?;
            let interrupt = strand.interrupt_token().nested();
            let interrupt_clone = interrupt.clone();
            strand.spawn_task(async move {
                tokio::time::sleep(duration).await;
                interrupt_clone.timeout();
            });
            strand
                .with_interrupt_token(interrupt, async move |strand| {
                    call!(strand, block, out).await
                })
                .await
        })
        .value("DateTime", global.types.date_time)
        .value("Duration", global.types.duration)
        .commit();
}
