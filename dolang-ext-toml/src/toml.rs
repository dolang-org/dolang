use std::{
    collections::HashSet,
    fmt::{self, Formatter, Write as _},
    num::NonZero,
    result,
};

use serde::de::{self, DeserializeSeed, Visitor};
use toml_writer::TomlWrite;

use dolang::runtime::{
    Alloc, Output, Slot, Strand, Value,
    error::{Error, Result, ResultExt},
    unpack,
    value::{Empty, StrEmbryo, View},
    vm::Register,
};

const DATETIME_FIELD: &str = "$__toml_private_datetime";

/// [`fmt::Write`] adapter growing an embryo.
struct EmbryoWriter<'a, 'v> {
    embryo: &'a mut StrEmbryo<'v>,
    alloc: &'a mut dyn Alloc<'v>,
}

impl fmt::Write for EmbryoWriter<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.embryo.extend(self.alloc, s);
        Ok(())
    }
}

/// How a table entry is laid out in a document.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `key = value` in the table's body.
    Inline,
    /// A `[path.key]` section.
    Table,
    /// One `[[path.key]]` section per element.
    ArrayOfTables,
}

fn is_table(view: &View<'_, '_>) -> bool {
    matches!(view, View::Dict(_) | View::Record(_) | View::Struct(_))
}

/// Walks a value, writing TOML into an embryo.
///
/// Documents follow `toml::Serializer`'s layout: a table's inline entries
/// first, then its sub-tables and arrays of tables as sections, in order.
/// Lines are separated rather than terminated, so there is no trailing
/// newline.
struct Encoder<'a, 'v> {
    out: &'a mut StrEmbryo<'v>,
    /// Containers being written, for cycle detection.
    seen: HashSet<NonZero<usize>>,
    /// Encoded keys of the current section.
    path: Vec<String>,
    /// Whether anything has been written yet.
    started: bool,
}

/// Runs `f` with a writer into `embryo` that borrows `strand` only for this
/// call.
fn write<'v, 's>(
    embryo: &mut StrEmbryo<'v>,
    strand: &mut Strand<'v, 's>,
    f: impl FnOnce(&mut EmbryoWriter<'_, 'v>) -> fmt::Result,
) -> Result<'v, 's, ()> {
    let result = f(&mut EmbryoWriter {
        embryo,
        alloc: strand,
    });
    result.into_do(strand)
}

