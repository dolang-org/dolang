use std::{
    hash::{Hash, Hasher},
    io,
};

use web_time::SystemTime;

use dolang::runtime::value::fmt::Format;

use dolang::runtime::object::fmt;

use dolang::runtime::strand::InterruptMask;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Type, call, error::ResultExt,
    object::TypeBuilder, unpack, value::Root, vm::Builder,
};
use futures::future::{AbortHandle, Abortable};
use time::{
    Date as TimeDate, Duration as TimeDuration, Month as TimeMonth, OffsetDateTime,
    Time as TimeOfDay, Weekday as TimeWeekday, format_description::well_known::Rfc3339,
};

use crate::global::Global;

const NANOS_PER_SEC_I128: i128 = 1_000_000_000;
const NANOS_PER_SEC_F64: f64 = 1_000_000_000.0;
const NANOS_PER_DAY_I128: i128 = 86_400 * NANOS_PER_SEC_I128;

/// Sleeps for at least `duration`.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
async fn sleep(duration: std::time::Duration) {
    tokio::time::sleep(duration).await;
}

/// Sleeps for at least `duration`.
///
/// `setTimeout` takes whole milliseconds and fires almost immediately for
/// delays above `i32::MAX`, so the delay is rounded up and waited out in
/// chunks. Dropping the future clears the pending timeout.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
async fn sleep(duration: std::time::Duration) {
    const MAX_DELAY_MILLIS: u128 = i32::MAX as u128;
    let mut remaining = duration.as_nanos().div_ceil(1_000_000);
    loop {
        let chunk = remaining.min(MAX_DELAY_MILLIS);
        gloo_timers::future::TimeoutFuture::new(chunk as u32).await;
        remaining -= chunk;
        if remaining == 0 {
            break;
        }
    }
}

pub(crate) struct Calendar<'v> {
    pub(crate) date: Type<'v, Date>,
    pub(crate) month: Type<'v, Month>,
    pub(crate) weekday: Type<'v, Weekday>,
    pub(crate) months: [Root<'v>; 12],
    pub(crate) weekdays: [Root<'v>; 7],
}

impl<'v> Calendar<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let date = builder.register_type();
        let month = builder.register_type();
        let weekday = builder.register_type();
        let months = std::array::from_fn(|i| {
            let mut root = Root::new(builder);
            month.create_with_annex(
                builder,
                Month,
                TimeMonth::try_from((i + 1) as u8).unwrap(),
                &mut root,
            );
            root
        });
        let weekdays = std::array::from_fn(|i| {
            let mut root = Root::new(builder);
            weekday.create_with_annex(
                builder,
                Weekday,
                [
                    TimeWeekday::Monday,
                    TimeWeekday::Tuesday,
                    TimeWeekday::Wednesday,
                    TimeWeekday::Thursday,
                    TimeWeekday::Friday,
                    TimeWeekday::Saturday,
                    TimeWeekday::Sunday,
                ][i],
                &mut root,
            );
            root
        });
        Self {
            date,
            month,
            weekday,
            months,
            weekdays,
        }
    }
}

pub(crate) struct Date;

pub(crate) struct Month;

pub(crate) struct Weekday;

pub(crate) struct DateTime;

pub(crate) struct DateTimeAnnex {
    total_nanos: i128,
}

pub(crate) struct Duration;

pub(crate) struct DurationAnnex {
    total_nanos: i128,
}

fn month_index(month: TimeMonth) -> usize {
    usize::from(u8::from(month) - 1)
}

fn weekday_index(weekday: TimeWeekday) -> usize {
    match weekday {
        TimeWeekday::Monday => 0,
        TimeWeekday::Tuesday => 1,
        TimeWeekday::Wednesday => 2,
        TimeWeekday::Thursday => 3,
        TimeWeekday::Friday => 4,
        TimeWeekday::Saturday => 5,
        TimeWeekday::Sunday => 6,
    }
}

fn month_name(month: TimeMonth) -> &'static str {
    match month {
        TimeMonth::January => "JANUARY",
        TimeMonth::February => "FEBRUARY",
        TimeMonth::March => "MARCH",
        TimeMonth::April => "APRIL",
        TimeMonth::May => "MAY",
        TimeMonth::June => "JUNE",
        TimeMonth::July => "JULY",
        TimeMonth::August => "AUGUST",
        TimeMonth::September => "SEPTEMBER",
        TimeMonth::October => "OCTOBER",
        TimeMonth::November => "NOVEMBER",
        TimeMonth::December => "DECEMBER",
    }
}

