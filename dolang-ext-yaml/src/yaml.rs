use std::{borrow::Cow, collections::HashSet, fmt, mem, num::NonZero, result};

use ordered_float::OrderedFloat;
use saphyr::{Mapping as YamlMapping, Scalar, Yaml, YamlEmitter};

use dolang::runtime::AllocExt;
use dolang::runtime::{
    Alloc, Input, Instance, Object, Output, Slot, State, Strand, Value,
    error::{Error, Result},
    object::{Mut, Ref, TypeBuilder, fmt},
    unpack,
    value::{Array, Nil, PinStr, StrEmbryo, TypeObject, View, fmt::Format},
    vm::Register,
    with_instance, with_slots,
};
use saphyr_parser::{Event, Marker, Parser, ScalarStyle, Span, StrInput};

use crate::global::{Global, Tag};

/// `yaml.DecodeError`: the document is not valid YAML, or uses a feature
/// `yaml.decode` rejects.
pub(crate) struct DecodeError;

/// What is wrong with a document.
#[derive(Clone, Copy)]
enum DecodeKind {
    /// The scanner rejected the text.
    Syntax,
    /// A tag or alias.
    Unsupported,
    /// The stream holds no document, or more than one.
    Documents,
    /// An integer scalar is outside the `Int` range.
    Range,
}

pub(crate) struct DecodeAnnex {
    message: String,
    kind: DecodeKind,
    line: usize,
    /// 1-based.
    column: usize,
}

/// `yaml.EncodeError`: the value has a YAML form in general, but not as given.
///
/// A `ValueError`: the fault is in the value the caller passed.
pub(crate) struct EncodeError;

/// Why a value could not be written.
#[derive(Clone, Copy)]
enum EncodeKind {
    /// The value contains itself.
    Cycle,
    /// An `Int` is outside the signed 64-bit range.
    Range,
}

pub(crate) struct EncodeAnnex {
    message: String,
    kind: EncodeKind,
}

/// Failure while encoding.
enum Fault<'v, 's> {
    /// Raised as is.
    Do(Error<'v, 's>),
    /// Raised as `yaml.EncodeError`, with the path [`Builder::within`]
    /// collected.
    Encode(EncodeAnnex),
}

impl<'v, 's> From<Error<'v, 's>> for Fault<'v, 's> {
    fn from(error: Error<'v, 's>) -> Self {
        Self::Do(error)
    }
}

impl Fault<'_, '_> {
    fn encode(kind: EncodeKind, message: impl Into<String>) -> Self {
        Self::Encode(EncodeAnnex {
            message: message.into(),
            kind,
        })
    }
}

type EncodeResult<'v, 's, T> = result::Result<T, Fault<'v, 's>>;

