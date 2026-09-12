#![deny(warnings)]

mod extension;
mod global;
mod time;

use std::{io, time::SystemTime};

use dolang::runtime::{Output, Result, Strand, Value};

pub use extension::TimeExt;

use crate::global::Global;

/// Extracts a Do `time.DateTime` runtime value.
pub fn as_datetime<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Option<SystemTime> {
    let global = strand.state::<Global<'v>>();
    let datetime = global.types.date_time.cast(value)?;
    datetime.enter_sync(strand, |_strand, inst| inst.annex().to_system_time().ok())
}

/// Constructs a Do `time.DateTime` from a Rust system time.
pub fn datetime<'v>(
    strand: &mut Strand<'v, '_>,
    time: SystemTime,
    out: impl Output<'v>,
) -> io::Result<()> {
    let global = strand.state::<Global<'v>>();
    let annex = time::DateTimeAnnex::from_system_time(time)?;
    global
        .types
        .date_time
        .create_with_annex(strand, time::DateTime, annex, out);
    Ok(())
}

/// Constructs a Do `time.DateTime` from Unix nanoseconds.
pub fn create_datetime<'v, 's>(
    strand: &mut Strand<'v, 's>,
    total_nanos: i128,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    time::create_datetime(strand, global, total_nanos, out)
}

/// Extracts the Unix nanoseconds of a Do `time.DateTime`, or fails with a
/// type error.
pub fn datetime_to_unix_nanos<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, i128> {
    let global = strand.state::<Global<'v>>();
    time::datetime_to_unix_nanos(strand, global.types.date_time, value)
}

/// Constructs a Do `time.Duration` from a Rust duration.
pub fn duration<'v, 's>(
    strand: &mut Strand<'v, 's>,
    duration: std::time::Duration,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let total_nanos =
        i128::from(duration.as_secs()) * 1_000_000_000 + i128::from(duration.subsec_nanos());
    global.types.duration.create_with_annex(
        strand,
        time::Duration,
        time::DurationAnnex::from_total_nanos(total_nanos),
        out,
    );
    Ok(())
}

/// Extracts a non-negative Do `time.Duration` runtime value.
pub fn as_duration<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Option<std::time::Duration> {
    let global = strand.state::<Global<'v>>();
    let duration = global.types.duration.cast(value)?;
    duration.enter_sync(strand, |strand, duration| {
        duration.annex().to_std_duration(strand).ok()
    })
}

/// Returns whether a value is a Do `time.Duration`.
pub fn is_duration<'v>(strand: &Strand<'v, '_>, value: &Value<'v>) -> bool {
    let global = strand.state::<Global<'v>>();
    global.types.duration.cast(value).is_some()
}

/// Coerces a duration argument: a `time.Duration`, or a non-negative number of
/// seconds. `context` names the argument in error messages.
pub fn coerce_duration<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    context: &str,
) -> Result<'v, 's, std::time::Duration> {
    let global = strand.state::<Global<'v>>();
    time::coerce_duration(strand, global, value, context)
}
