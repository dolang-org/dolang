//! `TimeZone` and `Zoned`: time zones, and instants seen in one.

use std::hash::{Hash, Hasher};

use dolang::runtime::{
    AllocExt, Error, Instance, Object, Output, Result, State, Strand, Value,
    object::TypeBuilder,
    object::fmt,
    unpack,
    value::{Nil, fmt::Format},
    with_instance,
};
use jiff::{
    Timestamp, Zoned as JiffZoned,
    civil::DateTime as CivilDateTime,
    fmt::strtime,
    tz::{Offset, TimeZone as JiffTimeZone},
};

use crate::{
    global::{Global, Tag},
    time::{
        Duration, DurationAnnex, coerce_component, coerce_month, create_date, create_datetime,
        datetime_to_unix_nanos, float_seconds_to_nanos, format_rfc3339, month_index, str_arg,
        timestamp, weekday_index,
    },
};

const NANOS_PER_SEC: i128 = 1_000_000_000;

pub(crate) struct TimeZone;

pub(crate) struct Zoned;

/// Reads a `zone:` argument: a `TimeZone`, defaulting to UTC.
pub(crate) fn zone_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Option<&Value<'v>>,
) -> Result<'v, 's, JiffTimeZone> {
    let Some(value) = value else {
        return Ok(JiffTimeZone::UTC);
    };
    let Some(tz) = global.types.time_zone.cast(value) else {
        return Err(Error::type_error(strand, "expected TimeZone"));
    };
    Ok(with_instance!(strand, tz, |tz| tz.annex().clone()))
}

pub(crate) fn create_zoned<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    zoned: JiffZoned,
    out: impl Output<'v>,
) {
    global
        .types
        .zoned
        .create_with_annex(strand, Zoned, zoned, out);
}

/// Formats a zoned instant with a strftime-style pattern.
pub(crate) fn format_zoned<'v, 's>(
    strand: &mut Strand<'v, 's>,
    zoned: &JiffZoned,
    pattern: &str,
) -> Result<'v, 's, String> {
    strtime::format(pattern, zoned).map_err(|err| Error::value(strand, err.to_string()))
}

fn create_offset_duration<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    offset: Offset,
    out: impl Output<'v>,
) {
    global.types.duration.create_with_annex(
        strand,
        Duration,
        DurationAnnex::from_total_nanos(i128::from(offset.seconds()) * NANOS_PER_SEC),
        out,
    );
}

/// Coerces a signed offset: a `Duration` or a number of seconds, which must be
/// whole and within ±25:59:59.
fn coerce_offset<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, Offset> {
    let nanos = if let Some(duration) = global.types.duration.cast(value) {
        with_instance!(strand, duration, |duration| duration.annex().total_nanos())
    } else if let Some(secs) = value.as_int(strand) {
        secs.checked_mul(NANOS_PER_SEC)
            .ok_or_else(|| Error::value(strand, "offset out of range"))?
    } else if let Some(secs) = value.as_f64(strand) {
        float_seconds_to_nanos(strand, secs, "offset")?
    } else {
        return Err(Error::type_error(
            strand,
            "offset must be a Duration, integer, or Float",
        ));
    };
    if nanos % NANOS_PER_SEC != 0 {
        return Err(Error::value(
            strand,
            "offset must be a whole number of seconds",
        ));
    }
    i32::try_from(nanos / NANOS_PER_SEC)
        .ok()
        .and_then(|secs| Offset::from_seconds(secs).ok())
        .ok_or_else(|| Error::value(strand, "offset out of range"))
}

/// Writes an offset as `±HH:MM`, with `:SS` when it has seconds.
fn format_offset(offset: Offset) -> String {
    let secs = offset.seconds();
    let sign = if secs < 0 { '-' } else { '+' };
    let secs = secs.unsigned_abs();
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    if seconds == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    }
}

/// Names a zone for display: its IANA name, else its fixed offset.
fn zone_label(tz: &JiffTimeZone) -> String {
    if let Some(name) = tz.iana_name() {
        name.to_owned()
    } else if let Ok(offset) = tz.to_fixed_offset() {
        format_offset(offset)
    } else {
        "unnamed".to_owned()
    }
}