impl<'v> Encoder<'_, 'v> {
    fn emit<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&mut EmbryoWriter<'_, 'v>) -> fmt::Result,
    ) -> Result<'v, 's, ()> {
        write(self.out, strand, f)
    }

    /// Calls `f` with the name of a table key.
    fn with_key<'s, R>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
        f: impl FnOnce(&mut Self, &mut Strand<'v, 's>, &str) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        match key.view(strand.vm()) {
            View::Str(s) => f(self, strand, &s.pin()),
            View::Sym(sym) => {
                let name = sym.as_str(strand.vm());
                f(self, strand, name)
            }
            _ => Err(Error::type_error(
                strand,
                "TOML table keys must be Str or Sym values",
            )),
        }
    }

    /// Marks a container as being written, rejecting cycles.
    fn enter<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, NonZero<usize>> {
        let (id, kind) = match value.view(strand.vm()) {
            View::Array(handle) => (handle.id().addr(), "array"),
            View::Tuple(handle) => (handle.id().addr(), "tuple"),
            View::Dict(handle) => (handle.id().addr(), "dict"),
            View::Record(handle) => (handle.id().addr(), "record"),
            View::Struct(handle) => (handle.id().addr(), "struct"),
            _ => unreachable!("not a container"),
        };
        if self.seen.insert(id) {
            Ok(id)
        } else {
            Err(Error::value(strand, format!("cyclic reference to {kind}")))
        }
    }

    /// Calls `f` with each element of an array or tuple.
    fn each_elem<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        seq: &Value<'v>,
        mut f: impl FnMut(&mut Self, &mut Strand<'v, 's>, &Value<'v>) -> Result<'v, 's, ()>,
    ) -> Result<'v, 's, ()> {
        strand.with_slots_sync(|strand, [mut elem]| {
            match seq.view(strand.vm()) {
                View::Array(handle) => {
                    let len = handle.len(strand)?;
                    for i in 0..len {
                        if !handle.get(strand, i, &mut elem)? {
                            break;
                        }
                        f(self, strand, &elem)?;
                    }
                }
                View::Tuple(handle) => {
                    for i in 0..handle.len() {
                        if !handle.get(strand, i, &mut elem)? {
                            break;
                        }
                        f(self, strand, &elem)?;
                    }
                }
                _ => unreachable!("not a sequence"),
            }
            Ok(())
        })
    }

    /// Calls `f` with each key and value of a dict, record, or struct.
    fn each_pair<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        table: &Value<'v>,
        mut f: impl FnMut(&mut Self, &mut Strand<'v, 's>, &Value<'v>, &Value<'v>) -> Result<'v, 's, ()>,
    ) -> Result<'v, 's, ()> {
        strand.with_slots_sync(|strand, [mut key, mut val]| {
            match table.view(strand.vm()) {
                View::Dict(handle) => {
                    let mut pairs = handle.pairs();
                    while pairs.next(strand, &mut key, &mut val)? {
                        f(self, strand, &key, &val)?;
                    }
                }
                View::Record(handle) => {
                    let mut pairs = handle.pairs();
                    while pairs.next(strand, &mut key, &mut val)? {
                        f(self, strand, &key, &val)?;
                    }
                }
                View::Struct(handle) => {
                    let mut pairs = handle.pairs();
                    while pairs.next(strand, &mut key, &mut val)? {
                        f(self, strand, &key, &val)?;
                    }
                }
                _ => unreachable!("not a table"),
            }
            Ok(())
        })
    }

    /// Chooses how a table entry's value is laid out, as `toml::Serializer`
    /// does: tables become sections, and so does a non-empty array whose
    /// elements are all tables.
    fn layout<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, Layout> {
        let view = value.view(strand.vm());
        if is_table(&view) {
            return Ok(Layout::Table);
        }
        if !matches!(view, View::Array(_) | View::Tuple(_)) {
            return Ok(Layout::Inline);
        }
        let mut any = false;
        let mut all_tables = true;
        self.each_elem(strand, value, |_, strand, elem| {
            any = true;
            all_tables &= is_table(&elem.view(strand.vm()));
            Ok(())
        })?;
        Ok(if any && all_tables {
            Layout::ArrayOfTables
        } else {
            Layout::Inline
        })
    }

    /// Starts a new line.
    fn line<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        if std::mem::replace(&mut self.started, true) {
            self.emit(strand, |w| w.newline())?;
        }
        Ok(())
    }

    /// Writes the header for the current path, after a blank line.
    fn header<'s>(&mut self, strand: &mut Strand<'v, 's>, array: bool) -> Result<'v, 's, ()> {
        if std::mem::replace(&mut self.started, true) {
            self.emit(strand, |w| {
                w.newline()?;
                w.newline()
            })?;
        }
        let path = &self.path;
        write(self.out, strand, |w| {
            if array {
                w.open_array_of_tables_header()?;
            } else {
                w.open_table_header()?;
            }
            for (i, key) in path.iter().enumerate() {
                if i > 0 {
                    w.key_sep()?;
                }
                w.write_str(key)?;
            }
            if array {
                w.close_array_of_tables_header()
            } else {
                w.close_table_header()
            }
        })
    }

    /// Writes a value in inline form.
    fn inline<'s>(&mut self, strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, ()> {
        match value.view(strand.vm()) {
            View::Nil => Err(Error::type_error(
                strand,
                "nil values are not TOML-representable",
            )),
            View::Bool(b) => self.emit(strand, |w| w.value(b)),
            View::Int(i) => {
                let i = i64::try_from(i).map_err(|_| {
                    Error::value(strand, "TOML integers outside i64 range are not supported")
                })?;
                self.emit(strand, |w| w.value(i))
            }
            // Drop the sign of NaN, as `toml`'s serializer does.
            View::Float(x) if x.is_nan() => self.emit(strand, |w| w.value(x.copysign(1.0))),
            View::Float(x) => self.emit(strand, |w| w.value(x)),
            View::Str(s) => self.emit(strand, |w| w.value(&*s.pin())),
            View::Sym(sym) => {
                let name = sym.as_str(strand.vm());
                self.emit(strand, |w| w.value(name))
            }
            View::Bin(_) => Err(Error::type_error(
                strand,
                "binary values are not TOML-representable",
            )),
            View::Array(_) | View::Tuple(_) => {
                let id = self.enter(strand, value)?;
                self.emit(strand, |w| w.open_array())?;
                let mut first = true;
                self.each_elem(strand, value, |this, strand, elem| {
                    if !std::mem::take(&mut first) {
                        this.emit(strand, |w| {
                            w.val_sep()?;
                            w.space()
                        })?;
                    }
                    this.inline(strand, elem)
                })?;
                self.emit(strand, |w| w.close_array())?;
                self.seen.remove(&id);
                Ok(())
            }
            View::Dict(_) | View::Record(_) | View::Struct(_) => {
                let id = self.enter(strand, value)?;
                self.emit(strand, |w| w.open_inline_table())?;
                let mut first = true;
                self.each_pair(strand, value, |this, strand, key, val| {
                    if !std::mem::take(&mut first) {
                        this.emit(strand, |w| w.val_sep())?;
                    }
                    this.emit(strand, |w| w.space())?;
                    this.keyval(strand, key, val)
                })?;
                if !first {
                    self.emit(strand, |w| w.space())?;
                }
                self.emit(strand, |w| w.close_inline_table())?;
                self.seen.remove(&id);
                Ok(())
            }
            _ => Err(Error::type_error(strand, "unsupported TOML type")),
        }
    }

    /// Writes `key = value` with the value inline.
    fn keyval<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
        val: &Value<'v>,
    ) -> Result<'v, 's, ()> {
        self.with_key(strand, key, |this, strand, name| {
            this.emit(strand, |w| {
                w.key(name)?;
                w.space()?;
                w.keyval_sep()?;
                w.space()
            })
        })?;
        self.inline(strand, val)
    }

    /// Writes a table as a document section at the current path: inline
    /// entries first, then sub-tables and arrays of tables.
    ///
    /// The root has no header. Other headers follow `toml::Serializer`: an
    /// array element always gets `[[path]]`; a plain table gets `[path]` when
    /// it has inline entries or nothing else to imply it.
    fn table<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        table: &Value<'v>,
        array: bool,
    ) -> Result<'v, 's, ()> {
        let id = self.enter(strand, table)?;
        let root = self.path.is_empty();
        let mut headed = root;
        if array {
            self.header(strand, true)?;
            headed = true;
        }
        let mut sections = false;
        self.each_pair(strand, table, |this, strand, key, val| {
            if this.layout(strand, val)? != Layout::Inline {
                sections = true;
                return Ok(());
            }
            if !std::mem::replace(&mut headed, true) {
                this.header(strand, false)?;
            }
            this.line(strand)?;
            this.keyval(strand, key, val)
        })?;
        if !headed && !sections {
            self.header(strand, false)?;
        }
        if sections {
            self.each_pair(strand, table, |this, strand, key, val| {
                let layout = this.layout(strand, val)?;
                if layout == Layout::Inline {
                    return Ok(());
                }
                let mut encoded = String::new();
                this.with_key(strand, key, |_, _, name| {
                    let _ = encoded.key(name);
                    Ok(())
                })?;
                this.path.push(encoded);
                if layout == Layout::Table {
                    this.table(strand, val, false)?;
                } else {
                    this.each_elem(strand, val, |this, strand, elem| {
                        this.table(strand, elem, true)
                    })?;
                }
                this.path.pop();
                Ok(())
            })?;
        }
        self.seen.remove(&id);
        Ok(())
    }
}

