//! Formatting specifications and destinations.

use std::fmt;

use unicode_segmentation::UnicodeSegmentation;

use crate::{
    error::{Error, Result},
    strand::Strand,
    value::Value,
};

use super::prim::Prim;

/// Destination for formatting Do values.
///
/// This mirrors [`fmt::Write`], but supplies the active [`Strand`] to each
/// operation and returns native Do errors so destinations may allocate
/// GC-managed storage while data is appended.
pub trait Format<'v> {
    /// Appends a string slice to this destination.
    fn write_str<'s>(&mut self, strand: &mut Strand<'v, 's>, s: &str) -> Result<'v, 's, ()>;

    /// Appends a character to this destination.
    fn write_char<'s>(&mut self, strand: &mut Strand<'v, 's>, c: char) -> Result<'v, 's, ()> {
        let mut buf = [0; 4];
        self.write_str(strand, c.encode_utf8(&mut buf))
    }

    /// Writes formatted data to this destination.
    fn write_fmt<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        args: fmt::Arguments<'_>,
    ) -> Result<'v, 's, ()> {
        let mut writer = FormatWrite::new(self, strand);
        let result = fmt::write(&mut writer, args);
        let error = writer.error.take();
        drop(writer);
        match result {
            Ok(()) => Ok(()),
            Err(err) => Err(error.unwrap_or_else(|| Error::runtime(strand, err))),
        }
    }
}

struct FormatWrite<'v, 's, 'a, F: ?Sized> {
    format: &'a mut F,
    strand: &'a mut Strand<'v, 's>,
    error: Option<Error<'v, 's>>,
}

impl<'v, 's, 'a, F: ?Sized> FormatWrite<'v, 's, 'a, F> {
    fn new(format: &'a mut F, strand: &'a mut Strand<'v, 's>) -> Self {
        Self {
            format,
            strand,
            error: None,
        }
    }
}

impl<'v, F: Format<'v> + ?Sized> fmt::Write for FormatWrite<'v, '_, '_, F> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.format.write_str(self.strand, s).map_err(|error| {
            self.error = Some(error);
            fmt::Error
        })
    }

    fn write_char(&mut self, c: char) -> fmt::Result {
        self.format.write_char(self.strand, c).map_err(|error| {
            self.error = Some(error);
            fmt::Error
        })
    }
}

impl<'v, W: fmt::Write + ?Sized> Format<'v> for W {
    fn write_str<'s>(&mut self, strand: &mut Strand<'v, 's>, s: &str) -> Result<'v, 's, ()> {
        fmt::Write::write_str(self, s).map_err(|err| Error::runtime(strand, err))
    }

    fn write_char<'s>(&mut self, strand: &mut Strand<'v, 's>, c: char) -> Result<'v, 's, ()> {
        fmt::Write::write_char(self, c).map_err(|err| Error::runtime(strand, err))
    }

    fn write_fmt<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        args: fmt::Arguments<'_>,
    ) -> Result<'v, 's, ()> {
        fmt::Write::write_fmt(self, args).map_err(|err| Error::runtime(strand, err))
    }
}

/// Recovers the specification carried by a `std.FmtSpec` or `std.FmtValue`.
///
/// A native formatter that needs to apply a bound layout itself — rather than
/// letting the value format itself — reads it from here.
///
/// # Errors
/// Returns a type error if `value` is neither of those types.
pub fn spec_of<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, Spec> {
    crate::stdlib::fmt::spec_of(strand, value)
}

/// Fill behavior for a formatted value.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Fill {
    /// Use the default space fill.
    #[default]
    Default,
    /// Fill with a specific character.
    Char(char),
    /// Pad numeric values with zeroes after their sign and radix prefix.
    Zero,
}

/// Alignment within the requested width.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// Sign behavior for numeric values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Sign {
    Plus,
    Space,
}

