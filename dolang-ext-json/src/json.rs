use std::{
    collections::HashSet,
    fmt::{self, Formatter},
    io,
    num::NonZero,
    result,
};

use serde::de::{self, DeserializeSeed, Visitor};
use serde_json::{
    error::Category,
    ser::{CharEscape, CompactFormatter, Formatter as JsonFormatter, PrettyFormatter},
};

use dolang::runtime::AllocExt;
use dolang::runtime::{
    Alloc, Input, Instance, Object, Output, Slot, State, Strand, Value,
    error::{Error, Result},
    io::IoResultExt as _,
    object::{Mut, Ref, TypeBuilder, fmt},
    unpack,
    value::{BinEmbryo, Nil, TypeObject, View, fmt::Format},
    vm::Register,
    with_instance, with_slots,
};

use crate::global::{Global, Tag};

/// `json.DecodeError`: the document is not valid JSON.
pub(crate) struct DecodeError;

/// What is wrong with a document.
#[derive(Clone, Copy)]
enum DecodeKind {
    /// Malformed or trailing text.
    Syntax,
    /// The document ends inside a value.
    Eof,
}

pub(crate) struct DecodeAnnex {
    message: String,
    kind: DecodeKind,
    line: usize,
    column: usize,
}

/// `json.EncodeError`: the value has a JSON form in general, but not as given.
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
    /// A `Float` key is not finite.
    Key,
}

pub(crate) struct EncodeAnnex {
    message: String,
    kind: EncodeKind,
}

/// Failure while encoding.
enum Fault<'v, 's> {
    /// Raised as is.
    Do(Error<'v, 's>),
    /// Raised as `json.EncodeError`, with the path [`Encoder::within`]
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

type EncodeResult<'v, 's, T = ()> = result::Result<T, Fault<'v, 's>>;

/// Destination for encoded bytes.
trait Sink<'v> {
    /// Runs `f` with a writer that borrows `strand` only for this call.
    fn write<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&mut dyn io::Write) -> io::Result<()>,
    ) -> Result<'v, 's, ()>;
}

/// [`io::Write`] adapter growing an embryo.
struct EmbryoWriter<'a, 'v> {
    embryo: &'a mut BinEmbryo<'v>,
    alloc: &'a mut dyn Alloc<'v>,
}

impl io::Write for EmbryoWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.embryo.extend(self.alloc, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'v> Sink<'v> for BinEmbryo<'v> {
    fn write<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&mut dyn io::Write) -> io::Result<()>,
    ) -> Result<'v, 's, ()> {
        let result = f(&mut EmbryoWriter {
            embryo: self,
            alloc: strand,
        });
        result.into_do_io(strand)
    }
}

/// Writes the contents of a JSON string, escaping as `serde_json` does.
///
/// Runs are split only at the ASCII bytes being escaped, so each fragment is
/// a whole-character slice of `value`.
fn write_str_contents(
    formatter: &mut impl JsonFormatter,
    writer: &mut dyn io::Write,
    value: &str,
) -> io::Result<()> {
    let bytes = value.as_bytes();
    let mut start = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        let escape = match byte {
            b'"' => CharEscape::Quote,
            b'\\' => CharEscape::ReverseSolidus,
            0x08 => CharEscape::Backspace,
            b'\t' => CharEscape::Tab,
            b'\n' => CharEscape::LineFeed,
            0x0c => CharEscape::FormFeed,
            b'\r' => CharEscape::CarriageReturn,
            0x00..0x20 => CharEscape::AsciiControl(byte),
            _ => continue,
        };
        if start < i {
            formatter.write_string_fragment(writer, &value[start..i])?;
        }
        formatter.write_char_escape(writer, escape)?;
        start = i + 1;
    }
    if start < bytes.len() {
        formatter.write_string_fragment(writer, &value[start..])?;
    }
    Ok(())
}

/// Walks a value, driving `formatter` into `sink`.
struct Encoder<'a, 'v, 'p, F, S> {
    formatter: F,
    sink: &'a mut S,
    /// Containers being written, for cycle detection.
    seen: HashSet<NonZero<usize>>,
    /// Nil, or an `Array` of the steps from the fault outward.
    path: Slot<'v, 'p>,
}