struct Seed<'v, 'a, 'b>(&'a mut Strand<'v, 'b>, Slot<'v, 'a>);

struct MapKeySeed<'v, 'a, 'b, 't> {
    strand: &'a mut Strand<'v, 'b>,
    out: Slot<'v, 'a>,
    text: &'t mut Option<String>,
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

impl<'v, 'a, 'b, 'de> DeserializeSeed<'de> for MapKeySeed<'v, 'a, 'b, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> result::Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_str(self)
    }
}

impl<'v, 'a, 'b, 'de> Visitor<'de> for Seed<'v, 'a, 'b> {
    type Value = ();

    fn expecting(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "TOML-like values")
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
        let value = i64::try_from(v).map_err(|_| de::Error::custom("numeric overflow"))?;
        Output::set(self.0, &mut self.1, value);
        Ok(())
    }

    fn visit_u128<E>(mut self, v: u128) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        let value = i128::try_from(v).map_err(|_| de::Error::custom("numeric overflow"))?;
        Output::set(self.0, &mut self.1, value);
        Ok(())
    }

    fn visit_f64<E>(mut self, v: f64) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
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

    fn visit_borrowed_str<E>(self, v: &'de str) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(v)
    }

    fn visit_string<E>(self, v: String) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&v)
    }

    fn visit_none<E>(self) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(de::Error::custom("nil values are not TOML-representable"))
    }

    fn visit_unit<E>(self) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(de::Error::custom("nil values are not TOML-representable"))
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
        self.0
            .with_slots_sync(|strand, [mut key, mut value, mut saved_key]| {
                let mut key_text = None;
                let mut have_key = map
                    .next_key_seed(MapKeySeed {
                        strand,
                        out: Slot::reborrow(&mut key),
                        text: &mut key_text,
                    })?
                    .is_some();

                while have_key {
                    let current_key = key_text.take().unwrap();
                    map.next_value_seed(Seed(strand, Slot::reborrow(&mut value)))?;
                    Output::set(strand, &mut saved_key, &key);

                    if current_key == DATETIME_FIELD {
                        have_key = map
                            .next_key_seed(MapKeySeed {
                                strand,
                                out: Slot::reborrow(&mut key),
                                text: &mut key_text,
                            })?
                            .is_some();

                        if !have_key && looks_like_datetime(strand, &value) {
                            return Err(de::Error::custom("TOML datetimes are not supported"));
                        }

                        dict.insert(strand, &mut saved_key, &mut value, false)
                            .unwrap();
                    } else {
                        dict.insert(strand, &mut key, &mut value, false).unwrap();
                        have_key = map
                            .next_key_seed(MapKeySeed {
                                strand,
                                out: Slot::reborrow(&mut key),
                                text: &mut key_text,
                            })?
                            .is_some();
                    }
                }

                Ok(())
            })
    }
}

