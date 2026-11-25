use crate::global::Tag;

use std::{
    hash::{Hash, Hasher},
    io,
    time::SystemTime,
};

use dolang::runtime::value::fmt::Format;

use dolang::runtime::object::fmt;

use dolang::runtime::strand::InterruptMask;
use dolang::runtime::{
    AllocExt, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Type, Value, call,
    io::IoResultExt as _, object::TypeBuilder, unpack, value::Root, vm::Register, with_instance,
    with_interrupt_mask, with_interrupt_token,
};
use futures::future::{AbortHandle, Abortable};
use jiff::{
    Span, Timestamp,
    civil::{Date as CivilDate, DateTime as CivilDateTime, Weekday as CivilWeekday},
    fmt::strtime,
    tz::{Offset, TimeZone as JiffTimeZone},
};

use crate::{global::Global, zone};

const NANOS_PER_SEC_I128: i128 = 1_000_000_000;
const NANOS_PER_SEC_F64: f64 = 1_000_000_000.0;

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
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        let date = builder.register_type();
        let month = builder.register_type();
        let weekday = builder.register_type();
        let months = std::array::from_fn(|i| {
            let mut root = Root::new(builder);
            month.create_with_annex(builder, Month, MonthNum(i as i8 + 1), &mut root);
            root
        });
        let weekdays = std::array::from_fn(|i| {
            let mut root = Root::new(builder);
            weekday.create_with_annex(
                builder,
                Weekday,
                CivilWeekday::from_monday_zero_offset(i as i8).unwrap(),
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

/// A month number, 1 through 12.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct MonthNum(i8);

const MONTH_NAMES: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];

const WEEKDAY_NAMES: [&str; 7] = [
    "MONDAY",
    "TUESDAY",
    "WEDNESDAY",
    "THURSDAY",
    "FRIDAY",
    "SATURDAY",
    "SUNDAY",
];

/// Returns a month's index into [`Calendar::months`], from its number.
pub(crate) fn month_index(month: i8) -> usize {
    (month - 1) as usize
}

fn month_num_index(month: MonthNum) -> usize {
    month_index(month.0)
}

pub(crate) fn weekday_index(weekday: CivilWeekday) -> usize {
    weekday.to_monday_zero_offset() as usize
}

fn month_name(month: MonthNum) -> &'static str {
    MONTH_NAMES[month_num_index(month)]
}

fn weekday_name(weekday: CivilWeekday) -> &'static str {
    WEEKDAY_NAMES[weekday_index(weekday)]
}

/// Converts Unix nanoseconds to a jiff timestamp, or fails with an overflow
/// error.
pub(crate) fn timestamp<'v, 's>(
    strand: &mut Strand<'v, 's>,
    total_nanos: i128,
) -> Result<'v, 's, Timestamp> {
    Timestamp::from_nanosecond(total_nanos).map_err(|_| Error::overflow(strand))
}

impl DateTimeAnnex {
    pub(crate) fn from_total_nanos(total_nanos: i128) -> Self {
        Self { total_nanos }
    }

    pub(crate) fn total_nanos(&self) -> i128 {
        self.total_nanos
    }

