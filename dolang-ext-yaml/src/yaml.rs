use std::{borrow::Cow, collections::HashSet, fmt, mem, num::NonZero};

use ordered_float::OrderedFloat;
use saphyr::{Mapping as YamlMapping, Scalar, Yaml, YamlEmitter};

use dolang::runtime::{
    Alloc, Output, Slot, Strand, Value,
    error::{Error, Result, ResultExt},
    unpack,
    value::{Array, Empty, Nil, PinStr, StrEmbryo, View},
    vm::Register,
};
use saphyr_parser::{Event, Parser, ScalarStyle, Span, StrInput};

pub(crate) fn configure<'v>(builder: &mut Register<'v>) {
    builder
        .module("yaml")
        .function("decode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected Str"))?
                .pin();

            parse_yaml(strand, &src, out)
        })
        .function("encode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            strand.with_slots_sync(|strand, [mut keep]| {
                Output::set(strand, &mut keep, Empty::Array);
                let mut builder = Builder {
                    keep: keep.as_array(strand.vm()).expect("keep must be an array"),
                    pins: Vec::new(),
                    seen: HashSet::new(),
                };
                // Declared after `builder`, so it drops first, even on error.
                let yaml = builder.value(strand, &arg)?;
                let mut buf = StrEmbryo::new();
                let mut writer = EmbryoWriter {
                    embryo: &mut buf,
                    alloc: strand,
                    skip: "---\n",
                };
                let result = YamlEmitter::new(&mut writer).dump(&yaml);
                result.into_do(strand)?;
                buf.finish(strand, out);
                Ok(())
            })
        })
        .commit();
}

/// Appends emitter output to an embryo, discarding the leading document
/// marker.
struct EmbryoWriter<'a, 'v> {
    embryo: &'a mut StrEmbryo<'v>,
    alloc: &'a mut dyn Alloc<'v>,
    /// The part of the document marker not yet discarded.
    skip: &'static str,
}

impl fmt::Write for EmbryoWriter<'_, '_> {
    fn write_str(&mut self, mut s: &str) -> fmt::Result {
        if !self.skip.is_empty() {
            if let Some(rest) = s.strip_prefix(self.skip) {
                self.skip = "";
                s = rest;
            } else if let Some(rest) = self.skip.strip_prefix(s) {
                self.skip = rest;
                return Ok(());
            } else {
                debug_assert!(false, "YAML emitter output must start with the marker");
                self.skip = "";
            }
        }
        self.embryo.extend(self.alloc, s);
        Ok(())
    }
}

/// Builds a saphyr tree whose strings borrow from the encoded values.
struct Builder<'k, 'v> {
    /// Holds every `Str` and `Sym` whose text the tree borrows.
    keep: Array<'v, 'k>,
    /// Pins of the borrowed `Str` texts.
    pins: Vec<PinStr<'v, 'static>>,
    seen: HashSet<NonZero<usize>>,
}