pub(crate) fn configure<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("yaml")
        .value("DecodeError", global.types.decode_error)
        .value("EncodeError", global.types.encode_error)
        .function("decode", async move |strand, args, out| {
            unpack!(strand, args => arg);
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected Str"))?
                .pin();

            parse_yaml(strand, &src, out)
        })
        .function("encode", async move |strand, args, out| {
            unpack!(strand, args => arg);
            with_slots!(strand, |mut keep, path, mut tuple, mut error| {
                strand.new_array(&mut keep);
                let mut builder = Builder {
                    keep: keep.as_array(strand.vm()).expect("keep must be an array"),
                    pins: Vec::new(),
                    seen: HashSet::new(),
                    path,
                };
                // Declared after `builder`, so it drops first, even on error.
                let yaml = match builder.value(strand, &arg) {
                    Ok(yaml) => yaml,
                    Err(Fault::Do(error)) => return Err(error),
                    Err(Fault::Encode(annex)) => {
                        match builder.path.as_array(strand.vm()) {
                            Some(path) => {
                                path.reverse(strand).expect("path is not shared");
                                path.to_tuple(strand, &mut tuple)
                                    .expect("path is not shared");
                            }
                            None => strand.tuple([Nil; 0], &mut tuple),
                        }
                        let ty = strand.state::<Tag>().types.encode_error;
                        ty.create_with_annex(strand, EncodeError, annex, &mut error);
                        with_instance!(strand, ty.cast(&error).unwrap(), |instance| {
                            let mut borrow = instance.borrow_mut_unwrap();
                            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &tuple);
                        });
                        return Err(Error::from_value(strand, &error));
                    }
                };
                let mut buf = StrEmbryo::new();
                let mut writer = EmbryoWriter {
                    embryo: &mut buf,
                    alloc: strand,
                    skip: "---\n",
                };
                let result = YamlEmitter::new(&mut writer).dump(&yaml);
                result.expect("writing to a StrEmbryo cannot fail");
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
    pins: Vec<PinStr<'v>>,
    seen: HashSet<NonZero<usize>>,
    /// Nil, or an `Array` of the steps from the fault outward.
    path: Slot<'v, 'k>,
}

impl<'v> Builder<'_, 'v> {
    fn value<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> EncodeResult<'v, 's, Yaml<'static>> {
        match value.view(strand.vm()) {
            View::Nil => Ok(Yaml::Value(Scalar::Null)),
            View::Bool(b) => Ok(Yaml::Value(Scalar::Boolean(b))),
            View::Int(i) => {
                let i = i64::try_from(i).map_err(|_| {
                    Fault::encode(
                        EncodeKind::Range,
                        "YAML integers outside i64 range are not supported",
                    )
                })?;
                Ok(Yaml::Value(Scalar::Integer(i)))
            }
            View::Float(f) => Ok(Yaml::Value(Scalar::FloatingPoint(OrderedFloat(f)))),
            View::Str(s) => {
                self.keep.push(strand, value)?;
                // The pin keeps the text alive and in place, and is dropped only
                // after the tree that borrows it.
                let pin = s.pin();
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
            View::Bin(_) => {
                Err(Error::type_error(strand, "binary values are not YAML-representable").into())
            }
            View::Array(handle) => {
                let id = handle.id().addr();
                self.enter(id, "array")?;
                let mut seq = Vec::new();
                with_slots!(strand, |mut elem| {
                    let len = handle.len(strand)?;
                    seq.reserve(len);
                    for i in 0..len {
                        if !handle.get(strand, i, &mut elem)? {
                            break;
                        }
                        let item = self
                            .value(strand, &elem)
                            .map_err(|fault| self.within(strand, fault, i))?;
                        seq.push(item);
                    }
                    EncodeResult::Ok(())
                })?;
                self.seen.remove(&id);
                Ok(Yaml::Sequence(seq))
            }
            View::Tuple(handle) => {
                let id = handle.id().addr();
                self.enter(id, "tuple")?;
                let mut seq = Vec::with_capacity(handle.len());
                with_slots!(strand, |mut elem| {
                    for i in 0..handle.len() {
                        if !handle.get(strand, i, &mut elem)? {
                            break;
                        }
                        let item = self
                            .value(strand, &elem)
                            .map_err(|fault| self.within(strand, fault, i))?;
                        seq.push(item);
                    }
                    EncodeResult::Ok(())
                })?;
                self.seen.remove(&id);
                Ok(Yaml::Sequence(seq))
            }
            View::Dict(handle) => {
                let id = handle.id().addr();
                self.enter(id, "dict")?;
                let mut pairs = handle.pairs();
                let map = self.mapping(strand, |strand, key, val| pairs.next(strand, key, val))?;
                self.seen.remove(&id);
                Ok(Yaml::Mapping(map))
            }
            View::Struct(handle) => {
                let id = handle.id().addr();
                self.enter(id, "struct")?;
                let mut pairs = handle.pairs();
                let map = self.mapping(strand, |strand, key, val| pairs.next(strand, key, val))?;
                self.seen.remove(&id);
                Ok(Yaml::Mapping(map))
            }
            View::Record(_) => {
                Err(Error::type_error(strand, "`Record` has no YAML encoding").into())
            }
            _ => Err(Error::type_error(strand, "unsupported YAML type").into()),
        }
    }

    /// Records that `fault` lies under `segment`, an index or a key.
    fn within<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        fault: Fault<'v, 's>,
        segment: impl Input<'v>,
    ) -> Fault<'v, 's> {
        if let Fault::Encode(_) = fault {
            if self.path.is_nil() {
                strand.new_array(&mut self.path);
            }
            let path = self.path.as_array(strand.vm()).expect("path is an Array");
            path.push(strand, segment).expect("path is not shared");
        }
        fault
    }

    fn enter<'s>(&mut self, id: NonZero<usize>, kind: &str) -> EncodeResult<'v, 's, ()> {
        if self.seen.insert(id) {
            Ok(())
        } else {
            Err(Fault::encode(
                EncodeKind::Cycle,
                format!("cyclic reference to {kind}"),
            ))
        }
    }

    /// Builds a mapping from the pairs `next` yields.
    ///
    /// A fault in a key leaves the path at the mapping.
    fn mapping<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut next: impl FnMut(&mut Strand<'v, 's>, Slot<'v, '_>, Slot<'v, '_>) -> Result<'v, 's, bool>,
    ) -> EncodeResult<'v, 's, YamlMapping<'static>> {
        let mut map = YamlMapping::new();
        with_slots!(strand, |mut key, mut val| {
            while next(strand, Slot::reborrow(&mut key), Slot::reborrow(&mut val))? {
                let yaml_key = self.value(strand, &key)?;
                let yaml_val = self
                    .value(strand, &val)
                    .map_err(|fault| self.within(strand, fault, &*key))?;
                map.insert(yaml_key, yaml_val);
            }
            EncodeResult::Ok(())
        })?;
        Ok(map)
    }
}

