use std::{
    collections::HashSet,
    fmt::{self, Formatter},
    num::NonZero,
    result,
};

use serde::de::{self, DeserializeSeed, Visitor};
use toml_writer::TomlWrite;

use dolang::runtime::{
    Alloc, Input, Instance, Object, Output, Slot, State, Strand, Value,
    error::{Error, Result},
    object::{Mut, Ref, TypeBuilder, fmt},
    unpack,
    value::{AsTuple, Empty, Nil, StrEmbryo, TypeObject, View, fmt::Format},
    vm::Register,
};

use crate::global::{Global, Tag};

const DATETIME_FIELD: &str = "$__toml_private_datetime";

/// The `toml` crate's messages for numbers it can't represent.
const INT_OVERFLOW: &str = "integer number overflowed";
const FLOAT_OVERFLOW: &str = "floating-point number overflowed";

/// `toml.DecodeError`: the input is not valid TOML, or uses a feature
/// `toml.decode` rejects.
pub(crate) struct DecodeError;

/// What is wrong with the input.
#[derive(Clone, Copy)]
enum DecodeKind {
    /// The text is not valid TOML.
    Syntax,
    /// A datetime, date, or time value.
    Unsupported,
    /// A number is too large for an `Int` or `Float`.
    Range,
}

pub(crate) struct DecodeAnnex {
    message: String,
    kind: DecodeKind,
    line: usize,
    /// 1-based, in characters.
    column: usize,
}

/// `toml.EncodeError`: the value has a TOML form in general, but not as given.
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
    /// Two keys of one table have the same name.
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
    /// Raised as `toml.EncodeError`, with the path [`Encoder::within`]
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
    matches!(view, View::Dict(_) | View::Struct(_))
}

/// Walks a value, writing TOML into an embryo.
///
/// Documents follow `toml::Serializer`'s layout: a table's inline entries
/// first, then its sub-tables and arrays of tables as sections, in order.
/// Lines are separated rather than terminated, so there is no trailing
/// newline.
struct Encoder<'a, 'v, 'p> {
    out: &'a mut StrEmbryo<'v>,
    /// Containers being written, for cycle detection.
    seen: HashSet<NonZero<usize>>,
    /// Encoded keys of the current section.
    section: Vec<String>,
    /// Whether anything has been written yet.
    started: bool,
    /// Nil, or an `Array` of the steps from the fault outward.
    path: Slot<'v, 'p>,
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
    result.expect("writing to a StrEmbryo cannot fail");
    Ok(())
}

impl<'v> Encoder<'_, 'v, '_> {
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