/// Representation requested for a formatted value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Kind {
    Str,
    Dbg,
    Verbatim,
    Hex,
    Oct,
    Bin,
    Dec,
    Exp,
    Fixed,
}

impl Kind {
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            Self::Str => "STR",
            Self::Dbg => "DBG",
            Self::Verbatim => "VERBATIM",
            Self::Hex => "HEX",
            Self::Oct => "OCT",
            Self::Bin => "BIN",
            Self::Dec => "DEC",
            Self::Exp => "EXP",
            Self::Fixed => "FIXED",
        }
    }

    pub(crate) fn is_text(self) -> bool {
        matches!(self, Self::Str | Self::Dbg | Self::Verbatim)
    }
}

/// A formatting request.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Spec {
    pub fill: Fill,
    pub align: Option<Align>,
    pub sign: Option<Sign>,
    pub width: Option<u32>,
    pub precision: Option<u32>,
    pub alt: bool,
    pub kind: Option<Kind>,
}

/// A buffered formatting sink which applies clipping and padding when finished.
#[must_use = "formatted output is not written until Pad::finish is called"]
pub struct Pad<'a, 'v> {
    spec: Spec,
    sink: &'a mut dyn Format<'v>,
    measure: fn(&str) -> usize,
    buffer: String,
}

impl<'a, 'v> Pad<'a, 'v> {
    /// Creates a padding adapter using extended grapheme count.
    pub fn new(spec: Spec, sink: &'a mut dyn Format<'v>) -> Self {
        Self::with_measure(spec, sink, grapheme_count)
    }

    /// Creates a padding adapter using a custom width measurement function.
    pub fn with_measure(
        spec: Spec,
        sink: &'a mut dyn Format<'v>,
        measure: fn(&str) -> usize,
    ) -> Self {
        Self {
            spec,
            sink,
            measure,
            buffer: String::new(),
        }
    }

    /// Applies the specification and writes the buffered output to the underlying sink.
    pub fn finish<'s>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        let precision = self.spec.precision.map(|value| value as usize);
        let content = precision.map_or(self.buffer.as_str(), |budget| {
            clip_prefix_by(&self.buffer, budget, self.measure)
        });
        let content_width = (self.measure)(content);
        let requested = self
            .spec
            .width
            .map_or(content_width, |width| width as usize);
        let padding = requested.saturating_sub(content_width);
        let align = self.spec.align.unwrap_or(Align::Left);
        if self.spec.fill == Fill::Zero {
            if !matches!(self.spec.align, None | Some(Align::Right)) {
                return Err(Error::type_error(
                    strand,
                    "zero fill requires right alignment",
                ));
            }
            let split = numeric_prefix_len(content);
            self.sink.write_str(strand, &content[..split])?;
            write_fill(strand, self.sink, Fill::Zero, padding, self.measure)?;
            return self.sink.write_str(strand, &content[split..]);
        }
        let (left, right) = match align {
            Align::Left => (0, padding),
            Align::Right => (padding, 0),
            Align::Center => (padding / 2, padding - padding / 2),
        };
        write_fill(strand, self.sink, self.spec.fill, left, self.measure)?;
        self.sink.write_str(strand, content)?;
        write_fill(strand, self.sink, self.spec.fill, right, self.measure)
    }
}

fn numeric_prefix_len(value: &str) -> usize {
    let sign = value
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b'+' | b'-' | b' ')) as usize;
    let radix = value[sign..]
        .get(..2)
        .is_some_and(|prefix| matches!(prefix, "0x" | "0o" | "0b"));
    sign + usize::from(radix) * 2
}

impl<'v> Format<'v> for Pad<'_, 'v> {
    fn write_str<'s>(&mut self, _strand: &mut Strand<'v, 's>, value: &str) -> Result<'v, 's, ()> {
        self.buffer.push_str(value);
        Ok(())
    }
}