/// Raises `yaml.DecodeError` positioned at `mark`.
fn decode_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    kind: DecodeKind,
    mark: Marker,
    message: impl fmt::Display,
) -> Error<'v, 's> {
    let line = mark.line();
    let column = mark.col() + 1;
    let global = strand.state::<Tag>();
    Error::object_with_annex(
        strand,
        global.types.decode_error,
        DecodeError,
        DecodeAnnex {
            message: format!("{message} at line {line} column {column}"),
            kind,
            line,
            column,
        },
    )
}

/// Raises for an event sequence the parser should never produce.
fn internal<'v, 's>(strand: &mut Strand<'v, 's>, span: Span, what: &str) -> Error<'v, 's> {
    debug_assert!(false, "internal YAML parse error: {what}");
    decode_error(
        strand,
        DecodeKind::Syntax,
        span.start,
        format_args!("internal YAML parse error: {what}"),
    )
}

fn parse_yaml<'v, 's, 'i>(
    strand: &mut Strand<'v, 's>,
    src: &'i str,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut parser: Parser<'i, StrInput<'i>> = Parser::new_from_str(src);

    match next_event(strand, &mut parser)? {
        (Event::StreamStart, _) => {}
        (_, span) => return Err(internal(strand, span, "expected stream start")),
    }

    match next_event(strand, &mut parser)? {
        (Event::DocumentStart(_explicit), _) => {}
        (Event::StreamEnd, span) => {
            return Err(decode_error(
                strand,
                DecodeKind::Documents,
                span.start,
                "no YAML document",
            ));
        }
        (_, span) => return Err(internal(strand, span, "expected document start")),
    }

    let first = next_event(strand, &mut parser)?;
    parse_event_into(strand, &mut parser, first, out)?;

    match next_event(strand, &mut parser)? {
        (Event::DocumentEnd, _) => {}
        (_, span) => return Err(internal(strand, span, "expected document end")),
    }

    match next_event(strand, &mut parser)? {
        (Event::StreamEnd, _) => Ok(()),
        (Event::DocumentStart(_), span) => Err(decode_error(
            strand,
            DecodeKind::Documents,
            span.start,
            "yaml.decode only accepts a single document",
        )),
        (_, span) => Err(internal(strand, span, "unexpected trailing event")),
    }
}

/// Raises for a tag, which `yaml.decode` does not resolve.
fn tag_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    span: Span,
    tag: impl fmt::Display,
) -> Error<'v, 's> {
    decode_error(
        strand,
        DecodeKind::Unsupported,
        span.start,
        format_args!("YAML tags are not supported: {tag}"),
    )
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
        | Event::MappingEnd => Err(internal(strand, span, "unexpected structural event")),

        Event::Alias(_anchor) => Err(decode_error(
            strand,
            DecodeKind::Unsupported,
            span.start,
            "YAML aliases are not supported",
        )),

        Event::Scalar(value, style, _anchor, tag) => {
            if let Some(tag) = tag {
                return Err(tag_error(strand, span, tag));
            }
            scalar_into_slot(strand, value.as_ref(), style, span, out)
        }

        Event::SequenceStart(_anchor, tag) => {
            if let Some(tag) = tag {
                return Err(tag_error(strand, span, tag));
            }

            strand.new_array(&mut out);
            let array = out
                .as_array(strand.vm())
                .expect("sequence output must be an array");

            loop {
                let next = next_event(strand, parser)?;
                if matches!(next.0, Event::SequenceEnd) {
                    break;
                }

                with_slots!(strand, |mut item| {
                    parse_event_into(strand, parser, next, Slot::reborrow(&mut item))?;
                    array.push(strand, &item)
                })?;
            }

            Ok(())
        }

        Event::MappingStart(_anchor, tag) => {
            if let Some(tag) = tag {
                return Err(tag_error(strand, span, tag));
            }

            strand.new_dict(&mut out);
            let dict = out
                .as_dict(strand.vm())
                .expect("mapping output must be a dict");

            loop {
                let next = next_event(strand, parser)?;
                if matches!(next.0, Event::MappingEnd) {
                    break;
                }

                with_slots!(strand, |mut key, mut value| {
                    parse_event_into(strand, parser, next, Slot::reborrow(&mut key))?;
                    let value_event = next_event(strand, parser)?;
                    if matches!(value_event.0, Event::MappingEnd) {
                        return Err(internal(strand, value_event.1, "missing value for key"));
                    }
                    parse_event_into(strand, parser, value_event, Slot::reborrow(&mut value))?;
                    dict.insert(strand, &key, &value, false)
                })?;
            }

            Ok(())
        }
    }
}