impl<'v, 'a, 'b, 'de> Visitor<'de> for MapKeySeed<'v, 'a, 'b, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "a TOML key")
    }

    fn visit_str<E>(mut self, v: &str) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.strand, &mut self.out, v);
        *self.text = Some(v.to_owned());
        Ok(())
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(v)
    }

    fn visit_string<E>(self, v: String) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&v)
    }
}

fn looks_like_datetime<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> bool {
    match value.view(strand.vm()) {
        View::Str(s) => {
            strand.access(|access| s.as_str(access).parse::<toml::value::Datetime>().is_ok())
        }
        _ => false,
    }
}

fn parse<'v, 's>(strand: &mut Strand<'v, 's>, src: &str, out: Slot<'v, '_>) -> Result<'v, 's, ()> {
    if let Ok(de) = toml::de::Deserializer::parse(src) {
        return Seed(strand, out)
            .deserialize(de)
            .map_err(|e| Error::value(strand, e.to_string()));
    }

    let de =
        toml::de::ValueDeserializer::parse(src).map_err(|e| Error::value(strand, e.to_string()))?;
    Seed(strand, out)
        .deserialize(de)
        .map_err(|e| Error::value(strand, e.to_string()))
}

pub(crate) fn configure<'v>(builder: &mut Register<'v>) {
    builder
        .module("toml")
        .function("encode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let mut buf = StrEmbryo::new();
            let mut encoder = Encoder {
                out: &mut buf,
                seen: HashSet::new(),
                path: Vec::new(),
                started: false,
            };
            if is_table(&arg.view(strand.vm())) {
                encoder.table(strand, &arg, false)?;
            } else {
                encoder.inline(strand, &arg)?;
            }
            buf.finish(strand, out);
            Ok(())
        })
        .function("decode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected Str"))?
                .pin();
            parse(strand, &src, out)
        })
        .commit();
}