fn clip_prefix_by(value: &str, width: usize, measure: fn(&str) -> usize) -> &str {
    let mut end = 0;
    for (offset, grapheme) in value.grapheme_indices(true) {
        let candidate_end = offset + grapheme.len();
        if measure(&value[..candidate_end]) > width {
            break;
        }
        end = candidate_end;
    }
    &value[..end]
}

fn grapheme_count(value: &str) -> usize {
    value.graphemes(true).count()
}

fn write_fill<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sink: &mut dyn Format<'v>,
    fill: Fill,
    width: usize,
    measure: fn(&str) -> usize,
) -> Result<'v, 's, ()> {
    if width == 0 {
        return Ok(());
    }
    let ch = match fill {
        Fill::Default => ' ',
        Fill::Char(ch) => ch,
        Fill::Zero => '0',
    };
    let mut buffer = [0; 4];
    let fill_width = measure(ch.encode_utf8(&mut buffer));
    let repeats = width.checked_div(fill_width).unwrap_or(0);
    let remainder = width.checked_rem(fill_width).unwrap_or(width);
    for _ in 0..repeats {
        sink.write_char(strand, ch)?;
    }
    for _ in 0..remainder {
        sink.write_char(strand, ' ')?;
    }
    Ok(())
}

pub(crate) fn unresolved_kind<'v, 's>(strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
    Error::type_error(strand, "unresolved format kind")
}

pub(crate) fn format_prim<'v, 's>(
    prim: Prim,
    strand: &mut Strand<'v, 's>,
    spec: &Spec,
    sink: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    let kind = spec.kind.ok_or_else(|| unresolved_kind(strand))?;
    match prim {
        Prim::Int(value) => format_int(value, strand, spec, sink),
        Prim::F64(value) => format_float(value, strand, spec, sink),
        Prim::Nil | Prim::Bool(_) => {
            if !kind.is_text() || spec.sign.is_some() || spec.alt || spec.fill == Fill::Zero {
                return Err(Error::type_error(strand, "unsupported format option"));
            }
            let mut pad = Pad::new(*spec, sink);
            crate::fmt!(strand, &mut pad, "{prim}")?;
            pad.finish(strand)
        }
    }
}

pub(crate) fn format_int<'v, 's>(
    value: i128,
    strand: &mut Strand<'v, 's>,
    spec: &Spec,
    sink: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    let kind = spec.kind.ok_or_else(|| unresolved_kind(strand))?;
    if spec.precision.is_some() || matches!(kind, Kind::Exp | Kind::Fixed) {
        return Err(Error::type_error(
            strand,
            "unsupported integer format option",
        ));
    }
    if spec.alt && matches!(kind, Kind::Str | Kind::Dbg | Kind::Verbatim | Kind::Dec) {
        return Err(Error::type_error(
            strand,
            "unsupported integer format option",
        ));
    }
    let magnitude = value.unsigned_abs();
    let digits = match kind {
        Kind::Str | Kind::Dbg | Kind::Verbatim | Kind::Dec => magnitude.to_string(),
        Kind::Hex => format!("{magnitude:x}"),
        Kind::Oct => format!("{magnitude:o}"),
        Kind::Bin => format!("{magnitude:b}"),
        Kind::Exp | Kind::Fixed => unreachable!(),
    };
    let sign = sign_prefix(value.is_negative(), spec.sign);
    let radix = if spec.alt {
        match kind {
            Kind::Hex => "0x",
            Kind::Oct => "0o",
            Kind::Bin => "0b",
            _ => "",
        }
    } else {
        ""
    };
    finish_numeric(strand, spec, sink, sign, radix, &digits)
}

