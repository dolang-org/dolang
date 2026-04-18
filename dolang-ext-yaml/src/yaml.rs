use std::{borrow::Cow, collections::HashSet, num::NonZero};

use ordered_float::OrderedFloat;
use saphyr::{Mapping as YamlMapping, Scalar, Yaml, YamlEmitter};

use dolang::runtime::{
    Output, Slot, Strand, Value,
    error::{Error, Result, ResultExt},
    unpack,
    value::{Empty, Nil, View},
    vm::Builder,
};
use saphyr_parser::{Event, Parser, ScalarStyle, Span, StrInput};

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    builder
        .module("yaml")
        .function("from_str", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected str"))?
                .pin();

            parse_yaml(strand, &src, out)
        })
        .function("to_str", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let mut seen = HashSet::new();
            let yaml = value_to_yaml(strand, &arg, &mut seen)?;
            let mut output = String::new();
            YamlEmitter::new(&mut output).dump(&yaml).into_do(strand)?;
            let result = output.strip_prefix("---\n").unwrap_or(&output);
            Output::set(strand, out, result);
            Ok(())
        })
        .commit();
}

fn value_to_yaml<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    seen: &mut HashSet<NonZero<usize>>,
) -> Result<'v, 's, Yaml<'static>> {
    match value.view(strand.vm()) {
        View::Nil => Ok(Yaml::Value(Scalar::Null)),
        View::Bool(b) => Ok(Yaml::Value(Scalar::Boolean(b))),
        View::Int(i) => Ok(Yaml::Value(Scalar::Integer(i))),
        View::Float(f) => Ok(Yaml::Value(Scalar::FloatingPoint(OrderedFloat(f)))),
        View::Str(s) => Ok(Yaml::Value(Scalar::String(Cow::Owned(s.into())))),
        View::Sym(sym) => Ok(Yaml::Value(Scalar::String(Cow::Owned(
            sym.as_str(strand.vm()).to_owned(),
        )))),
        View::Bin(_) => Err(Error::type_error(
            strand,
            "binary values are not YAML-representable",
        )),
        View::Array(handle) => {
            let id = handle.id();
            if !seen.insert(id.addr()) {
                return Err(Error::type_error(strand, "cyclic reference to array"));
            }
            let len = handle.len(strand)?;
            let mut seq = Vec::with_capacity(len);
            for i in 0..len {
                strand.with_slots_sync(|strand, [mut elem]| {
                    handle.get(strand, i, &mut elem)?;
                    seq.push(value_to_yaml(strand, &elem, seen)?);
                    Ok(())
                })?;
            }
            seen.remove(&id.addr());
            Ok(Yaml::Sequence(seq))
        }
        View::Dict(handle) => {
            let id = handle.id();
            if !seen.insert(id.addr()) {
                return Err(Error::type_error(strand, "cyclic reference to dict"));
            }
            let mut map = YamlMapping::new();
            let mut pairs = handle.pairs();
            strand.with_slots_sync(|strand, [mut key, mut val]| {
                while pairs.next(strand, &mut key, &mut val)? {
                    let yaml_key = value_to_yaml(strand, &key, seen)?;
                    let yaml_val = value_to_yaml(strand, &val, seen)?;
                    map.insert(yaml_key, yaml_val);
                }
                Ok(())
            })?;
            seen.remove(&id.addr());
            Ok(Yaml::Mapping(map))
        }
        View::Record(handle) => {
            let id = handle.id();
            if !seen.insert(id.addr()) {
                return Err(Error::type_error(strand, "cyclic reference to record"));
            }
            let mut map = YamlMapping::new();
            let mut pairs = handle.pairs();
            strand.with_slots_sync(|strand, [mut key, mut val]| {
                while pairs.next(strand, &mut key, &mut val)? {
                    let yaml_key = value_to_yaml(strand, &key, seen)?;
                    let yaml_val = value_to_yaml(strand, &val, seen)?;
                    map.insert(yaml_key, yaml_val);
                }
                Ok(())
            })?;
            seen.remove(&id.addr());
            Ok(Yaml::Mapping(map))
        }
        View::Tuple(handle) => {
            let id = handle.id();
            if !seen.insert(id.addr()) {
                return Err(Error::type_error(strand, "cyclic reference to tuple"));
            }
            let len = handle.len();
            let mut seq = Vec::with_capacity(len);
            for i in 0..len {
                strand.with_slots_sync(|strand, [mut elem]| {
                    handle.get(strand, i, &mut elem)?;
                    seq.push(value_to_yaml(strand, &elem, seen)?);
                    Ok(())
                })?;
            }
            seen.remove(&id.addr());
            Ok(Yaml::Sequence(seq))
        }
        View::Object(_) => Err(Error::type_error(strand, "unsupported YAML type")),
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
        None => return Err(Error::type_error(strand, "no YAML document")),
    }

    let (event, span) = match next_event(strand, &mut parser)? {
        Some(pair) => pair,
        None => return Err(Error::type_error(strand, "no YAML document")),
    };

    match event {
        Event::DocumentStart(_explicit) => {}
        Event::StreamEnd => return Err(Error::type_error(strand, "no YAML document")),
        _ => {
            return Err(error_at(
                strand,
                span,
                "internal YAML parse error: expected document start",
            ));
        }
    }

    let first = next_event(strand, &mut parser)?
        .ok_or_else(|| Error::type_error(strand, "invalid YAML document"))?;
    parse_event_into(strand, &mut parser, first, out)?;

    match next_event(strand, &mut parser)? {
        Some((Event::DocumentEnd, _)) => {}
        Some((Event::DocumentStart(_), span)) => {
            return Err(error_at(
                strand,
                span,
                "yaml.from_str only accepts a single document",
            ));
        }
        Some((_, span)) => {
            return Err(error_at(
                strand,
                span,
                "internal YAML parse error: expected document end",
            ));
        }
        None => return Err(Error::type_error(strand, "invalid YAML document")),
    }

    match next_event(strand, &mut parser)? {
        Some((Event::StreamEnd, _)) | None => Ok(()),
        Some((Event::DocumentStart(_), span)) => Err(error_at(
            strand,
            span,
            "yaml.from_str only accepts a single document",
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
                    .ok_or_else(|| Error::type_error(strand, "invalid YAML sequence"))?;
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
                    .ok_or_else(|| Error::type_error(strand, "invalid YAML mapping"))?;
                if matches!(next.0, Event::MappingEnd) {
                    break;
                }

                strand.with_slots_sync(|strand, [mut key, mut value]| {
                    parse_event_into(strand, parser, next, Slot::reborrow(&mut key))?;
                    let value_event = next_event(strand, parser)?.ok_or_else(|| {
                        Error::type_error(strand, "invalid YAML mapping: missing value for key")
                    })?;
                    if matches!(value_event.0, Event::MappingEnd) {
                        return Err(error_at(
                            strand,
                            value_event.1,
                            "invalid YAML mapping: missing value for key",
                        ));
                    }
                    parse_event_into(strand, parser, value_event, Slot::reborrow(&mut value))?;
                    dict.insert(strand, &key, &value)
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
        let (event, span) = event.map_err(|err| Error::runtime(strand, err))?;
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
    Error::type_error(
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

fn parse_int<'v, 's>(strand: &mut Strand<'v, 's>, value: &str) -> Result<'v, 's, Option<i64>> {
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
        i64::from_str_radix(digits, radix)
            .map(i128::from)
            .map(|value| -value)
            .map_err(|_| Error::type_error(strand, "numeric overflow"))?
    } else {
        i64::from_str_radix(digits, radix)
            .map(i128::from)
            .map_err(|_| Error::type_error(strand, "numeric overflow"))?
    };

    let signed =
        i64::try_from(unsigned).map_err(|_| Error::type_error(strand, "numeric overflow"))?;
    Ok(Some(signed))
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