/// Returns whether two zones give the same offset, abbreviation and DST flag at
/// every instant. Aliases, such as `US/Eastern` and `America/New_York`, are
/// the same zone.
fn same_rules(a: &JiffTimeZone, b: &JiffTimeZone) -> bool {
    if a.iana_name().is_some() && a.iana_name() == b.iana_name() {
        return true;
    }
    if let (Ok(a), Ok(b)) = (a.to_fixed_offset(), b.to_fixed_offset()) {
        return a == b;
    }
    let start = Timestamp::MIN;
    let (a_info, b_info) = (a.to_offset_info(start), b.to_offset_info(start));
    if (a_info.offset(), a_info.abbreviation(), a_info.dst())
        != (b_info.offset(), b_info.abbreviation(), b_info.dst())
    {
        return false;
    }
    let (mut a, mut b) = (a.following(start), b.following(start));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(a), Some(b))
                if (a.timestamp(), a.offset(), a.abbreviation(), a.dst())
                    == (b.timestamp(), b.offset(), b.abbreviation(), b.dst()) => {}
            _ => return false,
        }
    }
}

impl<'v> Object<'v> for TimeZone {
    const NAME: &'v str = "TimeZone";
    const MODULE: &'v str = "time";
    type Annex = JiffTimeZone;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let hour = builder.sym("hour");
        let minute = builder.sym("minute");
        let second = builder.sym("second");
        let nanosecond = builder.sym("nanosecond");
        let disambiguate = builder.sym("disambiguate");
        builder
            .type_method("local", async move |this, strand, args, out| {
                unpack!(strand, args =>);
                this.create_with_annex(strand, TimeZone, JiffTimeZone::system(), out);
                Ok(())
            })
            .type_method("utc", async move |this, strand, args, out| {
                unpack!(strand, args =>);
                this.create_with_annex(strand, TimeZone, JiffTimeZone::UTC, out);
                Ok(())
            })
            .type_method("get", async move |this, strand, args, out| {
                unpack!(strand, args => name);
                let name = str_arg(strand, &name, "get: expected Str name")?;
                let tz = JiffTimeZone::get(&name)
                    .map_err(|_| Error::data(strand, format!("unknown time zone: {name}")))?;
                this.create_with_annex(strand, TimeZone, tz, out);
                Ok(())
            })
            .type_method("fixed", async move |this, strand, args, out| {
                unpack!(strand, args => offset);
                let global = strand.state::<Tag>();
                let offset = coerce_offset(strand, global, &offset)?;
                this.create_with_annex(strand, TimeZone, JiffTimeZone::fixed(offset), out);
                Ok(())
            })
            .get("name", |this, strand, out| {
                match this.annex().iana_name() {
                    Some(name) => strand.store(out, name),
                    None => strand.store(out, Nil),
                }
                Ok(())
            })
            .method("offset_at", async move |this, strand, args, out| {
                unpack!(strand, args => datetime);
                let global = strand.state::<Tag>();
                let nanos = datetime_to_unix_nanos(strand, global.types.date_time, &datetime)?;
                let ts = timestamp(strand, nanos)?;
                create_offset_duration(strand, global, this.annex().to_offset(ts), out);
                Ok(())
            })
            .method("datetime", async move |this, strand, args, out| {
                unpack!(
                    strand,
                    args => year, month, day, ?:hour, ?:minute, ?:second, ?:nanosecond,
                        ?:disambiguate
                );
                const CONTEXT: &str = "datetime";
                let global = strand.state::<Tag>();
                let year = coerce_component::<i16>(strand, &year, CONTEXT, "year")?;
                let month = coerce_month(strand, global, &month, CONTEXT)?;
                let day = coerce_component::<i8>(strand, &day, CONTEXT, "day")?;
                let mut time = [0i32; 4];
                for (slot, (value, name)) in time.iter_mut().zip([
                    (hour, "hour"),
                    (minute, "minute"),
                    (second, "second"),
                    (nanosecond, "nanosecond"),
                ]) {
                    if let Some(value) = value {
                        *slot = coerce_component::<i32>(strand, &value, CONTEXT, name)?;
                    }
                }
                let [hour, minute, second, nanosecond] = time;
                let civil = i8::try_from(hour)
                    .ok()
                    .zip(i8::try_from(minute).ok())
                    .zip(i8::try_from(second).ok())
                    .and_then(|((hour, minute), second)| {
                        CivilDateTime::new(year, month, day, hour, minute, second, nanosecond).ok()
                    })
                    .ok_or_else(|| Error::value(strand, "invalid date or time"))?;
                let syms = &global.syms;
                let choice = match &disambiguate {
                    None => Some(syms.compatible),
                    Some(value) => value.as_sym(strand),
                };
                let ambiguous = this.annex().to_ambiguous_zoned(civil);
                let zoned = match choice {
                    Some(sym) if sym == syms.compatible => ambiguous.compatible(),
                    Some(sym) if sym == syms.earlier => ambiguous.earlier(),
                    Some(sym) if sym == syms.later => ambiguous.later(),
                    Some(sym) if sym == syms.reject => ambiguous.unambiguous(),
                    _ => {
                        return Err(Error::value(
                            strand,
                            "disambiguate must be :COMPATIBLE:, :EARLIER:, :LATER:, or :REJECT:",
                        ));
                    }
                };
                let zoned = zoned.map_err(|err| Error::value(strand, err.to_string()))?;
                create_datetime(strand, global, zoned.timestamp().as_nanosecond(), out)
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", zone_label(&this.annex()))
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<TimeZone {}>", zone_label(&this.annex()))
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        // Zones with the same rules agree here, as `eq` requires.
        this.annex()
            .to_offset(Timestamp::UNIX_EPOCH)
            .seconds()
            .hash(hasher);
        Ok(())
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        let Some(other) = global.types.time_zone.cast(other) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        Ok(with_instance!(strand, other, |other| same_rules(
            &this.annex(),
            &other.annex()
        )))
    }
}

fn zoned_rfc<'v, 's>(strand: &mut Strand<'v, 's>, zoned: &JiffZoned) -> Result<'v, 's, String> {
    format_rfc3339(strand, zoned.timestamp(), zoned.offset())
}

impl<'v> Object<'v> for Zoned {
    const NAME: &'v str = "Zoned";
    const MODULE: &'v str = "time";
    type Annex = JiffZoned;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("year", |this, strand, out| {
                strand.store(out, i64::from(this.annex().year()));
                Ok(())
            })
            .get("month", |this, strand, out| {
                let global = strand.state::<Tag>();
                strand.store(
                    out,
                    &global.calendar.months[month_index(this.annex().month())],
                );
                Ok(())
            })
            .get("day", |this, strand, out| {
                strand.store(out, i64::from(this.annex().day()));
                Ok(())
            })
            .get("hour", |this, strand, out| {
                strand.store(out, i64::from(this.annex().hour()));
                Ok(())
            })
            .get("minute", |this, strand, out| {
                strand.store(out, i64::from(this.annex().minute()));
                Ok(())
            })
            .get("second", |this, strand, out| {
                strand.store(out, i64::from(this.annex().second()));
                Ok(())
            })
            .get("nanosecond", |this, strand, out| {
                strand.store(out, i64::from(this.annex().subsec_nanosecond()));
                Ok(())
            })
            .get("weekday", |this, strand, out| {
                let global = strand.state::<Tag>();
                strand.store(
                    out,
                    &global.calendar.weekdays[weekday_index(this.annex().weekday())],
                );
                Ok(())
            })
            .get("date", |this, strand, out| {
                let global = strand.state::<Tag>();
                create_date(strand, global, this.annex().date(), out);
                Ok(())
            })
            .get("offset", |this, strand, out| {
                let global = strand.state::<Tag>();
                create_offset_duration(strand, global, this.annex().offset(), out);
                Ok(())
            })
            .get("abbrev", |this, strand, out| {
                let zoned = this.annex();
                let info = zoned.time_zone().to_offset_info(zoned.timestamp());
                strand.store(out, info.abbreviation());
                Ok(())
            })
            .get("zone", |this, strand, out| {
                let global = strand.state::<Tag>();
                global.types.time_zone.create_with_annex(
                    strand,
                    TimeZone,
                    this.annex().time_zone().clone(),
                    out,
                );
                Ok(())
            })
            .get("datetime", |this, strand, out| {
                let global = strand.state::<Tag>();
                create_datetime(
                    strand,
                    global,
                    this.annex().timestamp().as_nanosecond(),
                    out,
                )
            })
            .method("rfc", async move |this, strand, args, out| {
                unpack!(strand, args =>);
                let text = zoned_rfc(strand, &this.annex())?;
                strand.store(out, text.as_str());
                Ok(())
            })
            .method("format", async move |this, strand, args, out| {
                unpack!(strand, args => pattern);
                let pattern = str_arg(strand, &pattern, "format: expected Str pattern")?;
                let text = format_zoned(strand, &this.annex(), &pattern)?;
                strand.store(out, text.as_str());
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let text = zoned_rfc(strand, &this.annex())?;
        fmt!(strand, w, "{}", text)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let text = zoned_rfc(strand, &this.annex())?;
        fmt!(
            strand,
            w,
            "<Zoned {}[{}]>",
            text,
            zone_label(this.annex().time_zone())
        )
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().timestamp().hash(hasher);
        Ok(())
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        let Some(other) = global.types.zoned.cast(other) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let ts: Timestamp = this.annex().timestamp();
        Ok(with_instance!(strand, other, |other| ts == other.annex().timestamp()))
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        let Some(other) = global.types.zoned.cast(other) else {
            return Err(Error::unsupported(strand, "`<`"));
        };
        let ts = this.annex().timestamp();
        Ok(with_instance!(strand, other, |other| ts < other.annex().timestamp()))
    }
}