pub(crate) fn format_float<'v, 's>(
    value: f64,
    strand: &mut Strand<'v, 's>,
    spec: &Spec,
    sink: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    let kind = spec.kind.ok_or_else(|| unresolved_kind(strand))?;
    if spec.alt || matches!(kind, Kind::Hex | Kind::Oct | Kind::Bin | Kind::Dec) {
        return Err(Error::type_error(strand, "unsupported float format option"));
    }
    let magnitude = value.abs();
    let digits = match (kind, spec.precision) {
        (Kind::Exp, Some(precision)) => {
            let precision = precision as usize;
            format!("{magnitude:.precision$e}")
        }
        (Kind::Exp, None) => format!("{magnitude:e}"),
        (Kind::Fixed | Kind::Str | Kind::Dbg | Kind::Verbatim, Some(precision)) => {
            let precision = precision as usize;
            format!("{magnitude:.precision$}")
        }
        (Kind::Fixed | Kind::Str | Kind::Dbg | Kind::Verbatim, None) => magnitude.to_string(),
        _ => unreachable!(),
    };
    finish_numeric(
        strand,
        spec,
        sink,
        sign_prefix(value.is_sign_negative(), spec.sign),
        "",
        &digits,
    )
}

fn sign_prefix(negative: bool, sign: Option<Sign>) -> &'static str {
    if negative {
        "-"
    } else {
        match sign {
            Some(Sign::Plus) => "+",
            Some(Sign::Space) => " ",
            None => "",
        }
    }
}

pub(crate) fn finish_numeric<'v, 's>(
    strand: &mut Strand<'v, 's>,
    spec: &Spec,
    sink: &mut dyn Format<'v>,
    sign: &str,
    prefix: &str,
    digits: &str,
) -> Result<'v, 's, ()> {
    let mut rendered = format!("{sign}{prefix}{digits}");
    let mut pad_spec = *spec;
    pad_spec.precision = None;
    pad_spec.align = Some(spec.align.unwrap_or(Align::Right));
    if spec.fill == Fill::Zero {
        if matches!(spec.align, Some(Align::Left | Align::Center)) {
            return Err(Error::type_error(
                strand,
                "zero fill requires right alignment",
            ));
        }
        let width = spec.width.map_or(0, |width| width as usize);
        let zeroes = width.saturating_sub(grapheme_count(&rendered));
        rendered = format!("{sign}{prefix}{}{digits}", "0".repeat(zeroes));
        pad_spec.width = None;
        pad_spec.fill = Fill::Default;
    }
    let mut pad = Pad::new(pad_spec, sink);
    pad.write_str(strand, &rendered)?;
    pad.finish(strand)
}

pub(crate) fn push_indented(out: &mut String, value: &str, spaces: usize) {
    for (index, line) in value.split('\n').enumerate() {
        if index != 0 {
            out.push('\n');
        }
        out.extend(std::iter::repeat_n(' ', spaces));
        out.push_str(line);
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::with_vm;

    use super::*;

    #[test]
    fn pad_buffers_clips_and_aligns_in_graphemes() {
        with_vm(async |strand, []| {
            let mut out = String::new();
            let mut pad = Pad::new(
                Spec {
                    fill: Fill::Char('.'),
                    align: Some(Align::Center),
                    width: Some(7),
                    precision: Some(3),
                    kind: Some(Kind::Str),
                    ..Default::default()
                },
                &mut out,
            );
            pad.write_str(strand, "a").unwrap();
            pad.write_str(strand, "界b").unwrap();
            pad.finish(strand).unwrap();
            assert_eq!(out, "..a界b..");
        });
    }

    #[test]
    fn pad_uses_custom_measure_for_fill() {
        with_vm(async |strand, []| {
            let mut out = String::new();
            let mut pad = Pad::with_measure(
                Spec {
                    fill: Fill::Char('界'),
                    width: Some(4),
                    kind: Some(Kind::Str),
                    ..Default::default()
                },
                &mut out,
                |value| value.chars().filter(|ch| ch.is_ascii_alphabetic()).count(),
            );
            pad.write_str(strand, "ab").unwrap();
            pad.finish(strand).unwrap();
            assert_eq!(out, "ab  ");
        });
    }
}