fn weekday_name(weekday: TimeWeekday) -> &'static str {
    match weekday {
        TimeWeekday::Monday => "MONDAY",
        TimeWeekday::Tuesday => "TUESDAY",
        TimeWeekday::Wednesday => "WEDNESDAY",
        TimeWeekday::Thursday => "THURSDAY",
        TimeWeekday::Friday => "FRIDAY",
        TimeWeekday::Saturday => "SATURDAY",
        TimeWeekday::Sunday => "SUNDAY",
    }
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
    pub(crate) fn from_total_nanos(total_nanos: i128) -> Self {
        Self { total_nanos }
    }

    fn secs(&self) -> f64 {
        self.total_nanos as f64 / NANOS_PER_SEC_F64
    }

    fn write_seconds<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        if self.total_nanos == 0 {
            return fmt!(strand, w, "0s");
        }

        if self.total_nanos < 0 {
            fmt!(strand, w, "-")?;
        }

        let abs_nanos = self.total_nanos.unsigned_abs();
        let secs = abs_nanos / (NANOS_PER_SEC_I128 as u128);
        let nanos = abs_nanos % (NANOS_PER_SEC_I128 as u128);

        if nanos == 0 {
            return fmt!(strand, w, "{}s", secs);
        }

        let mut frac = format!("{:09}", nanos);
        while frac.ends_with('0') {
            frac.pop();
        }

        fmt!(strand, w, "{}.{}s", secs, frac)
    }

    pub(crate) fn to_std_duration<'v, 's>(
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
        format!("{context}: expected Int or Float seconds"),
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

pub(crate) fn coerce_duration<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &dolang::runtime::Value<'v>,
    context: &str,
) -> Result<'v, 's, std::time::Duration> {
    if let Some(duration) = global.types.duration.cast(value) {
        return duration.enter_sync(strand, |strand, duration| {
            duration.annex().to_std_duration(strand)
        });
    }

    if let Some(i) = value.as_int(strand) {
        if i < 0 {
            return Err(Error::runtime(
                strand,
                format!("{context} must be non-negative"),
            ));
        }
        let secs = u64::try_from(i).map_err(|_| Error::overflow(strand))?;
        return Ok(std::time::Duration::from_secs(secs));
    }

    if let Some(f) = value.as_f64(strand) {
        if !f.is_finite() || f < 0.0 {
            return Err(Error::runtime(
                strand,
                format!("{context} must be a non-negative finite number"),
            ));
        }
        return Ok(std::time::Duration::from_secs_f64(f));
    }

    Err(Error::type_error(
        strand,
        format!("{context} must be a Duration, integer, or Float"),
    ))
}

pub(crate) fn datetime_to_unix_nanos<'v, 's>(
    strand: &mut Strand<'v, 's>,
    date_time: dolang::runtime::Type<'v, DateTime>,
    value: &dolang::runtime::Value<'v>,
) -> Result<'v, 's, i128> {
    let datetime = date_time
        .cast(value)
        .ok_or_else(|| Error::type_error(strand, "expected DateTime"))?;
    Ok(datetime.enter_sync(strand, |_strand, datetime| datetime.annex().total_nanos()))
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

fn create_date<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    date: TimeDate,
    out: impl Output<'v>,
) {
    global.types.date.create_with_annex(strand, Date, date, out);
}

fn date_from_rfc<'v, 's>(strand: &mut Strand<'v, 's>, text: &str) -> Result<'v, 's, TimeDate> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(Error::value(strand, "expected RFC full-date (YYYY-MM-DD)"));
    }
    let year = text[..4]
        .parse::<i32>()
        .map_err(|_| Error::value(strand, "expected RFC full-date (YYYY-MM-DD)"))?;
    let month = text[5..7]
        .parse::<u8>()
        .ok()
        .and_then(|month| TimeMonth::try_from(month).ok())
        .ok_or_else(|| Error::value(strand, "expected RFC full-date (YYYY-MM-DD)"))?;
    let day = text[8..10]
        .parse::<u8>()
        .map_err(|_| Error::value(strand, "expected RFC full-date (YYYY-MM-DD)"))?;
    TimeDate::from_calendar_date(year, month, day)
        .map_err(|_| Error::value(strand, "invalid calendar date"))
}

fn format_date_rfc(date: TimeDate) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