impl<'v> Builder<'_, 'v> {
    fn value<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, Yaml<'static>> {
        match value.view(strand.vm()) {
            View::Nil => Ok(Yaml::Value(Scalar::Null)),
            View::Bool(b) => Ok(Yaml::Value(Scalar::Boolean(b))),
            View::Int(i) => Ok(Yaml::Value(Scalar::Integer(
                i64::try_from(i).map_err(|_| Error::overflow(strand))?,
            ))),
            View::Float(f) => Ok(Yaml::Value(Scalar::FloatingPoint(OrderedFloat(f)))),
            View::Str(s) => {
                self.keep.push(strand, value)?;
                // SAFETY: `keep` roots the string until the encode ends, and the
                // pin is dropped only after the tree that borrows its text.
                let pin = unsafe { s.pin().into_static_unchecked() };
                // SAFETY: as above; the text lives as long as the pin.
                let text = unsafe { mem::transmute::<&str, &'static str>(&pin) };
                self.pins.push(pin);
                Ok(Yaml::Value(Scalar::String(Cow::Borrowed(text))))
            }
            View::Sym(sym) => {
                let text = sym.as_str(strand.vm());
                // SAFETY: the name lives in the symbol object, which `keep`
                // roots until the tree that borrows it is dropped.
                let text = unsafe { mem::transmute::<&str, &'static str>(text) };
                self.keep.push(strand, value)?;
                Ok(Yaml::Value(Scalar::String(Cow::Borrowed(text))))
            }
            View::Bin(_) => Err(Error::type_error(
                strand,
                "binary values are not YAML-representable",
            )),
            View::Array(handle) => {
                let id = handle.id().addr();
                self.enter(strand, id, "array")?;
                let mut seq = Vec::new();
                strand.with_slots_sync(|strand, [mut elem]| {
                    let len = handle.len(strand)?;
                    seq.reserve(len);
                    for i in 0..len {
                        if !handle.get(strand, i, &mut elem)? {
                            break;
                        }
                        seq.push(self.value(strand, &elem)?);
                    }
                    Ok(())
                })?;
                self.seen.remove(&id);
                Ok(Yaml::Sequence(seq))
            }
            View::Tuple(handle) => {
                let id = handle.id().addr();
                self.enter(strand, id, "tuple")?;
                let mut seq = Vec::with_capacity(handle.len());
                strand.with_slots_sync(|strand, [mut elem]| {
                    for i in 0..handle.len() {
                        if !handle.get(strand, i, &mut elem)? {
                            break;
                        }
                        seq.push(self.value(strand, &elem)?);
                    }
                    Ok(())
                })?;
                self.seen.remove(&id);
                Ok(Yaml::Sequence(seq))
            }
            View::Dict(handle) => {
                let id = handle.id().addr();
                self.enter(strand, id, "dict")?;
                let mut pairs = handle.pairs();
                let map = self.mapping(strand, |strand, key, val| pairs.next(strand, key, val))?;
                self.seen.remove(&id);
                Ok(Yaml::Mapping(map))
            }
            View::Record(handle) => {
                let id = handle.id().addr();
                self.enter(strand, id, "record")?;
                let mut pairs = handle.pairs();
                let map = self.mapping(strand, |strand, key, val| pairs.next(strand, key, val))?;
                self.seen.remove(&id);
                Ok(Yaml::Mapping(map))
            }
            View::Struct(handle) => {
                let id = handle.id().addr();
                self.enter(strand, id, "struct")?;
                let mut pairs = handle.pairs();
                let map = self.mapping(strand, |strand, key, val| pairs.next(strand, key, val))?;
                self.seen.remove(&id);
                Ok(Yaml::Mapping(map))
            }
            _ => Err(Error::type_error(strand, "unsupported YAML type")),
        }
    }

    fn enter<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        id: NonZero<usize>,
        kind: &str,
    ) -> Result<'v, 's, ()> {
        if self.seen.insert(id) {
            Ok(())
        } else {
            Err(Error::type_error(
                strand,
                format!("cyclic reference to {kind}"),
            ))
        }
    }

    /// Builds a mapping from the pairs `next` yields.
    fn mapping<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut next: impl FnMut(&mut Strand<'v, 's>, Slot<'v, '_>, Slot<'v, '_>) -> Result<'v, 's, bool>,
    ) -> Result<'v, 's, YamlMapping<'static>> {
        let mut map = YamlMapping::new();
        strand.with_slots_sync(|strand, [mut key, mut val]| {
            while next(strand, Slot::reborrow(&mut key), Slot::reborrow(&mut val))? {
                let yaml_key = self.value(strand, &key)?;
                let yaml_val = self.value(strand, &val)?;
                map.insert(yaml_key, yaml_val);
            }
            Ok(())
        })?;
        Ok(map)
    }
}