    pub(crate) fn from_system_time(time: SystemTime) -> io::Result<Self> {
        Self::from_epoch_offset(
            time.duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|err| err.duration()),
        )
    }

    /// Reads the current time. std's clock is unavailable in the browser, so
    /// this reads the host clock through `web_time`.
    pub(crate) fn now() -> io::Result<Self> {
        Self::from_epoch_offset(
            web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
                .map_err(|err| err.duration()),
        )
    }

    /// Converts an offset from the Unix epoch: `Ok` after it, `Err` before it.
    fn from_epoch_offset(
        offset: std::result::Result<std::time::Duration, std::time::Duration>,
    ) -> io::Result<Self> {
        let (duration, before_epoch) = match offset {
            Ok(duration) => (duration, false),
            Err(duration) => (duration, true),
        };
        let total_nanos = i128::from(duration.as_secs())
            .checked_mul(NANOS_PER_SEC_I128)
            .and_then(|secs| secs.checked_add(i128::from(duration.subsec_nanos())))
            .and_then(|total_nanos| {
                if before_epoch {
                    total_nanos.checked_neg()
                } else {
                    Some(total_nanos)
                }
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timestamp overflow"))?;
        Ok(Self { total_nanos })
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

    pub(crate) fn total_nanos(&self) -> i128 {
        self.total_nanos
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
            return Err(Error::value(strand, "sleep duration must be non-negative"));
        }
        let total_nanos = self.total_nanos as u128;
        let secs = total_nanos / (NANOS_PER_SEC_I128 as u128);
        let nanos = total_nanos % (NANOS_PER_SEC_I128 as u128);
        let secs = u64::try_from(secs).map_err(|_| Error::overflow(strand))?;
        Ok(std::time::Duration::new(secs, nanos as u32))
    }
}

pub(crate) fn float_seconds_to_nanos<'v, 's>(
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
    value: &Value<'v>,
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

/// Formats an instant as RFC 3339, at `offset`; UTC is written `Z`.
///
/// RFC 3339 has only four-digit years, so years before 0000 are a value
/// error.
pub(crate) fn format_rfc3339<'v, 's>(
    strand: &mut Strand<'v, 's>,
    ts: Timestamp,
    offset: Offset,
) -> Result<'v, 's, String> {
    if offset.to_datetime(ts).year() < 0 {
        return Err(Error::value(
            strand,
            "RFC 3339 cannot represent years before 0000",
        ));
    }
    Ok(if offset == Offset::UTC {
        ts.to_string()
    } else {
        ts.display_with_offset(offset).to_string()
    })
}

fn format_datetime_rfc3339<'v, 's>(
    strand: &mut Strand<'v, 's>,
    datetime: &DateTimeAnnex,
) -> Result<'v, 's, String> {
    let ts = timestamp(strand, datetime.total_nanos())?;
    format_rfc3339(strand, ts, Offset::UTC)
}

/// Parses a fixed-width run of ASCII digits.
fn digits(bytes: &[u8]) -> Option<i32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0, |acc, digit| acc * 10 + i32::from(digit - b'0')),
    )
}

/// Parses `YYYY-MM-DD` into its components, without validating the date.
fn full_date_parts(bytes: &[u8]) -> Option<(i16, i8, i8)> {
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    Some((
        digits(&bytes[..4])? as i16,
        digits(&bytes[5..7])? as i8,
        digits(&bytes[8..10])? as i8,
    ))
}

/// Parses a strict RFC 3339 date-time: `full-date "T" time [frac] offset`,
/// with the offset `Z` or `±hh:mm`. `T` and `Z` may be lowercase.
fn parse_rfc3339(text: &str) -> Option<Timestamp> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || !matches!(bytes[10], b'T' | b't') {
        return None;
    }
    let (year, month, day) = full_date_parts(&bytes[..10])?;
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let hour = digits(&bytes[11..13])? as i8;
    let minute = digits(&bytes[14..16])? as i8;
    let second = digits(&bytes[17..19])? as i8;
    let mut rest = &bytes[19..];
    let mut nanos = 0;
    if let [b'.', frac @ ..] = rest {
        let len = frac.iter().take_while(|b| b.is_ascii_digit()).count();
        if len == 0 {
            return None;
        }
        // Digits past nanoseconds are truncated.
        let kept = len.min(9);
        nanos = digits(&frac[..kept])? * 10i32.pow((9 - kept) as u32);
        rest = &frac[len..];
    }
    let offset = match rest {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h @ .., b':', m1, m2] if h.len() == 2 => {
            let hours = digits(h)?;
            let minutes = digits(&[*m1, *m2])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let seconds = hours * 3600 + minutes * 60;
            if *sign == b'-' { -seconds } else { seconds }
        }
        _ => return None,
    };
    let datetime = CivilDateTime::new(year, month, day, hour, minute, second, nanos).ok()?;
    Offset::from_seconds(offset)
        .ok()?
        .to_timestamp(datetime)
        .ok()
}