impl<'v> Object<'v> for Date {
    const NAME: &'v str = "Date";
    const MODULE: &'v str = "time";
    type Annex = TimeDate;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("year", |this, strand, out| {
                Output::set(strand, out, this.annex().year());
                Ok(())
            })
            .get("month", |this, strand, out| {
                let global = strand.state::<Global<'v>>();
                Output::set(
                    strand,
                    out,
                    &global.calendar.months[month_index(this.annex().month())],
                );
                Ok(())
            })
            .get("day", |this, strand, out| {
                Output::set(strand, out, this.annex().day());
                Ok(())
            })
            .get("weekday", |this, strand, out| {
                let global = strand.state::<Global<'v>>();
                Output::set(
                    strand,
                    out,
                    &global.calendar.weekdays[weekday_index(this.annex().weekday())],
                );
                Ok(())
            })
            .type_method("today", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let now = DateTimeAnnex::from_system_time(SystemTime::now()).into_do(strand)?;
                let date = OffsetDateTime::from_unix_timestamp_nanos(now.total_nanos())
                    .map_err(|_| Error::runtime(strand, "invalid DateTime"))?
                    .date();
                this.create_with_annex(strand, Date, date, out);
                Ok(())
            })
            .type_method("from_ymd", async move |this, strand, args, out| {
                let ([year, month, day], []) = unpack!(strand, args, 3, 0)?;
                let year = year
                    .as_int(strand)
                    .and_then(|year| i32::try_from(year).ok())
                    .ok_or_else(|| Error::type_error(strand, "from_ymd: expected Int year"))?;
                let global = strand.state::<Global<'v>>();
                let month = if let Some(month) = global.types.month.cast(&month) {
                    month.enter_sync(strand, |_strand, month| *month.annex())
                } else {
                    let month = month
                        .as_int(strand)
                        .and_then(|month| u8::try_from(month).ok())
                        .ok_or_else(|| {
                            Error::type_error(strand, "from_ymd: expected Month or Int month")
                        })?;
                    TimeMonth::try_from(month)
                        .map_err(|_| Error::value(strand, "month must be in 1..12"))?
                };
                let day = day
                    .as_int(strand)
                    .and_then(|day| u8::try_from(day).ok())
                    .ok_or_else(|| Error::type_error(strand, "from_ymd: expected Int day"))?;
                let date = TimeDate::from_calendar_date(year, month, day)
                    .map_err(|_| Error::value(strand, "invalid calendar date"))?;
                this.create_with_annex(strand, Date, date, out);
                Ok(())
            })
            .type_method("parse_rfc", async move |this, strand, args, out| {
                let ([text], []) = unpack!(strand, args, 1, 0)?;
                let text = text
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "parse_rfc: expected string"))?
                    .to_string();
                let date = date_from_rfc(strand, &text)?;
                this.create_with_annex(strand, Date, date, out);
                Ok(())
            })
            .method("rfc", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let text = format_date_rfc(*this.annex());
                Output::set(strand, out, text.as_str());
                Ok(())
            })
            .method("datetime", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                create_datetime(
                    strand,
                    global,
                    this.annex()
                        .with_time(TimeOfDay::MIDNIGHT)
                        .assume_utc()
                        .unix_timestamp_nanos(),
                    out,
                )
            })
            .method("add_days", async move |this, strand, args, out| {
                let ([days], []) = unpack!(strand, args, 1, 0)?;
                let days = days
                    .as_int(strand)
                    .and_then(|days| i64::try_from(days).ok())
                    .ok_or_else(|| Error::type_error(strand, "add_days: expected Int days"))?;
                let date = this
                    .annex()
                    .checked_add(TimeDuration::days(days))
                    .ok_or_else(|| Error::overflow(strand))?;
                let global = strand.state::<Global<'v>>();
                create_date(strand, global, date, out);
                Ok(())
            })
            .method("sub_days", async move |this, strand, args, out| {
                let ([days], []) = unpack!(strand, args, 1, 0)?;
                let days = days
                    .as_int(strand)
                    .and_then(|days| i64::try_from(days).ok())
                    .ok_or_else(|| Error::type_error(strand, "sub_days: expected Int days"))?;
                let date = this
                    .annex()
                    .checked_sub(TimeDuration::days(days))
                    .ok_or_else(|| Error::overflow(strand))?;
                let global = strand.state::<Global<'v>>();
                create_date(strand, global, date, out);
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", format_date_rfc(*this.annex()))
    }
    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<Date {}>", format_date_rfc(*this.annex()))
    }
    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().hash(hasher);
        Ok(())
    }
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.date.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| *this.annex() == *other.annex()))
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
        if let Some(other) = global.types.date.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| *this.annex() < *other.annex()))
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
        if let Some(other) = global.types.date.cast(other) {
            other.enter_sync(strand, |strand, other| {
                global.types.duration.create_with_annex(
                    strand,
                    Duration,
                    DurationAnnex::from_total_nanos(
                        i128::from(((*this.annex()) - (*other.annex())).whole_days())
                            * NANOS_PER_DAY_I128,
                    ),
                    out,
                );
                Ok(())
            })
        } else {
            Err(Error::not_supported(strand))
        }
    }
}

