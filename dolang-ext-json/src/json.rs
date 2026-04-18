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
    value::{Empty, Nil, View},
    vm::Builder,
};

/// Wraps a value for serde serialization.
struct SerializeValue<'v, 's, 'a> {
    strand: RefCell<&'a mut Strand<'v, 's>>,
    seen: RefCell<&'a mut HashSet<NonZero<usize>>>,
    value: &'a Value<'v>,
}

impl<'v, 's, 'a> Serialize for SerializeValue<'v, 's, 'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let strand = &mut *self.strand.borrow_mut();
        let seen = &mut *self.seen.borrow_mut();
        match self.value.view(strand.vm()) {
            View::Nil => serializer.serialize_none(),
            View::Bool(b) => serializer.serialize_bool(b),
            View::Int(i) => serializer.serialize_i64(i),
            View::Float(f) => serializer.serialize_f64(f),
            View::Str(s) => strand.access(|access| serializer.serialize_str(s.as_str(access))),
            View::Bin(_) => Err(ser::Error::custom(
                "binary values are not JSON-representable",
            )),
            View::Sym(sym) => serializer.serialize_str(sym.as_str(strand.vm())),

            View::Array(handle) => {
                let id = handle.id();
                if !seen.insert(id.addr()) {
                    return Err(ser::Error::custom("cyclic reference to array"));
                }
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
                            seen: RefCell::new(seen),
                            value: &elem,
                        })
                    })?
                }
                seen.remove(&id.addr());
                seq.end()
            }

            View::Dict(handle) => {
                let id = handle.id();
                if !seen.insert(id.addr()) {
                    return Err(ser::Error::custom("cyclic reference to dict"));
                }
                let len = handle
                    .len(strand)
                    .map_err(|_| ser::Error::custom("concurrent access to dict"))?;
                let mut ser = serializer.serialize_map(Some(len))?;
                let mut pairs = handle.pairs();
                strand.with_slots_sync(|strand, [mut key, mut val]| {
                    while pairs
                        .next(strand, &mut key, &mut val)
                        .map_err(|_| ser::Error::custom("concurrent access to dict"))?
                    {
                        ser.serialize_key(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: RefCell::new(seen),
                            value: &key,
                        })?;
                        ser.serialize_value(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: RefCell::new(seen),
                            value: &val,
                        })?;
                    }
                    Ok(())
                })?;
                seen.remove(&id.addr());
                ser.end()
            }

            View::Record(handle) => {
                let id = handle.id();
                if !seen.insert(id.addr()) {
                    return Err(ser::Error::custom("cyclic reference to record"));
                }
                let len = handle
                    .len(strand)
                    .map_err(|_| ser::Error::custom("concurrent access to record"))?;
                let mut ser = serializer.serialize_map(Some(len))?;
                let mut pairs = handle.pairs();
                strand.with_slots_sync(|strand, [mut key, mut val]| {
                    while pairs
                        .next(strand, &mut key, &mut val)
                        .map_err(|_| ser::Error::custom("concurrent access to record"))?
                    {
                        ser.serialize_key(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: RefCell::new(seen),
                            value: &key,
                        })?;
                        ser.serialize_value(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: RefCell::new(seen),
                            value: &val,
                        })?;
                    }
                    Ok(())
                })?;
                seen.remove(&id.addr());
                ser.end()
            }

            View::Tuple(handle) => {
                let id = handle.id();
                if !seen.insert(id.addr()) {
                    return Err(ser::Error::custom("cyclic reference to tuple"));
                }
                let len = handle.len();
                let mut seq = serializer.serialize_seq(Some(len))?;
                for i in 0..len {
                    strand.with_slots_sync(|strand, [mut elem]| {
                        handle
                            .get(strand, i, &mut elem)
                            .map_err(|_| ser::Error::custom("concurrent access to tuple"))?;
                        seq.serialize_element(&SerializeValue {
                            strand: RefCell::new(strand),
                            seen: RefCell::new(seen),
                            value: &elem,
                        })
                    })?
                }
                seen.remove(&id.addr());
                seq.end()
            }

            View::Object(_) => Err(ser::Error::custom("unsupported JSON type")),
        }
    }
}

struct Seed<'v, 'a, 'b>(&'a mut Strand<'v, 'b>, Slot<'v, 'a>);

impl<'v, 'a, 'b, 'de> DeserializeSeed<'de> for Seed<'v, 'a, 'b> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
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

    fn visit_i64<E>(mut self, v: i64) -> Result<Self::Value, E>
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
        Output::set(
            self.0,
            &mut self.1,
            TryInto::<i64>::try_into(v).map_err(|_| de::Error::custom("numeric overflow"))?,
        );
        Ok(())
    }

    fn visit_f64<E>(mut self, v: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, v);
        Ok(())
    }

    fn visit_unit<E>(mut self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Output::set(self.0, &mut self.1, Nil);
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
        self.0.with_slots_sync(|strand, [mut key, mut value]| {
            while let Some(()) = map.next_key_seed(Seed(strand, Slot::reborrow(&mut key)))? {
                map.next_value_seed(Seed(strand, Slot::reborrow(&mut value)))?;
                dict.insert(strand, &mut key, &mut value).unwrap();
            }
            Ok(())
        })
    }
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    builder
        .module("json")
        .function("to_str", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let mut seen = HashSet::new();
            let value = SerializeValue {
                strand: RefCell::new(strand),
                seen: RefCell::new(&mut seen),
                value: &arg,
            };
            let res = serde_json::to_string(&value).into_do(strand)?;
            Output::set(strand, out, res.as_str());
            Ok(())
        })
        .function("from_str", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected str"))?
                .pin();
            let mut dejson = serde_json::Deserializer::from_str(&src);
            Seed(strand, out).deserialize(&mut dejson).into_do(strand)
        })
        .commit();
}