impl<'v, F: JsonFormatter, S: Sink<'v>> Encoder<'_, 'v, '_, F, S> {
    fn emit<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&mut F, &mut dyn io::Write) -> io::Result<()>,
    ) -> Result<'v, 's, ()> {
        let formatter = &mut self.formatter;
        self.sink.write(strand, |writer| f(formatter, writer))
    }

    fn string<'s>(&mut self, strand: &mut Strand<'v, 's>, value: &str) -> Result<'v, 's, ()> {
        self.emit(strand, |f, w| {
            f.begin_string(w)?;
            write_str_contents(f, w, value)?;
            f.end_string(w)
        })
    }

    fn int<'s>(value: i128) -> EncodeResult<'v, 's, i64> {
        i64::try_from(value).map_err(|_| {
            Fault::encode(
                EncodeKind::Range,
                "JSON integers outside i64 range are not supported",
            )
        })
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

    fn enter<'s>(&mut self, id: NonZero<usize>, kind: &str) -> EncodeResult<'v, 's> {
        if self.seen.insert(id) {
            Ok(())
        } else {
            Err(Fault::encode(
                EncodeKind::Cycle,
                format!("cyclic reference to {kind}"),
            ))
        }
    }

    fn value<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> EncodeResult<'v, 's> {
        match value.view(strand.vm()) {
            View::Nil => self.emit(strand, |f, w| f.write_null(w))?,
            View::Bool(b) => self.emit(strand, |f, w| f.write_bool(w, b))?,
            View::Int(i) => {
                let i = Self::int(i)?;
                self.emit(strand, |f, w| f.write_i64(w, i))?
            }
            // `serde_json` writes non-finite floats as `null`.
            View::Float(x) if !x.is_finite() => self.emit(strand, |f, w| f.write_null(w))?,
            View::Float(x) => self.emit(strand, |f, w| f.write_f64(w, x))?,
            View::Str(s) => self.string(strand, &s.pin())?,
            View::Sym(sym) => {
                let name = sym.as_str(strand.vm());
                self.string(strand, name)?
            }
            View::Bin(_) => {
                return Err(
                    Error::type_error(strand, "binary values are not JSON-representable").into(),
                );
            }
            View::Array(handle) => {
                let len = handle.len(strand)?;
                self.array(
                    strand,
                    handle.id().addr(),
                    "array",
                    len,
                    |strand, i, elem| handle.get(strand, i, elem),
                )?
            }
            View::Tuple(handle) => self.array(
                strand,
                handle.id().addr(),
                "tuple",
                handle.len(),
                |strand, i, elem| handle.get(strand, i, elem),
            )?,
            View::Dict(handle) => {
                let mut pairs = handle.pairs();
                self.object(strand, handle.id().addr(), "dict", |strand, key, val| {
                    pairs.next(strand, key, val)
                })?
            }
            View::Struct(handle) => {
                let mut pairs = handle.pairs();
                self.object(strand, handle.id().addr(), "struct", |strand, key, val| {
                    pairs.next(strand, key, val)
                })?
            }
            View::Record(_) => {
                return Err(Error::type_error(strand, "`Record` has no JSON encoding").into());
            }
            _ => return Err(Error::type_error(strand, "unsupported JSON type").into()),
        }
        Ok(())
    }

    /// Writes an object key, quoting scalars as `serde_json` does.
    fn key<'s>(&mut self, strand: &mut Strand<'v, 's>, key: &Value<'v>) -> EncodeResult<'v, 's> {
        match key.view(strand.vm()) {
            View::Str(s) => self.string(strand, &s.pin())?,
            View::Sym(sym) => {
                let name = sym.as_str(strand.vm());
                self.string(strand, name)?
            }
            View::Int(i) => {
                let i = Self::int(i)?;
                self.emit(strand, |f, w| {
                    f.begin_string(w)?;
                    f.write_i64(w, i)?;
                    f.end_string(w)
                })?
            }
            View::Bool(b) => self.emit(strand, |f, w| {
                f.begin_string(w)?;
                f.write_bool(w, b)?;
                f.end_string(w)
            })?,
            View::Float(x) if !x.is_finite() => {
                return Err(Fault::encode(
                    EncodeKind::Key,
                    "float key must be finite (got NaN or +/-inf)",
                ));
            }
            View::Float(x) => self.emit(strand, |f, w| {
                f.begin_string(w)?;
                f.write_f64(w, x)?;
                f.end_string(w)
            })?,
            View::Bin(_) => {
                return Err(
                    Error::type_error(strand, "binary values are not JSON-representable").into(),
                );
            }
            _ => return Err(Error::type_error(strand, "key must be a string").into()),
        }
        Ok(())
    }

    fn array<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        id: NonZero<usize>,
        kind: &str,
        len: usize,
        mut get: impl FnMut(&mut Strand<'v, 's>, usize, &mut Slot<'v, '_>) -> Result<'v, 's, bool>,
    ) -> EncodeResult<'v, 's> {
        self.enter(id, kind)?;
        self.emit(strand, |f, w| f.begin_array(w))?;
        with_slots!(strand, |mut elem| {
            for i in 0..len {
                if !get(strand, i, &mut elem)? {
                    break;
                }
                self.emit(strand, |f, w| f.begin_array_value(w, i == 0))?;
                self.value(strand, &elem)
                    .map_err(|fault| self.within(strand, fault, i))?;
                self.emit(strand, |f, w| f.end_array_value(w))?;
            }
            EncodeResult::Ok(())
        })?;
        self.emit(strand, |f, w| f.end_array(w))?;
        self.seen.remove(&id);
        Ok(())
    }

    fn object<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        id: NonZero<usize>,
        kind: &str,
        mut next: impl FnMut(
            &mut Strand<'v, 's>,
            &mut Slot<'v, '_>,
            &mut Slot<'v, '_>,
        ) -> Result<'v, 's, bool>,
    ) -> EncodeResult<'v, 's> {
        self.enter(id, kind)?;
        self.emit(strand, |f, w| f.begin_object(w))?;
        with_slots!(strand, |mut key, mut val| {
            let mut first = true;
            while next(strand, &mut key, &mut val)? {
                self.emit(strand, |f, w| f.begin_object_key(w, first))?;
                first = false;
                self.key(strand, &key)?;
                self.emit(strand, |f, w| {
                    f.end_object_key(w)?;
                    f.begin_object_value(w)
                })?;
                self.value(strand, &val)
                    .map_err(|fault| self.within(strand, fault, &*key))?;
                self.emit(strand, |f, w| f.end_object_value(w))?;
            }
            EncodeResult::Ok(())
        })?;
        self.emit(strand, |f, w| f.end_object(w))?;
        self.seen.remove(&id);
        Ok(())
    }
}