macro_rules! calendar_enum {
    ($name:ident, $time:ty, $field:ident, $roots:ident, $index:ident, $name_fn:ident) => {
        impl<'v> Object<'v> for $name {
            const NAME: &'v str = stringify!($name);
            const MODULE: &'v str = "time";
            type Annex = $time;
            type Type = ();
            type TypeAnnex = ();
            fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
                builder
                    .method("next", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let global = strand.state::<Global<'v>>();
                        Output::set(
                            strand,
                            out,
                            &global.calendar.$roots
                                [($index(*this.annex()) + 1) % global.calendar.$roots.len()],
                        );
                        Ok(())
                    })
                    .method("previous", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let global = strand.state::<Global<'v>>();
                        let len = global.calendar.$roots.len();
                        Output::set(
                            strand,
                            out,
                            &global.calendar.$roots[($index(*this.annex()) + len - 1) % len],
                        );
                        Ok(())
                    })
            }
            fn type_get<'a, 's>(
                _this: Type<'v, Self>,
                strand: &'a mut Strand<'v, 's>,
                field: dolang::runtime::Sym<'v, 'a>,
                out: Slot<'v, 'a>,
            ) -> Result<'v, 's, ()> {
                let field_name = field.as_str(strand).to_owned();
                let global = strand.state::<Global<'v>>();
                let index = global.calendar.$roots.iter().position(|value| {
                    let value = global.types.$field.cast(value).unwrap();
                    value.enter_sync(strand, |_strand, value| {
                        $name_fn(*value.annex()) == field_name
                    })
                });
                if let Some(index) = index {
                    Output::set(strand, out, &global.calendar.$roots[index]);
                    Ok(())
                } else {
                    Err(Error::field(strand, field))
                }
            }
            fn display<'a, 's>(
                this: Instance<'v, 'a, Self>,
                strand: &'a mut Strand<'v, 's>,
                w: &mut dyn Format<'v>,
            ) -> Result<'v, 's, ()> {
                fmt!(strand, w, "{}", $name_fn(*this.annex()))
            }
            fn debug<'a, 's>(
                this: Instance<'v, 'a, Self>,
                strand: &'a mut Strand<'v, 's>,
                w: &mut dyn Format<'v>,
            ) -> Result<'v, 's, ()> {
                fmt!(
                    strand,
                    w,
                    "<{} {}>",
                    stringify!($name),
                    $name_fn(*this.annex())
                )
            }
            fn hash<'a, 's>(
                this: Instance<'v, 'a, Self>,
                _strand: &'a mut Strand<'v, 's>,
                hasher: &mut impl Hasher,
            ) -> Result<'v, 's, ()> {
                this.annex().hash(hasher);
                Ok(())
            }
            fn eq<'a, 's>(
                this: Instance<'v, 'a, Self>,
                strand: &'a mut Strand<'v, 's>,
                other: &dolang::runtime::Value<'v>,
            ) -> Result<'v, 's, bool> {
                let global = strand.state::<Global<'v>>();
                if let Some(other) = global.types.$field.cast(other) {
                    Ok(other.enter_sync(strand, |_strand, other| *this.annex() == *other.annex()))
                } else {
                    Err(Error::not_supported(strand))
                }
            }
        }
    };
}

calendar_enum!(
    Weekday,
    TimeWeekday,
    weekday,
    weekdays,
    weekday_index,
    weekday_name
);

