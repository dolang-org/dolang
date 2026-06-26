use std::{
    cell::RefCell,
    collections::HashSet,
    fmt::{self, Formatter},
    num::NonZero,
};

use serde::{
    Serialize, Serializer,
    de::{self, DeserializeSeed, Visitor},
    ser::{self, SerializeMap, SerializeSeq},
};

use dolang::runtime::{
    Output, Slot, Strand, Value,
    error::{Error, ResultExt},
    unpack,
    value::{Empty, View},
    vm::Builder,
};

const DATETIME_FIELD: &str = "$__toml_private_datetime";

struct SeenGuard<'a> {
    seen: &'a RefCell<HashSet<NonZero<usize>>>,
    id: NonZero<usize>,
}

impl Drop for SeenGuard<'_> {
    fn drop(&mut self) {
        self.seen.borrow_mut().remove(&self.id);
    }
}

fn enter_seen<'a, E>(
    seen: &'a RefCell<HashSet<NonZero<usize>>>,
    id: NonZero<usize>,
    err: impl FnOnce() -> E,
) -> Result<SeenGuard<'a>, E> {
    if !seen.borrow_mut().insert(id) {
        return Err(err());
    }
    Ok(SeenGuard { seen, id })
}

struct SerializeValue<'v, 's, 'a> {
    strand: RefCell<&'a mut Strand<'v, 's>>,
    seen: &'a RefCell<HashSet<NonZero<usize>>>,
    value: &'a Value<'v>,
}

struct SerializeKey<'v, 's, 'a> {
    strand: RefCell<&'a mut Strand<'v, 's>>,
    value: &'a Value<'v>,
}

impl<'v, 's, 'a> Serialize for SerializeKey<'v, 's, 'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let strand = &mut *self.strand.borrow_mut();
        match self.value.view(strand.vm()) {
            View::Str(s) => strand.access(|access| serializer.serialize_str(s.as_str(access))),
            View::Sym(sym) => serializer.serialize_str(sym.as_str(strand.vm())),
            _ => Err(ser::Error::custom(
                "TOML table keys must be str or sym values",
            )),
        }
    }
}