    /// Records that `fault` lies under `segment`, an index or a key.
    fn within<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        fault: Fault<'v, 's>,
        segment: impl Input<'v>,
    ) -> Fault<'v, 's> {
        if let Fault::Encode(_) = fault {
            if self.path.is_nil() {
                Output::set(strand, &mut self.path, Empty::Array);
            }
            let path = self.path.as_array(strand.vm()).expect("path is an Array");
            path.push(strand, segment).expect("path is not shared");
        }
        fault
    }

    /// Marks a container as being written, rejecting cycles.
    fn enter<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> EncodeResult<'v, 's, NonZero<usize>> {
        let (id, kind) = match value.view(strand.vm()) {
            View::Array(handle) => (handle.id().addr(), "array"),
            View::Tuple(handle) => (handle.id().addr(), "tuple"),
            View::Dict(handle) => (handle.id().addr(), "dict"),
            View::Struct(handle) => (handle.id().addr(), "struct"),
            _ => unreachable!("not a container"),
        };
        if self.seen.insert(id) {
            Ok(id)
        } else {
            Err(Fault::encode(
                EncodeKind::Cycle,
                format!("cyclic reference to {kind}"),
            ))
        }
    }

    /// Calls `f` with each element of an array or tuple, placing its faults
    /// under the element's index.
    fn each_elem<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        seq: &Value<'v>,
        mut f: impl FnMut(&mut Self, &mut Strand<'v, 's>, &Value<'v>) -> EncodeResult<'v, 's, ()>,
    ) -> EncodeResult<'v, 's, ()> {
        strand.with_slots_sync(|strand, [mut elem]| {
            match seq.view(strand.vm()) {
                View::Array(handle) => {
                    let len = handle.len(strand)?;
                    for i in 0..len {
                        if !handle.get(strand, i, &mut elem)? {
                            break;
                        }
                        f(self, strand, &elem).map_err(|fault| self.within(strand, fault, i))?;
                    }
                }
                View::Tuple(handle) => {
                    for i in 0..handle.len() {
                        if !handle.get(strand, i, &mut elem)? {
                            break;
                        }
                        f(self, strand, &elem).map_err(|fault| self.within(strand, fault, i))?;
                    }
                }
                _ => unreachable!("not a sequence"),
            }
            EncodeResult::Ok(())
        })
    }

    /// Calls `f` with each key and value of a dict or struct,
    /// placing its faults under the key.
    fn each_pair<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        table: &Value<'v>,
        mut f: impl FnMut(
            &mut Self,
            &mut Strand<'v, 's>,
            &Value<'v>,
            &Value<'v>,
        ) -> EncodeResult<'v, 's, ()>,
    ) -> EncodeResult<'v, 's, ()> {
        strand.with_slots_sync(|strand, [mut key, mut val]| {
            match table.view(strand.vm()) {
                View::Dict(handle) => {
                    let mut pairs = handle.pairs();
                    while pairs.next(strand, &mut key, &mut val)? {
                        f(self, strand, &key, &val)
                            .map_err(|fault| self.within(strand, fault, &*key))?;
                    }
                }
                View::Struct(handle) => {
                    let mut pairs = handle.pairs();
                    while pairs.next(strand, &mut key, &mut val)? {
                        f(self, strand, &key, &val)
                            .map_err(|fault| self.within(strand, fault, &*key))?;
                    }
                }
                _ => unreachable!("not a table"),
            }
            EncodeResult::Ok(())
        })
    }

    /// Rejects a table with two keys of the same name.
    fn check_keys<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        table: &Value<'v>,
    ) -> EncodeResult<'v, 's, ()> {
        let mut names = HashSet::new();
        let mut duplicate = None;
        self.each_pair(strand, table, |this, strand, key, _| {
            this.with_key(strand, key, |_, _, name| {
                if !names.insert(name.to_owned()) && duplicate.is_none() {
                    duplicate = Some(name.to_owned());
                }
                Ok(())
            })?;
            Ok(())
        })?;
        match duplicate {
            Some(name) => Err(Fault::encode(
                EncodeKind::Key,
                format!("duplicate TOML key {name:?}"),
            )),
            None => Ok(()),
        }
    }

    /// Chooses how a table entry's value is laid out, as `toml::Serializer`
    /// does: tables become sections, and so does a non-empty array whose
    /// elements are all tables.
    fn layout<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> EncodeResult<'v, 's, Layout> {
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
        let path = &self.section;
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
                fmt::Write::write_str(w, key)?;
            }
            if array {
                w.close_array_of_tables_header()
            } else {
                w.close_table_header()
            }
        })
    }

    /// Writes a value in inline form.
    fn inline<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> EncodeResult<'v, 's, ()> {
        match value.view(strand.vm()) {
            View::Nil => {
                Err(Error::type_error(strand, "nil values are not TOML-representable").into())
            }
            View::Bool(b) => Ok(self.emit(strand, |w| w.value(b))?),
            View::Int(i) => {
                let i = i64::try_from(i).map_err(|_| {
                    Fault::encode(
                        EncodeKind::Range,
                        "TOML integers outside i64 range are not supported",
                    )
                })?;
                Ok(self.emit(strand, |w| w.value(i))?)
            }
            // Drop the sign of NaN, as `toml`'s serializer does.
            View::Float(x) if x.is_nan() => Ok(self.emit(strand, |w| w.value(x.copysign(1.0)))?),
            View::Float(x) => Ok(self.emit(strand, |w| w.value(x))?),
            View::Str(s) => Ok(self.emit(strand, |w| w.value(&*s.pin()))?),
            View::Sym(sym) => {
                let name = sym.as_str(strand.vm());
                Ok(self.emit(strand, |w| w.value(name))?)
            }
            View::Bin(_) => {
                Err(Error::type_error(strand, "binary values are not TOML-representable").into())
            }
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
            View::Dict(_) | View::Struct(_) => {
                let id = self.enter(strand, value)?;
                self.check_keys(strand, value)?;
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
            _ => Err(Error::type_error(strand, "unsupported TOML type").into()),
        }
    }

    /// Writes `key = value` with the value inline.
    fn keyval<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
        val: &Value<'v>,
    ) -> EncodeResult<'v, 's, ()> {
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
    ) -> EncodeResult<'v, 's, ()> {
        let id = self.enter(strand, table)?;
        self.check_keys(strand, table)?;
        let root = self.section.is_empty();
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
                this.section.push(encoded);
                if layout == Layout::Table {
                    this.table(strand, val, false)?;
                } else {
                    this.each_elem(strand, val, |this, strand, elem| {
                        this.table(strand, elem, true)
                    })?;
                }
                this.section.pop();
                Ok(())
            })?;
        }
        self.seen.remove(&id);
        Ok(())
    }
}

