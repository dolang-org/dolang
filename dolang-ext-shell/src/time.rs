use std::{fmt, io, time::SystemTime};

use dolang::{
    compile::Compiler,
    runtime::{
        Error, Instance, Object, Output, Result, Slot, State, Strand, error::ResultExt,
        object::TypeBuilder, unpack, vm::Builder,
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::global::Global;

const NANOS_PER_SEC_I64: i64 = 1_000_000_000;
const NANOS_PER_SEC_I128: i128 = 1_000_000_000;

pub(crate) struct DateTime;

pub(crate) struct DateTimeAnnex {
    secs: i64,
    nanos: u32,
}

pub(crate) struct Duration;

pub(crate) struct DurationAnnex {
    total_nanos: i128,
}

impl DateTimeAnnex {
    fn normalize_unix_parts(secs: i64, nanos: i64) -> Option<(i64, u32)> {
        let carry = nanos.div_euclid(NANOS_PER_SEC_I64);
        let nanos = nanos.rem_euclid(NANOS_PER_SEC_I64) as u32;
        let secs = secs.checked_add(carry)?;
        Some((secs, nanos))
    }

    fn from_unix_parts(secs: i64, nanos: i64) -> Option<Self> {
        let (secs, nanos) = Self::normalize_unix_parts(secs, nanos)?;
        Some(Self { secs, nanos })
    }

    fn total_nanos(&self) -> i128 {
        i128::from(self.secs) * NANOS_PER_SEC_I128 + i128::from(self.nanos)
    }

    pub(crate) fn from_system_time(time: SystemTime) -> io::Result<Self> {
        match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => {
                let secs = i64::try_from(duration.as_secs()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow")
                })?;
                Ok(Self {
                    secs,
                    nanos: duration.subsec_nanos(),
                })
            }
            Err(err) => {
                let duration = err.duration();
                let secs = i64::try_from(duration.as_secs()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow")
                })?;
                if duration.subsec_nanos() == 0 {
                    let secs = 0i64.checked_sub(secs).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow")
                    })?;
                    Ok(Self { secs, nanos: 0 })
                } else {
                    let secs = 0i64
                        .checked_sub(secs)
                        .and_then(|v| v.checked_sub(1))
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow")
                        })?;
                    Ok(Self {
                        secs,
                        nanos: 1_000_000_000u32 - duration.subsec_nanos(),
                    })
                }
            }
        }
    }

    pub(crate) fn to_system_time(&self) -> io::Result<SystemTime> {
        if self.secs >= 0 {
            SystemTime::UNIX_EPOCH
                .checked_add(std::time::Duration::new(self.secs as u64, self.nanos))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow"))
        } else {
            let secs_abs = self.secs.unsigned_abs();
            let duration = if self.nanos == 0 {
                std::time::Duration::new(secs_abs, 0)
            } else {
                std::time::Duration::new(secs_abs - 1, 1_000_000_000u32 - self.nanos)
            };
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

    fn parts_floor(&self) -> Option<(i64, i64)> {
        let secs = self.total_nanos.div_euclid(NANOS_PER_SEC_I128);
        let nanos = self.total_nanos.rem_euclid(NANOS_PER_SEC_I128);
        let secs = i64::try_from(secs).ok()?;
        Some((secs, nanos as i64))
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
    secs: i64,
    nanos: i64,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let annex = DateTimeAnnex::from_unix_parts(secs, nanos)
        .ok_or_else(|| Error::runtime(strand, "invalid DateTime"))?;
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

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("seconds", |this, strand, out| {
                Output::set(strand, out, this.annex().secs);
                Ok(())
            })
            .get("nanoseconds", |this, strand, out| {
                Output::set(strand, out, i64::from(this.annex().nanos));
                Ok(())
            })
            .type_method("now", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = DateTimeAnnex::from_system_time(SystemTime::now()).into_do(strand)?;
                this.create_with_annex(strand, DateTime, annex, out);
                Ok(())
            })
            .type_method("from_unix", async move |this, strand, args, out| {
                let ([secs], [nanos]) = unpack!(strand, args, 1, 1)?;
                let secs = secs
                    .to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "from_unix: expected int seconds"))?;
                let nanos = match nanos {
                    Some(value) => value.to_i64(strand).map_err(|_| {
                        Error::type_error(strand, "from_unix: expected int nanoseconds")
                    })?,
                    None => 0,
                };
                let Some(annex) = DateTimeAnnex::from_unix_parts(secs, nanos) else {
                    return Err(Error::overflow(strand));
                };
                this.create_with_annex(strand, DateTime, annex, out);
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
                let nanos = datetime.unix_timestamp_nanos();
                let secs = nanos.div_euclid(NANOS_PER_SEC_I128);
                let nanos = nanos.rem_euclid(NANOS_PER_SEC_I128);
                let secs = i64::try_from(secs).map_err(|_| Error::overflow(strand))?;
                let annex = DateTimeAnnex::from_unix_parts(secs, nanos as i64)
                    .ok_or_else(|| Error::overflow(strand))?;
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
            Ok(this.annex().total_nanos() == other.annex().total_nanos())
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
            Ok(this.annex().total_nanos() < other.annex().total_nanos())
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
            let left = this.annex().total_nanos();
            let right = other.annex().total_nanos();
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
            .get("seconds", |this, strand, out| {
                let Some((secs, _)) = this.annex().parts_floor() else {
                    return Err(Error::overflow(strand));
                };
                Output::set(strand, out, secs);
                Ok(())
            })
            .get("nanoseconds", |this, strand, out| {
                let Some((_, nanos)) = this.annex().parts_floor() else {
                    return Err(Error::overflow(strand));
                };
                Output::set(strand, out, nanos);
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
        .value("DateTime", global.types.date_time)
        .value("Duration", global.types.duration)
        .commit();
}