pub(crate) fn coerce_duration<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: Option<State<'v, Global<'v>>>,
    value: &Value<'v>,
    context: &str,
) -> Result<'v, 's, std::time::Duration> {
    if let Some(duration) = global.and_then(|global| global.types.duration.cast(value)) {
        return with_instance!(strand, duration, |duration| {
            duration.annex().to_std_duration(strand)
        });
    }

    if let Some(i) = value.as_int(strand) {
        if i < 0 {
            return Err(Error::value(
                strand,
                format!("{context} must be non-negative"),
            ));
        }
        let secs = u64::try_from(i).map_err(|_| Error::overflow(strand))?;
        return Ok(std::time::Duration::from_secs(secs));
    }

    if let Some(f) = value.as_f64(strand) {
        if !f.is_finite() || f < 0.0 {
            return Err(Error::value(
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
    date_time: Type<'v, DateTime>,
    value: &Value<'v>,
) -> Result<'v, 's, i128> {
    let datetime = date_time
        .cast(value)
        .ok_or_else(|| Error::type_error(strand, "expected DateTime"))?;
    Ok(with_instance!(strand, datetime, |datetime| datetime
        .annex()
        .total_nanos()))
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

pub(crate) fn create_date<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    date: CivilDate,
    out: impl Output<'v>,
) {
    global.types.date.create_with_annex(strand, Date, date, out);
}

fn date_from_rfc<'v, 's>(strand: &mut Strand<'v, 's>, text: &str) -> Result<'v, 's, CivilDate> {
    let Some((year, month, day)) =
        full_date_parts(text.as_bytes()).filter(|(_, m, _)| (1..=12).contains(m))
    else {
        return Err(Error::data(strand, "expected RFC full-date (YYYY-MM-DD)"));
    };
    CivilDate::new(year, month, day).map_err(|_| Error::data(strand, "invalid calendar date"))
}

fn format_date_rfc(date: CivilDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// Coerces a month argument: a `Month`, or its number.
pub(crate) fn coerce_month<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    month: &Value<'v>,
    context: &str,
) -> Result<'v, 's, i8> {
    if let Some(month) = global.types.month.cast(month) {
        return Ok(with_instance!(strand, month, |month| month.annex().0));
    }
    let month = month.as_int(strand).ok_or_else(|| {
        Error::type_error(strand, format!("{context}: expected Month or Int month"))
    })?;
    i8::try_from(month)
        .ok()
        .filter(|month| (1..=12).contains(month))
        .ok_or_else(|| Error::value(strand, "month must be in 1..12"))
}

/// Coerces an integer calendar component. Values that don't fit `T` are
/// value errors, as out-of-range components are.
pub(crate) fn coerce_component<'v, 's, T: TryFrom<i128>>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    context: &str,
    name: &str,
) -> Result<'v, 's, T> {
    let value = value
        .as_int(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{context}: expected Int {name}")))?;
    T::try_from(value).map_err(|_| Error::value(strand, format!("{context}: {name} out of range")))
}

/// Reads a string argument, or fails with a type error carrying `message`.
pub(crate) fn str_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    message: &str,
) -> Result<'v, 's, String> {
    match value.as_str(strand) {
        Some(text) => Ok(text.to_string()),
        None => Err(Error::type_error(strand, message.to_owned())),
    }
}

/// Parses `text` with a strftime-style pattern. Input without an offset is
/// read as wall time in `tz`, taking the earlier instant of a repeated time and
/// moving past a skipped one.
fn parse_with_pattern(
    pattern: &str,
    text: &str,
    tz: &JiffTimeZone,
) -> std::result::Result<Timestamp, jiff::Error> {
    let parsed = strtime::parse(pattern, text)?;
    if parsed.offset().is_some() {
        return parsed.to_timestamp();
    }
    tz.to_ambiguous_zoned(parsed.to_datetime()?)
        .compatible()
        .map(|zoned| zoned.timestamp())
}

/// Adds signed days to a date; `None` is a count that already overflowed.
fn add_days<'v, 's>(
    strand: &mut Strand<'v, 's>,
    date: CivilDate,
    days: Option<i64>,
) -> Result<'v, 's, CivilDate> {
    days.and_then(|days| Span::new().try_days(days).ok())
        .and_then(|span| date.checked_add(span).ok())
        .ok_or_else(|| Error::overflow(strand))
}

impl<'v> Object<'v> for Date {
    const NAME: &'v str = "Date";
    const MODULE: &'v str = "time";
    type Annex = CivilDate;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let zone = builder.sym("zone");
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
            .get("weekday", |this, strand, out| {
                let global = strand.state::<Tag>();
                strand.store(
                    out,
                    &global.calendar.weekdays[weekday_index(this.annex().weekday())],
                );
                Ok(())
            })
            .type_method("today", async move |this, strand, args, out| {
                unpack!(strand, args => ?:zone);
                let global = strand.state::<Tag>();
                let tz = zone::zone_arg(strand, global, zone.as_deref())?;
                let now = DateTimeAnnex::now().into_do_io(strand)?;
                let date = timestamp(strand, now.total_nanos())?.to_zoned(tz).date();
                this.create_with_annex(strand, Date, date, out);
                Ok(())
            })
            .type_method("from_ymd", async move |this, strand, args, out| {
                unpack!(strand, args => year, month, day);
                let year = coerce_component::<i16>(strand, &year, "from_ymd", "year")?;
                let global = strand.state::<Tag>();
                let month = coerce_month(strand, global, &month, "from_ymd")?;
                let day = coerce_component::<i8>(strand, &day, "from_ymd", "day")?;
                let date = CivilDate::new(year, month, day)
                    .map_err(|_| Error::value(strand, "invalid calendar date"))?;
                this.create_with_annex(strand, Date, date, out);
                Ok(())
            })
            .type_method("parse_rfc", async move |this, strand, args, out| {
                unpack!(strand, args => text);
                let text = text
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "parse_rfc: expected string"))?
                    .to_string();
                let date = date_from_rfc(strand, &text)?;
                this.create_with_annex(strand, Date, date, out);
                Ok(())
            })
            .method("rfc", async move |this, strand, args, out| {
                unpack!(strand, args =>);
                let text = format_date_rfc(*this.annex());
                strand.store(out, text.as_str());
                Ok(())
            })
            .method("datetime", async move |this, strand, args, out| {
                unpack!(strand, args => ?:zone);
                let global = strand.state::<Tag>();
                let tz = zone::zone_arg(strand, global, zone.as_deref())?;
                let midnight = this
                    .annex()
                    .to_zoned(tz)
                    .map_err(|_| Error::overflow(strand))?;
                create_datetime(strand, global, midnight.timestamp().as_nanosecond(), out)
            })
            .method("add_days", async move |this, strand, args, out| {
                unpack!(strand, args => days);
                let days = days
                    .as_int(strand)
                    .and_then(|days| i64::try_from(days).ok())
                    .ok_or_else(|| Error::type_error(strand, "add_days: expected Int days"))?;
                let date = add_days(strand, *this.annex(), Some(days))?;
                let global = strand.state::<Tag>();
                create_date(strand, global, date, out);
                Ok(())
            })
            .method("sub_days", async move |this, strand, args, out| {
                unpack!(strand, args => days);
                let days = days
                    .as_int(strand)
                    .and_then(|days| i64::try_from(days).ok())
                    .ok_or_else(|| Error::type_error(strand, "sub_days: expected Int days"))?;
                let date = add_days(strand, *this.annex(), days.checked_neg())?;
                let global = strand.state::<Tag>();
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
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.date.cast(other) {
            Ok(with_instance!(strand, other, |other| *this.annex() == *other.annex()))
        } else {
            Err(Error::unsupported(strand, "`==`"))
        }
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.date.cast(other) {
            Ok(with_instance!(strand, other, |other| *this.annex() < *other.annex()))
        } else {
            Err(Error::unsupported(strand, "`<`"))
        }
    }
    fn sub<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.date.cast(other) {
            with_instance!(strand, other, |other| {
                global.types.duration.create_with_annex(
                    strand,
                    Duration,
                    DurationAnnex::from_total_nanos(
                        this.annex().duration_since(*other.annex()).as_nanos(),
                    ),
                    out,
                );
                Ok(())
            })
        } else {
            Err(Error::unsupported(strand, "`-`"))
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
                        unpack!(strand, args =>);
                        let global = strand.state::<Tag>();
                        strand.store(
                            out,
                            &global.calendar.$roots
                                [($index(*this.annex()) + 1) % global.calendar.$roots.len()],
                        );
                        Ok(())
                    })
                    .method("previous", async move |this, strand, args, out| {
                        unpack!(strand, args =>);
                        let global = strand.state::<Tag>();
                        let len = global.calendar.$roots.len();
                        strand.store(
                            out,
                            &global.calendar.$roots[($index(*this.annex()) + len - 1) % len],
                        );
                        Ok(())
                    })
            }
            fn type_get<'a, 's>(
                _this: Type<'v, Self>,
                strand: &'a mut Strand<'v, 's>,
                field: Sym<'v, 'a>,
                out: Slot<'v, 'a>,
            ) -> Result<'v, 's, ()> {
                let field_name = field.as_str(strand).to_owned();
                let global = strand.state::<Tag>();
                let index = global.calendar.$roots.iter().position(|value| {
                    let value = global.types.$field.cast(value).unwrap();
                    with_instance!(strand, value, |value| {
                        $name_fn(*value.annex()) == field_name
                    })
                });
                if let Some(index) = index {
                    strand.store(out, &global.calendar.$roots[index]);
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
                other: &Value<'v>,
            ) -> Result<'v, 's, bool> {
                let global = strand.state::<Tag>();
                if let Some(other) = global.types.$field.cast(other) {
                    Ok(with_instance!(strand, other, |other| *this.annex() == *other.annex()))
                } else {
                    Err(Error::unsupported(strand, "`==`"))
                }
            }
        }
    };
}