/// The next meaningful event. The parser ends with `StreamEnd`, so running
/// out first is an internal error.
fn next_event<'v, 's, 'i>(
    strand: &mut Strand<'v, 's>,
    parser: &mut Parser<'i, StrInput<'i>>,
) -> Result<'v, 's, (Event<'i>, Span)> {
    loop {
        let Some(event) = parser.next() else {
            let end = Span::empty(Marker::default());
            return Err(internal(strand, end, "unexpected end of events"));
        };
        let (event, span) = event
            .map_err(|err| decode_error(strand, DecodeKind::Syntax, *err.marker(), err.info()))?;
        if !matches!(event, Event::Nothing) {
            return Ok((event, span));
        }
    }
}

fn scalar_into_slot<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &str,
    style: ScalarStyle,
    span: Span,
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
    } else if let Some(int) = parse_int(value) {
        let int = int.ok_or_else(|| {
            decode_error(strand, DecodeKind::Range, span.start, "numeric overflow")
        })?;
        Output::set(strand, out, int);
    } else if let Some(float) = parse_float(value) {
        Output::set(strand, out, float);
    } else {
        Output::set(strand, out, value);
    }

    Ok(())
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

/// Parses an integer scalar: `None` if `value` isn't one, `Some(None)` if it
/// is but doesn't fit.
fn parse_int(value: &str) -> Option<Option<i128>> {
    let normalized = value.replace('_', "");
    let text = normalized.as_str();
    if text.is_empty() {
        return None;
    }

    let (negative, digits) = match text.as_bytes()[0] {
        b'+' => (false, &text[1..]),
        b'-' => (true, &text[1..]),
        _ => (false, text),
    };

    if digits.is_empty() {
        return None;
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
        return None;
    }

    let valid_digits = match radix {
        2 => digits.chars().all(|ch| matches!(ch, '0' | '1')),
        8 => digits.chars().all(|ch| ('0'..='7').contains(&ch)),
        10 => digits.chars().all(|ch| ch.is_ascii_digit()),
        16 => digits.chars().all(|ch| ch.is_ascii_hexdigit()),
        _ => false,
    };
    if !valid_digits {
        return None;
    }

    let magnitude = i128::from_str_radix(digits, radix).ok();
    Some(if negative {
        magnitude.map(|value| -value)
    } else {
        magnitude
    })
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

impl<'v> Object<'v> for DecodeError {
    const NAME: &'v str = "DecodeError";
    const MODULE: &'v str = "yaml";
    type Annex = DecodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let syntax = builder.sym("SYNTAX");
        let unsupported = builder.sym("UNSUPPORTED");
        let documents = builder.sym("DOCUMENTS");
        let range = builder.sym("RANGE");
        builder
            .nominal_supertype(TypeObject::DataError)
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    DecodeKind::Syntax => syntax,
                    DecodeKind::Unsupported => unsupported,
                    DecodeKind::Documents => documents,
                    DecodeKind::Range => range,
                };
                Output::set(strand, out, kind);
                Ok(())
            })
            .get("line", |this, strand, out| {
                Output::set(strand, out, this.annex().line);
                Ok(())
            })
            .get("column", |this, strand, out| {
                Output::set(strand, out, this.annex().column);
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<yaml.DecodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> Object<'v> for EncodeError {
    const NAME: &'v str = "EncodeError";
    const MODULE: &'v str = "yaml";
    /// Slot 0 is the path tuple.
    const SLOTS: usize = 1;
    type Annex = EncodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let cycle = builder.sym("CYCLE");
        let range = builder.sym("RANGE");
        builder
            .nominal_supertype(TypeObject::ValueError)
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    EncodeKind::Cycle => cycle,
                    EncodeKind::Range => range,
                };
                Output::set(strand, out, kind);
                Ok(())
            })
            .get("path", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<yaml.EncodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}