/// Encodes `value` into `sink` with `formatter`.
fn encode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    formatter: impl JsonFormatter,
    sink: &mut impl Sink<'v>,
) -> Result<'v, 's, ()> {
    with_slots!(strand, |path, mut tuple, mut error| {
        let mut encoder = Encoder {
            formatter,
            sink,
            seen: HashSet::new(),
            path,
        };
        let annex = match encoder.value(strand, value) {
            Ok(()) => return Ok(()),
            Err(Fault::Do(error)) => return Err(error),
            Err(Fault::Encode(annex)) => annex,
        };
        match encoder.path.as_array(strand.vm()) {
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
        Err(Error::from_value(strand, &error))
    })
}

/// Builds a decoded value in its slot.
///
/// A Do error is set aside in the stash, since a serde error can't carry it,
/// and a marker error stops deserialization.
struct Seed<'v, 'a, 'b>(
    &'a mut Strand<'v, 'b>,
    Slot<'v, 'a>,
    &'a mut Option<Error<'v, 'b>>,
);

impl<'v, 'a, 'b> Seed<'v, 'a, 'b> {
    fn stash<E: de::Error>(stash: &mut Option<Error<'v, 'b>>, error: Error<'v, 'b>) -> E {
        *stash = Some(error);
        E::custom("")
    }
}

impl<'v, 'a, 'b, 'de> DeserializeSeed<'de> for Seed<'v, 'a, 'b> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> result::Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'v, 'a, 'b, 'de> Visitor<'de> for Seed<'v, 'a, 'b> {
    type Value = ();