calendar_enum!(
    Weekday,
    CivilWeekday,
    weekday,
    weekdays,
    weekday_index,
    weekday_name
);

impl<'v> Object<'v> for Month {
    const NAME: &'v str = "Month";
    const MODULE: &'v str = "time";
    type Annex = MonthNum;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("next", async move |this, strand, args, out| {
                unpack!(strand, args =>);
                let global = strand.state::<Tag>();
                strand.store(
                    out,
                    &global.calendar.months[(month_num_index(*this.annex()) + 1) % 12],
                );
                Ok(())
            })
            .method("previous", async move |this, strand, args, out| {
                unpack!(strand, args =>);
                let global = strand.state::<Tag>();
                strand.store(
                    out,
                    &global.calendar.months[(month_num_index(*this.annex()) + 11) % 12],
                );
                Ok(())
            })
    }
    fn type_get<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let field_name = field.as_str(strand).to_owned();
        let global = strand.state::<Tag>();
        let index = global.calendar.months.iter().position(|value| {
            let value = global.types.month.cast(value).unwrap();
            with_instance!(strand, value, |value| {
                month_name(*value.annex()) == field_name
            })
        });
        if let Some(index) = index {
            strand.store(out, &global.calendar.months[index]);
            Ok(())
        } else {
            Err(Error::field(strand, field))
        }
    }
    fn type_index<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let index = index
            .as_int(strand)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| (1..=12).contains(index))
            .ok_or_else(|| Error::value(strand, "month index must be in 1..12"))?;
        let global = strand.state::<Tag>();
        strand.store(out, &global.calendar.months[index - 1]);
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
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.month.cast(other) {
            Ok(with_instance!(strand, other, |other| *this.annex() == *other.annex()))
        } else {
            Err(Error::unsupported(strand, "`==`"))
        }
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.month.cast(other) {
            Ok(with_instance!(strand, other, |other| *this.annex() < *other.annex()))
        } else {
            Err(Error::unsupported(strand, "`<`"))
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
        let nanos = builder.sym("nanos");
        let zone = builder.sym("zone");
        builder
            .get("unix_secs", |this, strand, out| {
                strand.store(out, this.annex().total_nanos() as f64 / NANOS_PER_SEC_F64);
                Ok(())
            })
            .get("unix_nanos", |this, strand, out| {
                strand.store(out, this.annex().total_nanos());
                Ok(())
            })
            .get("date", |this, strand, out| {
                let ts = timestamp(strand, this.annex().total_nanos())?;
                let global = strand.state::<Tag>();
                create_date(strand, global, Offset::UTC.to_datetime(ts).date(), out);
                Ok(())
            })
            .method("zoned", async move |this, strand, args, out| {
                unpack!(strand, args => tz);
                let global = strand.state::<Tag>();
                let tz = zone::zone_arg(strand, global, Some(&tz))?;
                let ts = timestamp(strand, this.annex().total_nanos())?;
                zone::create_zoned(strand, global, ts.to_zoned(tz), out);
                Ok(())
            })
            .method("format", async move |this, strand, args, out| {
                unpack!(strand, args => pattern, ?:zone);
                let pattern = str_arg(strand, &pattern, "format: expected Str pattern")?;
                let global = strand.state::<Tag>();
                let tz = zone::zone_arg(strand, global, zone.as_deref())?;
                let ts = timestamp(strand, this.annex().total_nanos())?;
                let text = zone::format_zoned(strand, &ts.to_zoned(tz), &pattern)?;
                strand.store(out, text.as_str());
                Ok(())
            })
            .type_method("now", async move |this, strand, args, out| {
                unpack!(strand, args =>);
                let annex = DateTimeAnnex::now().into_do_io(strand)?;
                this.create_with_annex(strand, DateTime, annex, out);
                Ok(())
            })
            .type_method("from_unix", async move |this, strand, args, out| {
                unpack!(strand, args => ?secs, ?:nanos);
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
                unpack!(strand, args => text);
                let text = text
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "parse_rfc: expected string"))?
                    .to_string();
                let ts = parse_rfc3339(&text)
                    .ok_or_else(|| Error::data(strand, "expected RFC 3339 date-time"))?;
                let annex = DateTimeAnnex::from_total_nanos(ts.as_nanosecond());
                this.create_with_annex(strand, DateTime, annex, out);
                Ok(())
            })
            .type_method("parse", async move |this, strand, args, out| {
                unpack!(strand, args => pattern, text, ?:zone);
                let pattern = str_arg(strand, &pattern, "parse: expected Str pattern")?;
                let text = str_arg(strand, &text, "parse: expected Str text")?;
                let global = strand.state::<Tag>();
                let tz = zone::zone_arg(strand, global, zone.as_deref())?;
                let ts = parse_with_pattern(&pattern, &text, &tz)
                    .map_err(|err| Error::data(strand, err.to_string()))?;
                let annex = DateTimeAnnex::from_total_nanos(ts.as_nanosecond());
                this.create_with_annex(strand, DateTime, annex, out);
                Ok(())
            })
            .method("rfc", async move |this, strand, args, out| {
                unpack!(strand, args =>);
                let formatted = format_datetime_rfc3339(strand, &this.annex())?;
                strand.store(out, formatted.as_str());
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
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.date_time.cast(other) {
            Ok(with_instance!(strand, other, |other| {
                this.annex().total_nanos == other.annex().total_nanos
            }))
        } else {
            Err(Error::unsupported(strand, "`==`"))
        }
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.date_time.cast(other) {
            Ok(with_instance!(strand, other, |other| {
                this.annex().total_nanos < other.annex().total_nanos
            }))
        } else {
            Err(Error::unsupported(strand, "`<`"))
        }
    }

    fn sub<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.date_time.cast(other) {
            with_instance!(strand, other, |other| {
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
            Err(Error::unsupported(strand, "`-`"))
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
                strand.store(out, this.annex().secs());
                Ok(())
            })
            .get("nanos", |this, strand, out| {
                strand.store(out, this.annex().total_nanos);
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
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.duration.cast(other) {
            Ok(with_instance!(strand, other, |other| {
                this.annex().total_nanos == other.annex().total_nanos
            }))
        } else {
            Err(Error::unsupported(strand, "`==`"))
        }
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Tag>();
        if let Some(other) = global.types.duration.cast(other) {
            Ok(with_instance!(strand, other, |other| {
                this.annex().total_nanos < other.annex().total_nanos
            }))
        } else {
            Err(Error::unsupported(strand, "`<`"))
        }
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("time")
        .function("sleep", async move |strand, args, _out| {
            unpack!(strand, args => duration);
            let duration = coerce_duration(strand, Some(global), &duration, "sleep duration")?;
            sleep(duration).await;
            Ok(())
        })
        .function("timeout", async move |strand, args, out| {
            unpack!(strand, args => duration, block);
            let duration = coerce_duration(strand, Some(global), &duration, "timeout duration")?;
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
            let res = with_interrupt_mask!(strand, mask - timeout_mask, {
                with_interrupt_token!(strand, interrupt, call!(strand, block, out))
            });
            abort.abort();
            res
        })
        .value("DateTime", global.types.date_time)
        .value("Duration", global.types.duration)
        .value("Date", global.types.date)
        .value("Month", global.types.month)
        .value("Weekday", global.types.weekday)
        .value("TimeZone", global.types.time_zone)
        .value("Zoned", global.types.zoned)
        .commit();
}