impl<'v, 's, 'a> Serialize for SerializeValue<'v, 's, 'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let strand = &mut *self.strand.borrow_mut();
        match self.value.view(strand.vm()) {
            View::Nil => Err(ser::Error::custom("nil values are not TOML-representable")),
            View::Bool(b) => serializer.serialize_bool(b),
            View::Int(i) => serializer.serialize_i64(i64::try_from(i).map_err(|_| {
                ser::Error::custom("TOML integers outside i64 range are not supported")
            })?),
            View::Float(f) => serializer.serialize_f64(f),
            View::Str(s) => strand.access(|access| serializer.serialize_str(s.as_str(access))),
            View::Bin(_) => Err(ser::Error::custom(
                "binary values are not TOML-representable",
            )),
            View::Sym(sym) => serializer.serialize_str(sym.as_str(strand.vm())),

            View::Array(handle) => {
                let id = handle.id();
                let _guard = enter_seen(self.seen, id.addr(), || {
                    ser::Error::custom("cyclic reference to array")
                })?;
                let len = handle
                    .len(strand)
                    .map_err(|_| ser::Error::custom("concurrent access to array"))?;
                let mut seq = serializer.serialize_seq(Some(len))?;
                for i in 0..len {
                    strand.with_slots_sync(|strand, [mut elem]| {
                        handle
                            .get(strand, i, &mut elem)
                            .map_err(|_| ser::Error::custom("concurrent access to array"))?;
                        seq.serialize_element(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: self.seen,
                            value: &elem,
                        })
                    })?;
                }
                seq.end()
            }

            View::Dict(handle) => {
                let id = handle.id();
                let _guard = enter_seen(self.seen, id.addr(), || {
                    ser::Error::custom("cyclic reference to dict")
                })?;
                let len = handle
                    .len(strand)
                    .map_err(|_| ser::Error::custom("concurrent access to dict"))?;
                let mut ser = serializer.serialize_map(Some(len))?;
                let mut pairs = handle.pairs();
                strand.with_slots_sync(|strand, [mut key, mut value]| {
                    while pairs
                        .next(strand, &mut key, &mut value)
                        .map_err(|_| ser::Error::custom("concurrent access to dict"))?
                    {
                        ser.serialize_key(&SerializeKey {
                            strand: RefCell::new(strand),
                            value: &key,
                        })?;
                        ser.serialize_value(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: self.seen,
                            value: &value,
                        })?;
                    }
                    Ok(())
                })?;
                ser.end()
            }

            View::Record(handle) => {
                let id = handle.id();
                let _guard = enter_seen(self.seen, id.addr(), || {
                    ser::Error::custom("cyclic reference to record")
                })?;
                let len = handle
                    .len(strand)
                    .map_err(|_| ser::Error::custom("concurrent access to record"))?;
                let mut ser = serializer.serialize_map(Some(len))?;
                let mut pairs = handle.pairs();
                strand.with_slots_sync(|strand, [mut key, mut value]| {
                    while pairs
                        .next(strand, &mut key, &mut value)
                        .map_err(|_| ser::Error::custom("concurrent access to record"))?
                    {
                        ser.serialize_key(&SerializeKey {
                            strand: RefCell::new(strand),
                            value: &key,
                        })?;
                        ser.serialize_value(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: self.seen,
                            value: &value,
                        })?;
                    }
                    Ok(())
                })?;
                ser.end()
            }

            View::Tuple(handle) => {
                let id = handle.id();
                let _guard = enter_seen(self.seen, id.addr(), || {
                    ser::Error::custom("cyclic reference to tuple")
                })?;
                let len = handle.len();
                let mut seq = serializer.serialize_seq(Some(len))?;
                for i in 0..len {
                    strand.with_slots_sync(|strand, [mut elem]| {
                        handle
                            .get(strand, i, &mut elem)
                            .map_err(|_| ser::Error::custom("concurrent access to tuple"))?;
                        seq.serialize_element(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: self.seen,
                            value: &elem,
                        })
                    })?;
                }
                seq.end()
            }

            View::Object(_) => Err(ser::Error::custom("unsupported TOML type")),
        }
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

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'v, 'a, 'b, 'de> DeserializeSeed<'de> for MapKeySeed<'v, 'a, 'b, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
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

    fn visit_i64<E>(mut self, v: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, i128::from(v));
        Ok(())
    }

    fn visit_i128<E>(mut self, v: i128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_u64<E>(mut self, v: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let value = i64::try_from(v).map_err(|_| de::Error::custom("numeric overflow"))?;
        Output::set(self.0, &mut self.1, value);
        Ok(())
    }

    fn visit_u128<E>(mut self, v: u128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let value = i128::try_from(v).map_err(|_| de::Error::custom("numeric overflow"))?;
        Output::set(self.0, &mut self.1, value);
        Ok(())
    }

    fn visit_f64<E>(mut self, v: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_bool<E>(mut self, v: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_str<E>(mut self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(v)
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&v)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(de::Error::custom("nil values are not TOML-representable"))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(de::Error::custom("nil values are not TOML-representable"))
    }

    fn visit_seq<A>(mut self, mut seq: A) -> Result<Self::Value, A::Error>
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

    fn visit_map<A>(mut self, mut map: A) -> Result<Self::Value, A::Error>
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

                        dict.insert(strand, &mut saved_key, &mut value).unwrap();
                    } else {
                        dict.insert(strand, &mut key, &mut value).unwrap();
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

    fn visit_str<E>(mut self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.strand, &mut self.out, v);
        *self.text = Some(v.to_owned());
        Ok(())
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(v)
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
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

fn is_document<'v>(value: &Value<'v>, vm: &dolang::runtime::vm::Vm<'v>) -> bool {
    matches!(value.view(vm), View::Dict(_) | View::Record(_))
}

fn parse<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: &str,
    out: Slot<'v, '_>,
) -> Result<(), Error<'v, 's>> {
    if let Ok(de) = toml::de::Deserializer::parse(src) {
        return Seed(strand, out).deserialize(de).into_do(strand);
    }

    let de = toml::de::ValueDeserializer::parse(src).into_do(strand)?;
    Seed(strand, out).deserialize(de).into_do(strand)
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    builder
        .module("toml")
        .function("to_str", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let as_document = is_document(&arg, strand.vm());
            let seen = RefCell::new(HashSet::new());
            let value = SerializeValue {
                strand: RefCell::new(strand),
                seen: &seen,
                value: &arg,
            };

            let dst = if as_document {
                toml::to_string(&value)
            } else {
                let mut dst = String::new();
                Serialize::serialize(&value, toml::ser::ValueSerializer::new(&mut dst))
                    .map(|_| ())
                    .map(|_| dst)
            }
            .into_do(strand)?;

            Output::set(strand, out, dst.as_str());
            Ok(())
        })
        .function("from_str", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected str"))?
                .pin();
            parse(strand, &src, out)
        })
        .commit();
}