impl<'v> Object<'v> for Month {
    const NAME: &'v str = "Month";
    const MODULE: &'v str = "time";
    type Annex = TimeMonth;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("next", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                Output::set(
                    strand,
                    out,
                    &global.calendar.months[(month_index(*this.annex()) + 1) % 12],
                );
                Ok(())
            })
            .method("previous", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                Output::set(
                    strand,
                    out,
                    &global.calendar.months[(month_index(*this.annex()) + 11) % 12],
                );
                Ok(())
            })
    }
    fn type_get<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: dolang::runtime::Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let field_name = field.as_str(strand).to_owned();
        let global = strand.state::<Global<'v>>();
        let index = global.calendar.months.iter().position(|value| {
            let value = global.types.month.cast(value).unwrap();
            value.enter_sync(strand, |_strand, value| {
                month_name(*value.annex()) == field_name
            })
        });
        if let Some(index) = index {
            Output::set(strand, out, &global.calendar.months[index]);
            Ok(())
        } else {
            Err(Error::field(strand, field))
        }
    }
    fn type_index<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &dolang::runtime::Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let index = index
            .as_int(strand)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| (1..=12).contains(index))
            .ok_or_else(|| Error::value(strand, "month index must be in 1..12"))?;
        let global = strand.state::<Global<'v>>();
        Output::set(strand, out, &global.calendar.months[index - 1]);
        Ok(())
    }
    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", month_name(*this.annex()))
    }
    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<Month {}>", month_name(*this.annex()))
    }
    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().hash(hasher);
        Ok(())
    }
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.month.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| *this.annex() == *other.annex()))
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
        if let Some(other) = global.types.month.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| *this.annex() < *other.annex()))
        } else {
            Err(Error::not_supported(strand))
        }
    }
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
            .method("date", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let datetime =
                    OffsetDateTime::from_unix_timestamp_nanos(this.annex().total_nanos())
                        .map_err(|_| Error::runtime(strand, "invalid DateTime"))?;
                let global = strand.state::<Global<'v>>();
                create_date(strand, global, datetime.date(), out);
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
                        .ok_or_else(|| Error::type_error(strand, "from_unix: expected Int nanos"))
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
            .type_method("parse_rfc", async move |this, strand, args, out| {
                let ([text], []) = unpack!(strand, args, 1, 0)?;
                let text = text
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "parse_rfc: expected string"))?
                    .to_string();
                let datetime = OffsetDateTime::parse(&text, &Rfc3339)
                    .map_err(|err| Error::runtime(strand, err))?;
                let annex = DateTimeAnnex::from_total_nanos(datetime.unix_timestamp_nanos());
                this.create_with_annex(strand, DateTime, annex, out);
                Ok(())
            })
            .method("rfc", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let formatted = format_datetime_rfc3339(strand, &this.annex())?;
                Output::set(strand, out, formatted.as_str());
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let formatted = format_datetime_rfc3339(strand, &this.annex())?;
        fmt!(strand, w, "{}", formatted)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<DateTime ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().total_nanos.hash(hasher);
        Ok(())
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.date_time.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().total_nanos == other.annex().total_nanos
            }))
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
        if let Some(other) = global.types.date_time.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().total_nanos < other.annex().total_nanos
            }))
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
        if let Some(other) = global.types.date_time.cast(other) {
            other.enter_sync(strand, |strand, other| {
                let left = this.annex().total_nanos;
                let right = other.annex().total_nanos;
                global.types.duration.create_with_annex(
                    strand,
                    Duration,
                    DurationAnnex::from_total_nanos(left - right),
                    out,
                );
                Ok(())
            })
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        this.annex().write_seconds(strand, w)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<Duration ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().total_nanos.hash(hasher);
        Ok(())
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.duration.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().total_nanos == other.annex().total_nanos
            }))
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
        if let Some(other) = global.types.duration.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().total_nanos < other.annex().total_nanos
            }))
        } else {
            Err(Error::not_supported(strand))
        }
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("time")
        .function("sleep", async move |strand, args, _out| {
            let ([duration], []) = unpack!(strand, args, 1, 0)?;
            let duration = coerce_duration(strand, global, &duration, "sleep duration")?;
            sleep(duration).await;
            Ok(())
        })
        .function("timeout", async move |strand, args, out| {
            let ([duration, block], []) = unpack!(strand, args, 2, 0)?;
            let duration = coerce_duration(strand, global, &duration, "timeout duration")?;
            let mask = strand.interrupt_mask();
            let timeout_mask = InterruptMask::TIMED_OUT;
            let interrupt = strand.interrupt_token().nested(timeout_mask);
            let (abort, reg) = AbortHandle::new_pair();
            let interrupt_clone = interrupt.clone();
            strand.spawn_task(async move {
                let _ = Abortable::new(
                    async move {
                        sleep(duration).await;
                        interrupt_clone.timeout();
                    },
                    reg,
                )
                .await;
            });
            let res = strand
                .with_interrupt_mask(mask - timeout_mask, async move |strand| {
                    strand
                        .with_interrupt_token(interrupt, async move |strand| {
                            call!(strand, block, out).await
                        })
                        .await
                })
                .await;
            abort.abort();
            res
        })
        .value("DateTime", global.types.date_time)
        .value("Duration", global.types.duration)
        .value("Date", global.types.date)
        .value("Month", global.types.month)
        .value("Weekday", global.types.weekday)
        .commit();
}