    fn expecting(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "JSON-like values")
    }

    fn visit_i64<E>(mut self, v: i64) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, i128::from(v));
        Ok(())
    }

    fn visit_i128<E>(mut self, v: i128) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_u64<E>(mut self, v: u64) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, i128::from(v));
        Ok(())
    }

    fn visit_f64<E>(mut self, v: f64) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_unit<E>(mut self) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, Nil);
        Ok(())
    }

    fn visit_bool<E>(mut self, v: bool) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_str<E>(mut self, v: &str) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_seq<A>(self, mut seq: A) -> result::Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let Seed(strand, mut out, stash) = self;
        strand.new_array(&mut out);
        let array = out.as_array(strand).unwrap();
        with_slots!(strand, |mut elem| {
            while let Some(()) =
                seq.next_element_seed(Seed(strand, Slot::reborrow(&mut elem), &mut *stash))?
            {
                array
                    .push(strand, &mut elem)
                    .map_err(|error| Self::stash(stash, error))?;
            }
            Ok(())
        })
    }

    fn visit_map<A>(self, mut map: A) -> result::Result<Self::Value, A::Error>
    where
        A: de::MapAccess<'de>,
    {
        let Seed(strand, mut out, stash) = self;
        strand.new_dict(&mut out);
        let dict = out.as_dict(strand).unwrap();
        with_slots!(strand, |mut key, mut value| {
            while let Some(()) =
                map.next_key_seed(Seed(strand, Slot::reborrow(&mut key), &mut *stash))?
            {
                map.next_value_seed(Seed(strand, Slot::reborrow(&mut value), &mut *stash))?;
                dict.insert(strand, &mut key, &mut value, false)
                    .map_err(|error| Self::stash(stash, error))?;
            }
            Ok(())
        })
    }
}

/// Raises `json.DecodeError` for a serde error.
fn decode_error<'v, 's>(strand: &mut Strand<'v, 's>, error: serde_json::Error) -> Error<'v, 's> {
    let kind = match error.classify() {
        Category::Eof => DecodeKind::Eof,
        Category::Syntax => DecodeKind::Syntax,
        // The input is a `Str`, and the visitor accepts every JSON type.
        Category::Io | Category::Data => {
            debug_assert!(false, "unexpected JSON error: {error}");
            DecodeKind::Syntax
        }
    };
    let global = strand.state::<Tag>();
    Error::object_with_annex(
        strand,
        global.types.decode_error,
        DecodeError,
        DecodeAnnex {
            message: error.to_string(),
            kind,
            line: error.line(),
            column: error.column(),
        },
    )
}

impl<'v> Object<'v> for DecodeError {
    const NAME: &'v str = "DecodeError";
    const MODULE: &'v str = "json";
    type Annex = DecodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let syntax = builder.sym("SYNTAX");
        let eof = builder.sym("EOF");
        builder
            .nominal_supertype(TypeObject::DataError)
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    DecodeKind::Syntax => syntax,
                    DecodeKind::Eof => eof,
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
        fmt!(strand, w, "<json.DecodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> Object<'v> for EncodeError {
    const NAME: &'v str = "EncodeError";
    const MODULE: &'v str = "json";
    /// Slot 0 is the path tuple.
    const SLOTS: usize = 1;
    type Annex = EncodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let cycle = builder.sym("CYCLE");
        let range = builder.sym("RANGE");
        let key = builder.sym("KEY");
        builder
            .nominal_supertype(TypeObject::ValueError)
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    EncodeKind::Cycle => cycle,
                    EncodeKind::Range => range,
                    EncodeKind::Key => key,
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
        fmt!(strand, w, "<json.EncodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

pub(crate) fn configure<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    let indent = builder.sym("indent");

    builder
        .module("json")
        .value("DecodeError", global.types.decode_error)
        .value("EncodeError", global.types.encode_error)
        .function("encode", async move |strand, args, out| {
            unpack!(strand, args => arg, ?:indent);
            let indent = match indent {
                Some(indent) if !indent.is_nil() => Some(
                    indent
                        .as_int(strand)
                        .and_then(|indent| usize::try_from(indent).ok())
                        .ok_or_else(|| {
                            Error::value(strand, "indent: expected a non-negative Int")
                        })?,
                ),
                _ => None,
            };
            let mut buf = BinEmbryo::new();
            match indent {
                None => encode(strand, &arg, CompactFormatter, &mut buf)?,
                Some(indent) => {
                    let spaces = vec![b' '; indent];
                    let formatter = PrettyFormatter::with_indent(&spaces);
                    encode(strand, &arg, formatter, &mut buf)?;
                }
            }
            // SAFETY: the formatters write only ASCII apart from string
            // fragments, which are slices of `Str` or `Sym` contents split only
            // at the ASCII bytes being escaped.
            unsafe { buf.finish_str_unchecked(strand, out) };
            Ok(())
        })
        .function("decode", async move |strand, args, out| {
            unpack!(strand, args => arg);
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected Str"))?
                .pin();
            let mut dejson = serde_json::Deserializer::from_str(&src);
            let mut stash = None;
            let result = Seed(strand, out, &mut stash)
                .deserialize(&mut dejson)
                .and_then(|()| dejson.end());
            if let Some(error) = stash {
                return Err(error);
            }
            result.map_err(|error| decode_error(strand, error))
        })
        .commit();
}