/// Builds a decoded value in its slot.
///
/// A visitor error notes its kind in the cell, since a serde error can't
/// carry it.
struct Seed<'v, 'a, 'b>(
    &'a mut Strand<'v, 'b>,
    Slot<'v, 'a>,
    &'a mut Option<DecodeKind>,
);

impl Seed<'_, '_, '_> {
    fn fail<E: de::Error>(cell: &mut Option<DecodeKind>, kind: DecodeKind, message: &str) -> E {
        *cell = Some(kind);
        E::custom(message)
    }
}

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
        Output::set(self.0, &mut self.1, i128::from(v));
        Ok(())
    }

    fn visit_u128<E>(mut self, v: u128) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        let value = i128::try_from(v)
            .map_err(|_| Self::fail(self.2, DecodeKind::Range, "numeric overflow"))?;
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
        debug_assert!(false, "TOML has no null");
        Err(de::Error::custom("nil values are not TOML-representable"))
    }

    fn visit_unit<E>(self) -> result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        debug_assert!(false, "TOML has no null");
        Err(de::Error::custom("nil values are not TOML-representable"))
    }

    fn visit_seq<A>(self, mut seq: A) -> result::Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let Seed(strand, mut out, kind) = self;
        Output::set(strand, &mut out, Empty::Array);
        let array = out.as_array(strand).unwrap();
        strand.with_slots_sync(|strand, [mut elem]| {
            while let Some(()) =
                seq.next_element_seed(Seed(strand, Slot::reborrow(&mut elem), &mut *kind))?
            {
                array.push(strand, &mut elem).unwrap();
            }
            Ok(())
        })
    }

    fn visit_map<A>(self, mut map: A) -> result::Result<Self::Value, A::Error>
    where
        A: de::MapAccess<'de>,
    {
        let Seed(strand, mut out, kind) = self;
        Output::set(strand, &mut out, Empty::Dict);
        let dict = out.as_dict(strand).unwrap();
        strand.with_slots_sync(|strand, [mut key, mut value, mut saved_key]| {
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
                map.next_value_seed(Seed(strand, Slot::reborrow(&mut value), &mut *kind))?;
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
                        return Err(Self::fail(
                            kind,
                            DecodeKind::Unsupported,
                            "TOML datetimes are not supported",
                        ));
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

/// Parses `src` as a document, or failing that as a bare value.
///
/// When neither parses, the error that got further into the input is
/// reported, the document's on a tie.
fn parse<'v, 's>(strand: &mut Strand<'v, 's>, src: &str, out: Slot<'v, '_>) -> Result<'v, 's, ()> {
    let mut kind = None;
    let result = match toml::de::Deserializer::parse(src) {
        Ok(de) => Seed(&mut *strand, out, &mut kind).deserialize(de),
        Err(doc_error) => match toml::de::ValueDeserializer::parse(src) {
            Ok(de) => Seed(&mut *strand, out, &mut kind).deserialize(de),
            Err(value_error) => {
                let start = |e: &toml::de::Error| e.span().map_or(0, |span| span.start);
                Err(if start(&value_error) > start(&doc_error) {
                    value_error
                } else {
                    doc_error
                })
            }
        },
    };
    result.map_err(|error| decode_error(strand, src, &error, kind))
}

/// Raises `toml.DecodeError` for `error`, of `kind` when the visitor noted
/// one.
fn decode_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: &str,
    error: &toml::de::Error,
    kind: Option<DecodeKind>,
) -> Error<'v, 's> {
    let message = error.message();
    let kind = kind.unwrap_or(match message {
        INT_OVERFLOW | FLOAT_OVERFLOW => DecodeKind::Range,
        _ => DecodeKind::Syntax,
    });
    let offset = error.span().map_or(0, |span| span.start);
    let before = src.get(..offset).unwrap_or(src);
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = before.matches('\n').count() + 1;
    let column = before[line_start..].chars().count() + 1;
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

pub(crate) fn configure<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("toml")
        .value("DecodeError", global.types.decode_error)
        .value("EncodeError", global.types.encode_error)
        .function("encode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            strand.with_slots_sync(|strand, [path, mut tuple, mut error]| {
                let mut buf = StrEmbryo::new();
                let mut encoder = Encoder {
                    out: &mut buf,
                    seen: HashSet::new(),
                    section: Vec::new(),
                    started: false,
                    path,
                };
                let result = if is_table(&arg.view(strand.vm())) {
                    encoder.table(strand, &arg, false)
                } else {
                    encoder.inline(strand, &arg)
                };
                match result {
                    Ok(()) => {}
                    Err(Fault::Do(error)) => return Err(error),
                    Err(Fault::Encode(annex)) => {
                        match encoder.path.as_array(strand.vm()) {
                            Some(path) => {
                                path.reverse(strand).expect("path is not shared");
                                path.to_tuple(strand, &mut tuple)
                                    .expect("path is not shared");
                            }
                            None => Output::set(strand, &mut tuple, AsTuple::new([Nil; 0])),
                        }
                        let ty = strand.state::<Tag>().types.encode_error;
                        ty.create_with_annex(strand, EncodeError, annex, &mut error);
                        ty.cast(&error)
                            .unwrap()
                            .enter_sync(strand, |strand, instance| {
                                let mut borrow = instance.borrow_mut_unwrap();
                                Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &tuple);
                            });
                        return Err(Error::from_value(strand, &error));
                    }
                }
                buf.finish(strand, out);
                Ok(())
            })
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

impl<'v> Object<'v> for DecodeError {
    const NAME: &'v str = "DecodeError";
    const MODULE: &'v str = "toml";
    type Annex = DecodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let syntax = builder.sym("SYNTAX");
        let unsupported = builder.sym("UNSUPPORTED");
        let range = builder.sym("RANGE");
        builder
            .nominal_supertype(TypeObject::DataError)
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    DecodeKind::Syntax => syntax,
                    DecodeKind::Unsupported => unsupported,
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
        fmt!(strand, w, "<toml.DecodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> Object<'v> for EncodeError {
    const NAME: &'v str = "EncodeError";
    const MODULE: &'v str = "toml";
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
        fmt!(strand, w, "<toml.EncodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}
