use std::{
    collections::HashSet,
    fmt::{self, Formatter},
    io,
    num::NonZero,
    result,
};

use serde::de::{self, DeserializeSeed, Visitor};
use serde_json::ser::{CharEscape, CompactFormatter, Formatter as JsonFormatter, PrettyFormatter};

use dolang::runtime::{
    Alloc, Output, Slot, Strand, Value,
    error::{Error, Result, ResultExt},
    unpack,
    value::{BinEmbryo, Empty, Nil, View},
    vm::Register,
};

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
        result.into_do(strand)
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
struct Encoder<'a, F, S> {
    formatter: F,
    sink: &'a mut S,
    /// Containers being written, for cycle detection.
    seen: HashSet<NonZero<usize>>,
}

impl<F: JsonFormatter, S> Encoder<'_, F, S> {
    fn emit<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&mut F, &mut dyn io::Write) -> io::Result<()>,
    ) -> Result<'v, 's, ()>
    where
        S: Sink<'v>,
    {
        let formatter = &mut self.formatter;
        self.sink.write(strand, |writer| f(formatter, writer))
    }

    fn string<'v, 's>(&mut self, strand: &mut Strand<'v, 's>, value: &str) -> Result<'v, 's, ()>
    where
        S: Sink<'v>,
    {
        self.emit(strand, |f, w| {
            f.begin_string(w)?;
            write_str_contents(f, w, value)?;
            f.end_string(w)
        })
    }

    fn int<'v, 's>(strand: &mut Strand<'v, 's>, value: i128) -> Result<'v, 's, i64> {
        i64::try_from(value)
            .map_err(|_| Error::value(strand, "JSON integers outside i64 range are not supported"))
    }

    fn enter<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        id: NonZero<usize>,
        kind: &str,
    ) -> Result<'v, 's, ()> {
        if self.seen.insert(id) {
            Ok(())
        } else {
            Err(Error::value(strand, format!("cyclic reference to {kind}")))
        }
    }

    fn value<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, ()>
    where
        S: Sink<'v>,
    {
        match value.view(strand.vm()) {
            View::Nil => self.emit(strand, |f, w| f.write_null(w)),
            View::Bool(b) => self.emit(strand, |f, w| f.write_bool(w, b)),
            View::Int(i) => {
                let i = Self::int(strand, i)?;
                self.emit(strand, |f, w| f.write_i64(w, i))
            }
            // `serde_json` writes non-finite floats as `null`.
            View::Float(x) if !x.is_finite() => self.emit(strand, |f, w| f.write_null(w)),
            View::Float(x) => self.emit(strand, |f, w| f.write_f64(w, x)),
            View::Str(s) => self.string(strand, &s.pin()),
            View::Sym(sym) => {
                let name = sym.as_str(strand.vm());
                self.string(strand, name)
            }
            View::Bin(_) => Err(Error::type_error(
                strand,
                "binary values are not JSON-representable",
            )),
            View::Array(handle) => {
                let len = handle.len(strand)?;
                self.array(
                    strand,
                    handle.id().addr(),
                    "array",
                    len,
                    |strand, i, elem| handle.get(strand, i, elem),
                )
            }
            View::Tuple(handle) => self.array(
                strand,
                handle.id().addr(),
                "tuple",
                handle.len(),
                |strand, i, elem| handle.get(strand, i, elem),
            ),
            View::Dict(handle) => {
                let mut pairs = handle.pairs();
                self.object(strand, handle.id().addr(), "dict", |strand, key, val| {
                    pairs.next(strand, key, val)
                })
            }
            View::Record(handle) => {
                let mut pairs = handle.pairs();
                self.object(strand, handle.id().addr(), "record", |strand, key, val| {
                    pairs.next(strand, key, val)
                })
            }
            View::Struct(handle) => {
                let mut pairs = handle.pairs();
                self.object(strand, handle.id().addr(), "struct", |strand, key, val| {
                    pairs.next(strand, key, val)
                })
            }
            _ => Err(Error::type_error(strand, "unsupported JSON type")),
        }
    }

    /// Writes an object key, quoting scalars as `serde_json` does.
    fn key<'v, 's>(&mut self, strand: &mut Strand<'v, 's>, key: &Value<'v>) -> Result<'v, 's, ()>
    where
        S: Sink<'v>,
    {
        match key.view(strand.vm()) {
            View::Str(s) => self.string(strand, &s.pin()),
            View::Sym(sym) => {
                let name = sym.as_str(strand.vm());
                self.string(strand, name)
            }
            View::Int(i) => {
                let i = Self::int(strand, i)?;
                self.emit(strand, |f, w| {
                    f.begin_string(w)?;
                    f.write_i64(w, i)?;
                    f.end_string(w)
                })
            }
            View::Bool(b) => self.emit(strand, |f, w| {
                f.begin_string(w)?;
                f.write_bool(w, b)?;
                f.end_string(w)
            }),
            View::Float(x) if !x.is_finite() => Err(Error::value(
                strand,
                "float key must be finite (got NaN or +/-inf)",
            )),
            View::Float(x) => self.emit(strand, |f, w| {
                f.begin_string(w)?;
                f.write_f64(w, x)?;
                f.end_string(w)
            }),
            View::Bin(_) => Err(Error::type_error(
                strand,
                "binary values are not JSON-representable",
            )),
            _ => Err(Error::type_error(strand, "key must be a string")),
        }
    }

    fn array<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        id: NonZero<usize>,
        kind: &str,
        len: usize,
        mut get: impl FnMut(&mut Strand<'v, 's>, usize, &mut Slot<'v, '_>) -> Result<'v, 's, bool>,
    ) -> Result<'v, 's, ()>
    where
        S: Sink<'v>,
    {
        self.enter(strand, id, kind)?;
        self.emit(strand, |f, w| f.begin_array(w))?;
        strand.with_slots_sync(|strand, [mut elem]| {
            for i in 0..len {
                if !get(strand, i, &mut elem)? {
                    break;
                }
                self.emit(strand, |f, w| f.begin_array_value(w, i == 0))?;
                self.value(strand, &elem)?;
                self.emit(strand, |f, w| f.end_array_value(w))?;
            }
            Ok(())
        })?;
        self.emit(strand, |f, w| f.end_array(w))?;
        self.seen.remove(&id);
        Ok(())
    }

    fn object<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        id: NonZero<usize>,
        kind: &str,
        mut next: impl FnMut(
            &mut Strand<'v, 's>,
            &mut Slot<'v, '_>,
            &mut Slot<'v, '_>,
        ) -> Result<'v, 's, bool>,
    ) -> Result<'v, 's, ()>
    where
        S: Sink<'v>,
    {
        self.enter(strand, id, kind)?;
        self.emit(strand, |f, w| f.begin_object(w))?;
        strand.with_slots_sync(|strand, [mut key, mut val]| {
            let mut first = true;
            while next(strand, &mut key, &mut val)? {
                self.emit(strand, |f, w| f.begin_object_key(w, first))?;
                first = false;
                self.key(strand, &key)?;
                self.emit(strand, |f, w| {
                    f.end_object_key(w)?;
                    f.begin_object_value(w)
                })?;
                self.value(strand, &val)?;
                self.emit(strand, |f, w| f.end_object_value(w))?;
            }
            Ok(())
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
    Encoder {
        formatter,
        sink,
        seen: HashSet::new(),
    }
    .value(strand, value)
}

struct Seed<'v, 'a, 'b>(&'a mut Strand<'v, 'b>, Slot<'v, 'a>);

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

    fn visit_seq<A>(mut self, mut seq: A) -> result::Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        Output::set(self.0, &mut self.1, Empty::Array);
        let array = self.1.as_array(self.0).unwrap();
        self.0.with_slots_sync(|strand, [mut elem]| {
            while let Some(()) = seq.next_element_seed(Seed(strand, Slot::reborrow(&mut elem)))? {
                array.push(strand, &mut elem).unwrap();
            }
            Ok(())
        })
    }

    fn visit_map<A>(mut self, mut map: A) -> result::Result<Self::Value, A::Error>
    where
        A: de::MapAccess<'de>,
    {
        Output::set(self.0, &mut self.1, Empty::Dict);
        let dict = self.1.as_dict(self.0).unwrap();
        self.0.with_slots_sync(|strand, [mut key, mut value]| {
            while let Some(()) = map.next_key_seed(Seed(strand, Slot::reborrow(&mut key)))? {
                map.next_value_seed(Seed(strand, Slot::reborrow(&mut value)))?;
                dict.insert(strand, &mut key, &mut value, false).unwrap();
            }
            Ok(())
        })
    }
}

pub(crate) fn configure<'v>(builder: &mut Register<'v>) {
    let indent_sym = builder.sym("indent");

    builder
        .module("json")
        .function("encode", async move |strand, args, out| {
            let ([arg], [indent]) = unpack!(strand, args, 1, 0, indent_sym = None)?;
            let indent = match indent {
                Some(indent) if !indent.is_nil() => Some(
                    indent
                        .as_int(strand)
                        .and_then(|indent| usize::try_from(indent).ok())
                        .ok_or_else(|| {
                            Error::type_error(strand, "indent: expected a non-negative Int")
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
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected Str"))?
                .pin();
            let mut dejson = serde_json::Deserializer::from_str(&src);
            Seed(strand, out)
                .deserialize(&mut dejson)
                .into_do_data(strand)
        })
        .commit();
}