fn parse_yaml<'v, 's, 'i>(
    strand: &mut Strand<'v, 's>,
    src: &'i str,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut parser: Parser<'i, StrInput<'i>> = Parser::new_from_str(src);

    match next_event(strand, &mut parser)? {
        Some((Event::StreamStart, _)) => {}
        Some((_, span)) => {
            return Err(error_at(
                strand,
                span,
                "internal YAML parse error: expected stream start",
            ));
        }
        None => return Err(Error::data(strand, "no YAML document")),
    }

    let (event, span) = match next_event(strand, &mut parser)? {
        Some(pair) => pair,
        None => return Err(Error::data(strand, "no YAML document")),
    };

    match event {
        Event::DocumentStart(_explicit) => {}
        Event::StreamEnd => return Err(Error::data(strand, "no YAML document")),
        _ => {
            return Err(error_at(
                strand,
                span,
                "internal YAML parse error: expected document start",
            ));
        }
    }

    let first = next_event(strand, &mut parser)?
        .ok_or_else(|| Error::data(strand, "invalid YAML document"))?;
    parse_event_into(strand, &mut parser, first, out)?;

    match next_event(strand, &mut parser)? {
        Some((Event::DocumentEnd, _)) => {}
        Some((Event::DocumentStart(_), span)) => {
            return Err(error_at(
                strand,
                span,
                "yaml.decode only accepts a single document",
            ));
        }
        Some((_, span)) => {
            return Err(error_at(
                strand,
                span,
                "internal YAML parse error: expected document end",
            ));
        }
        None => return Err(Error::data(strand, "invalid YAML document")),
    }

    match next_event(strand, &mut parser)? {
        Some((Event::StreamEnd, _)) | None => Ok(()),
        Some((Event::DocumentStart(_), span)) => Err(error_at(
            strand,
            span,
            "yaml.decode only accepts a single document",
        )),
        Some((_, span)) => Err(error_at(
            strand,
            span,
            "internal YAML parse error: unexpected trailing event",
        )),
    }
}

fn parse_event_into<'v, 's, 'i>(
    strand: &mut Strand<'v, 's>,
    parser: &mut Parser<'i, StrInput<'i>>,
    (event, span): (Event<'i>, Span),
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    match event {
        Event::Nothing
        | Event::StreamStart
        | Event::StreamEnd
        | Event::DocumentStart(_)
        | Event::DocumentEnd
        | Event::SequenceEnd
        | Event::MappingEnd => Err(error_at(
            strand,
            span,
            "internal YAML parse error: unexpected structural event",
        )),

        Event::Alias(_anchor) => Err(error_at(strand, span, "YAML aliases are not supported")),

        Event::Scalar(value, style, _anchor, tag) => {
            if let Some(tag) = tag {
                return Err(error_at(
                    strand,
                    span,
                    format!("YAML tags are not supported: {tag}"),
                ));
            }
            scalar_into_slot(strand, value.as_ref(), style, out)
        }

        Event::SequenceStart(_anchor, tag) => {
            if let Some(tag) = tag {
                return Err(error_at(
                    strand,
                    span,
                    format!("YAML tags are not supported: {tag}"),
                ));
            }

            Output::set(strand, &mut out, Empty::Array);
            let array = out
                .as_array(strand.vm())
                .expect("sequence output must be an array");

            loop {
                let next = next_event(strand, parser)?
                    .ok_or_else(|| Error::data(strand, "invalid YAML sequence"))?;
                if matches!(next.0, Event::SequenceEnd) {
                    break;
                }

                strand.with_slots_sync(|strand, [mut item]| {
                    parse_event_into(strand, parser, next, Slot::reborrow(&mut item))?;
                    array.push(strand, &item)
                })?;
            }

            Ok(())
        }

        Event::MappingStart(_anchor, tag) => {
            if let Some(tag) = tag {
                return Err(error_at(
                    strand,
                    span,
                    format!("YAML tags are not supported: {tag}"),
                ));
            }

            Output::set(strand, &mut out, Empty::Dict);
            let dict = out
                .as_dict(strand.vm())
                .expect("mapping output must be a dict");

            loop {
                let next = next_event(strand, parser)?
                    .ok_or_else(|| Error::data(strand, "invalid YAML mapping"))?;
                if matches!(next.0, Event::MappingEnd) {
                    break;
                }

                strand.with_slots_sync(|strand, [mut key, mut value]| {
                    parse_event_into(strand, parser, next, Slot::reborrow(&mut key))?;
                    let value_event = next_event(strand, parser)?.ok_or_else(|| {
                        Error::data(strand, "invalid YAML mapping: missing value for key")
                    })?;
                    if matches!(value_event.0, Event::MappingEnd) {
                        return Err(error_at(
                            strand,
                            value_event.1,
                            "invalid YAML mapping: missing value for key",
                        ));
                    }
                    parse_event_into(strand, parser, value_event, Slot::reborrow(&mut value))?;
                    dict.insert(strand, &key, &value, false)
                })?;
            }

            Ok(())
        }
    }
}

fn next_event<'v, 's, 'i>(
    strand: &mut Strand<'v, 's>,
    parser: &mut Parser<'i, StrInput<'i>>,
) -> Result<'v, 's, Option<(Event<'i>, Span)>> {
    loop {
        let Some(event) = parser.next() else {
            return Ok(None);
        };
        let (event, span) = event.map_err(|err| Error::data(strand, err.to_string()))?;
        if !matches!(event, Event::Nothing) {
            return Ok(Some((event, span)));
        }
    }
}

fn scalar_into_slot<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &str,
    style: ScalarStyle,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if style != ScalarStyle::Plain {
        Output::set(strand, out, value);
        return Ok(());
    }

    if is_null(value) {
        Output::set(strand, out, Nil);
    } else if let Some(bool) = parse_bool(value) {
        Output::set(strand, out, bool);
    } else if let Some(int) = parse_int(strand, value)? {
        Output::set(strand, out, int);
    } else if let Some(float) = parse_float(value) {
        Output::set(strand, out, float);
    } else {
        Output::set(strand, out, value);
    }

    Ok(())
}

fn error_at<'v, 's>(
    strand: &mut Strand<'v, 's>,
    span: Span,
    message: impl Into<String>,
) -> Error<'v, 's> {
    Error::data(
        strand,
        format!(
            "{} at line {} column {}",
            message.into(),
            span.start.line(),
            span.start.col()
        ),
    )
}

fn is_null(value: &str) -> bool {
    matches!(value, "" | "~" | "null" | "Null" | "NULL")
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

fn parse_int<'v, 's>(strand: &mut Strand<'v, 's>, value: &str) -> Result<'v, 's, Option<i128>> {
    let normalized = value.replace('_', "");
    let text = normalized.as_str();
    if text.is_empty() {
        return Ok(None);
    }

    let (negative, digits) = match text.as_bytes()[0] {
        b'+' => (false, &text[1..]),
        b'-' => (true, &text[1..]),
        _ => (false, text),
    };

    if digits.is_empty() {
        return Ok(None);
    }

    let (radix, digits) = if let Some(rest) = digits.strip_prefix("0b") {
        (2, rest)
    } else if let Some(rest) = digits.strip_prefix("0o") {
        (8, rest)
    } else if let Some(rest) = digits.strip_prefix("0x") {
        (16, rest)
    } else {
        (10, digits)
    };

    if digits.is_empty() {
        return Ok(None);
    }

    let valid_digits = match radix {
        2 => digits.chars().all(|ch| matches!(ch, '0' | '1')),
        8 => digits.chars().all(|ch| ('0'..='7').contains(&ch)),
        10 => digits.chars().all(|ch| ch.is_ascii_digit()),
        16 => digits.chars().all(|ch| ch.is_ascii_hexdigit()),
        _ => false,
    };
    if !valid_digits {
        return Ok(None);
    }

    let unsigned = if negative {
        i128::from_str_radix(digits, radix)
            .map(|value| -value)
            .map_err(|_| Error::data(strand, "numeric overflow"))?
    } else {
        i128::from_str_radix(digits, radix).map_err(|_| Error::data(strand, "numeric overflow"))?
    };
    Ok(Some(unsigned))
}

fn parse_float(value: &str) -> Option<f64> {
    let normalized = value.replace('_', "");
    match normalized.as_str() {
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => Some(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => Some(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => Some(f64::NAN),
        text if text.chars().any(|ch| matches!(ch, '.' | 'e' | 'E')) => text.parse().ok(),
        _ => None,
    }
}
