#[cfg(unix)]
use std::os::fd::{BorrowedFd, IntoRawFd};
#[cfg(windows)]
use std::os::windows::io::{BorrowedHandle, IntoRawHandle};
use std::{cell::RefCell, fmt, io, marker::PhantomData, ptr};

#[cfg(any(unix, windows))]
use ::serde::de::IntoDeserializer;
use ::serde::{
    Deserialize, Serialize,
    de::{self, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor},
    ser::{self},
};
use postcard::ser_flavors::{ExtendFlavor, Flavor};

use crate::{
    handle::OS_HANDLE_TYPE,
    transport::{RecvFrame, SendFrame},
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("handle transport error: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Message(String),
}

impl ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::Message(msg.to_string())
    }
}
impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::Message(msg.to_string())
    }
}

pub(crate) fn to_extend<'frame, T, F>(
    value: &'frame T,
    frame: &mut F,
    output: Vec<u8>,
) -> Result<Vec<u8>, Error>
where
    T: Serialize + ?Sized,
    F: SendFrame<'frame>,
{
    let mut postcard = postcard::Serializer {
        output: ExtendFlavor::new(output),
    };
    let frame = RefCell::new(frame);
    value.serialize(Serializer {
        inner: &mut postcard,
        frame: &frame,
        marker: PhantomData,
    })?;
    Ok(postcard.output.finalize()?)
}

pub(crate) fn from_bytes<'de, T, H>(bytes: &'de [u8], handles: &mut H) -> Result<T, Error>
where
    T: Deserialize<'de>,
    H: RecvFrame,
{
    let mut postcard = postcard::Deserializer::from_bytes(bytes);
    let value = T::deserialize(Deserializer {
        inner: &mut postcard,
        handles,
    })?;
    let remaining = postcard.finalize()?;
    if !remaining.is_empty() {
        return Err(Error::Message("trailing bytes in payload".into()));
    }
    Ok(value)
}

struct Serializer<'cell, 'borrow, 'frame, S, F> {
    inner: S,
    frame: &'cell RefCell<&'borrow mut F>,
    marker: PhantomData<&'frame ()>,
}
struct WithFrame<'value, 'cell, 'borrow, 'frame, T: ?Sized, F> {
    value: &'value T,
    frame: &'cell RefCell<&'borrow mut F>,
    marker: PhantomData<&'frame ()>,
}
struct Compound<'cell, 'borrow, 'frame, C, F> {
    inner: C,
    frame: &'cell RefCell<&'borrow mut F>,
    marker: PhantomData<&'frame ()>,
}

impl<'frame, T: Serialize + ?Sized, F: SendFrame<'frame>> Serialize
    for WithFrame<'_, '_, '_, 'frame, T, F>
{
    fn serialize<S: ser::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(Serializer {
            inner: serializer,
            frame: self.frame,
            marker: PhantomData,
        })
    }
}

macro_rules! forward_ser {
    ($($name:ident($ty:ty)),* $(,)?) => {$(
        fn $name(self, value: $ty) -> Result<Self::Ok, Self::Error> {
            self.inner.$name(value)
        }
    )*};
}

impl<'cell, 'borrow, 'frame, S, F> ser::Serializer for Serializer<'cell, 'borrow, 'frame, S, F>
where
    F: SendFrame<'frame>,
    S: ser::Serializer,
{
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Compound<'cell, 'borrow, 'frame, S::SerializeSeq, F>;
    type SerializeTuple = Compound<'cell, 'borrow, 'frame, S::SerializeTuple, F>;
    type SerializeTupleStruct = Compound<'cell, 'borrow, 'frame, S::SerializeTupleStruct, F>;
    type SerializeTupleVariant = Compound<'cell, 'borrow, 'frame, S::SerializeTupleVariant, F>;
    type SerializeMap = Compound<'cell, 'borrow, 'frame, S::SerializeMap, F>;
    type SerializeStruct = Compound<'cell, 'borrow, 'frame, S::SerializeStruct, F>;
    type SerializeStructVariant = Compound<'cell, 'borrow, 'frame, S::SerializeStructVariant, F>;

    forward_ser!(
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_i128(i128),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_u128(u128),
        serialize_f32(f32),
        serialize_f64(f64),
        serialize_char(char)
    );
    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        self.inner.serialize_str(v)
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        self.inner.serialize_bytes(v)
    }
    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.inner.serialize_none()
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.inner.serialize_some(&WithFrame {
            value,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        self.inner.serialize_unit()
    }
    fn serialize_unit_struct(self, name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.inner.serialize_unit_struct(name)
    }
    fn serialize_unit_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.inner.serialize_unit_variant(name, index, variant)
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        if ptr::eq(name, OS_HANDLE_TYPE) {
            #[cfg(unix)]
            {
                let raw = value
                    .serialize(RawHandleSerializer)
                    .map_err(ser::Error::custom)?;
                // SAFETY: the writer owns `value` until the frame's consuming
                // `finish` future completes. The frame lifetime is therefore a
                // valid extension of serde's erased raw descriptor borrow.
                let fd = unsafe { BorrowedFd::borrow_raw(raw as i32) };
                let index = self
                    .frame
                    .borrow_mut()
                    .attach_fd(fd)
                    .map_err(ser::Error::custom)?;
                return self.inner.serialize_u32(index);
            }
            #[cfg(windows)]
            {
                let raw = value
                    .serialize(RawHandleSerializer)
                    .map_err(ser::Error::custom)?;
                // The serializer's value remains borrowed for this call. The
                // Windows backend either copies the handle immediately or
                // records only its process-local value.
                let handle = unsafe { BorrowedHandle::borrow_raw(raw as _) };
                let value = self
                    .frame
                    .borrow_mut()
                    .attach_handle(handle)
                    .map_err(ser::Error::custom)?;
                #[cfg(target_pointer_width = "32")]
                return self.inner.serialize_u32(value as u32);
                #[cfg(target_pointer_width = "64")]
                return self.inner.serialize_u64(value as u64);
            }
        }
        self.inner.serialize_newtype_struct(
            name,
            &WithFrame {
                value,
                frame: self.frame,
                marker: PhantomData,
            },
        )
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.inner.serialize_newtype_variant(
            name,
            index,
            variant,
            &WithFrame {
                value,
                frame: self.frame,
                marker: PhantomData,
            },
        )
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(Compound {
            inner: self.inner.serialize_seq(len)?,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Ok(Compound {
            inner: self.inner.serialize_tuple(len)?,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Ok(Compound {
            inner: self.inner.serialize_tuple_struct(name, len)?,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn serialize_tuple_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Ok(Compound {
            inner: self
                .inner
                .serialize_tuple_variant(name, index, variant, len)?,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(Compound {
            inner: self.inner.serialize_map(len)?,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(Compound {
            inner: self.inner.serialize_struct(name, len)?,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Ok(Compound {
            inner: self
                .inner
                .serialize_struct_variant(name, index, variant, len)?,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn collect_str<T: ?Sized + fmt::Display>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.inner.collect_str(value)
    }
    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
}

macro_rules! compound {
    ($trait:ident,$method:ident) => {
        impl<'frame, C: ser::$trait, F: SendFrame<'frame>> ser::$trait
            for Compound<'_, '_, 'frame, C, F>
        {
            type Ok = C::Ok;
            type Error = C::Error;
            fn $method<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
                self.inner.$method(&WithFrame {
                    value,
                    frame: self.frame,
                    marker: PhantomData,
                })
            }
            fn end(self) -> Result<Self::Ok, Self::Error> {
                self.inner.end()
            }
        }
    };
}
compound!(SerializeSeq, serialize_element);
compound!(SerializeTuple, serialize_element);
compound!(SerializeTupleStruct, serialize_field);
compound!(SerializeTupleVariant, serialize_field);
impl<'frame, C: ser::SerializeMap, F: SendFrame<'frame>> ser::SerializeMap
    for Compound<'_, '_, 'frame, C, F>
{
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), Self::Error> {
        self.inner.serialize_key(&WithFrame {
            value: v,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), Self::Error> {
        self.inner.serialize_value(&WithFrame {
            value: v,
            frame: self.frame,
            marker: PhantomData,
        })
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.inner.end()
    }
}
impl<'frame, C: ser::SerializeStruct, F: SendFrame<'frame>> ser::SerializeStruct
    for Compound<'_, '_, 'frame, C, F>
{
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<(), Self::Error> {
        self.inner.serialize_field(
            key,
            &WithFrame {
                value: v,
                frame: self.frame,
                marker: PhantomData,
            },
        )
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.inner.end()
    }
}
impl<'frame, C: ser::SerializeStructVariant, F: SendFrame<'frame>> ser::SerializeStructVariant
    for Compound<'_, '_, 'frame, C, F>
{
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<(), Self::Error> {
        self.inner.serialize_field(
            key,
            &WithFrame {
                value: v,
                frame: self.frame,
                marker: PhantomData,
            },
        )
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.inner.end()
    }
}

#[cfg(any(unix, windows))]
struct RawHandleSerializer;
#[cfg(any(unix, windows))]
impl ser::Serializer for RawHandleSerializer {
    type Ok = usize;
    type Error = Error;
    type SerializeSeq = ser::Impossible<usize, Error>;
    type SerializeTuple = Self::SerializeSeq;
    type SerializeTupleStruct = Self::SerializeSeq;
    type SerializeTupleVariant = Self::SerializeSeq;
    type SerializeMap = Self::SerializeSeq;
    type SerializeStruct = Self::SerializeSeq;
    type SerializeStructVariant = Self::SerializeSeq;
    fn serialize_i32(self, v: i32) -> Result<usize, Error> {
        let _ = v;
        #[cfg(unix)]
        return Ok(v as usize);
        #[cfg(windows)]
        return raw_error();
    }
    fn is_human_readable(&self) -> bool {
        false
    }
    fn serialize_bool(self, _: bool) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_i8(self, _: i8) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_i16(self, _: i16) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_i64(self, _: i64) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_i128(self, _: i128) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_u8(self, _: u8) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_u16(self, _: u16) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_u32(self, v: u32) -> Result<usize, Error> {
        let _ = v;
        #[cfg(all(windows, target_pointer_width = "32"))]
        return Ok(v as usize);
        #[cfg(not(all(windows, target_pointer_width = "32")))]
        return raw_error();
    }
    fn serialize_u64(self, v: u64) -> Result<usize, Error> {
        let _ = v;
        #[cfg(all(windows, target_pointer_width = "64"))]
        return Ok(v as usize);
        #[cfg(not(all(windows, target_pointer_width = "64")))]
        return raw_error();
    }
    fn serialize_u128(self, _: u128) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_f32(self, _: f32) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_f64(self, _: f64) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_char(self, _: char) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_str(self, _: &str) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_none(self) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_unit(self) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
    ) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<usize, Error> {
        raw_error()
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        raw_error()
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Error> {
        raw_error()
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        raw_error()
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        raw_error()
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Error> {
        raw_error()
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, Error> {
        raw_error()
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        raw_error()
    }
}
#[cfg(any(unix, windows))]
fn raw_error<T>() -> Result<T, Error> {
    Err(Error::Message("invalid OsHandle representation".into()))
}

struct Deserializer<'a, D, H> {
    inner: D,
    handles: &'a mut H,
}
struct VisitorWrap<'a, V, H> {
    inner: V,
    handles: &'a mut H,
}
struct SeedWrap<'a, S, H> {
    inner: S,
    handles: &'a mut H,
}
struct SeqWrap<'a, A, H> {
    inner: A,
    handles: &'a mut H,
}
struct MapWrap<'a, A, H> {
    inner: A,
    handles: &'a mut H,
}
struct EnumWrap<'a, A, H> {
    inner: A,
    handles: &'a mut H,
}
struct VariantWrap<'a, A, H> {
    inner: A,
    handles: &'a mut H,
}

macro_rules! forward_de {
    ($($method:ident),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            self.inner.$method(VisitorWrap { inner: visitor, handles: self.handles }).map_err(|e| Error::Message(e.to_string()))
        }
    )*};
}

impl<'de, D, H> de::Deserializer<'de> for Deserializer<'_, D, H>
where
    D: de::Deserializer<'de>,
    D::Error: fmt::Display,
    H: RecvFrame,
{
    type Error = Error;
    forward_de!(
        deserialize_any,
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_option,
        deserialize_unit,
        deserialize_seq,
        deserialize_map,
        deserialize_identifier,
        deserialize_ignored_any
    );
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.inner
            .deserialize_unit_struct(
                name,
                VisitorWrap {
                    inner: visitor,
                    handles: self.handles,
                },
            )
            .map_err(|e| Error::Message(e.to_string()))
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        if ptr::eq(name, OS_HANDLE_TYPE) {
            #[cfg(unix)]
            {
                let index =
                    u32::deserialize(self.inner).map_err(|e| Error::Message(e.to_string()))?;
                let fd = self.handles.take_fd(index)?;
                // The private OsHandle visitor immediately adopts this raw fd.
                // No other visitor can request the reserved newtype identity.
                let raw = fd.into_raw_fd();
                return visitor.visit_newtype_struct(raw.into_deserializer());
            }
            #[cfg(windows)]
            {
                #[cfg(target_pointer_width = "32")]
                let value = u32::deserialize(self.inner)
                    .map_err(|e| Error::Message(e.to_string()))?
                    as usize;
                #[cfg(target_pointer_width = "64")]
                let value = u64::deserialize(self.inner)
                    .map_err(|e| Error::Message(e.to_string()))?
                    as usize;
                let handle = self.handles.take_handle(value)?;
                let raw = handle.into_raw_handle() as usize;
                #[cfg(target_pointer_width = "32")]
                return visitor.visit_newtype_struct((raw as u32).into_deserializer());
                #[cfg(target_pointer_width = "64")]
                return visitor.visit_newtype_struct((raw as u64).into_deserializer());
            }
        }
        self.inner
            .deserialize_newtype_struct(
                name,
                VisitorWrap {
                    inner: visitor,
                    handles: self.handles,
                },
            )
            .map_err(|e| Error::Message(e.to_string()))
    }
    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        self.inner
            .deserialize_tuple(
                len,
                VisitorWrap {
                    inner: visitor,
                    handles: self.handles,
                },
            )
            .map_err(|e| Error::Message(e.to_string()))
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.inner
            .deserialize_tuple_struct(
                name,
                len,
                VisitorWrap {
                    inner: visitor,
                    handles: self.handles,
                },
            )
            .map_err(|e| Error::Message(e.to_string()))
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.inner
            .deserialize_struct(
                name,
                fields,
                VisitorWrap {
                    inner: visitor,
                    handles: self.handles,
                },
            )
            .map_err(|e| Error::Message(e.to_string()))
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.inner
            .deserialize_enum(
                name,
                variants,
                VisitorWrap {
                    inner: visitor,
                    handles: self.handles,
                },
            )
            .map_err(|e| Error::Message(e.to_string()))
    }
    fn is_human_readable(&self) -> bool {
        false
    }
}

impl<'de, S, H> DeserializeSeed<'de> for SeedWrap<'_, S, H>
where
    S: DeserializeSeed<'de>,
    H: RecvFrame,
{
    type Value = S::Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.inner
            .deserialize(Deserializer {
                inner: d,
                handles: self.handles,
            })
            .map_err(de::Error::custom)
    }
}

macro_rules! visit_scalar { ($($name:ident($ty:ty)),* $(,)?) => {$(
 fn $name<E:de::Error>(self,v:$ty)->Result<Self::Value,E>{self.inner.$name(v)}
 )*}; }
impl<'de, V: Visitor<'de>, H: RecvFrame> Visitor<'de> for VisitorWrap<'_, V, H> {
    type Value = V::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.expecting(f)
    }
    visit_scalar!(
        visit_bool(bool),
        visit_i8(i8),
        visit_i16(i16),
        visit_i32(i32),
        visit_i64(i64),
        visit_i128(i128),
        visit_u8(u8),
        visit_u16(u16),
        visit_u32(u32),
        visit_u64(u64),
        visit_u128(u128),
        visit_f32(f32),
        visit_f64(f64),
        visit_char(char)
    );
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        self.inner.visit_str(v)
    }
    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        self.inner.visit_borrowed_str(v)
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        self.inner.visit_string(v)
    }
    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        self.inner.visit_bytes(v)
    }
    fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<Self::Value, E> {
        self.inner.visit_borrowed_bytes(v)
    }
    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
        self.inner.visit_byte_buf(v)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_none()
    }
    fn visit_some<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.inner
            .visit_some(Deserializer {
                inner: d,
                handles: self.handles,
            })
            .map_err(de::Error::custom)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_unit()
    }
    fn visit_newtype_struct<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.inner
            .visit_newtype_struct(Deserializer {
                inner: d,
                handles: self.handles,
            })
            .map_err(de::Error::custom)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        self.inner
            .visit_seq(SeqWrap {
                inner: a,
                handles: self.handles,
            })
            .map_err(de::Error::custom)
    }
    fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        self.inner
            .visit_map(MapWrap {
                inner: a,
                handles: self.handles,
            })
            .map_err(de::Error::custom)
    }
    fn visit_enum<A: EnumAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        self.inner
            .visit_enum(EnumWrap {
                inner: a,
                handles: self.handles,
            })
            .map_err(de::Error::custom)
    }
}
impl<'de, A: SeqAccess<'de>, H: RecvFrame> SeqAccess<'de> for SeqWrap<'_, A, H> {
    type Error = A::Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, A::Error> {
        self.inner.next_element_seed(SeedWrap {
            inner: seed,
            handles: self.handles,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}
impl<'de, A: MapAccess<'de>, H: RecvFrame> MapAccess<'de> for MapWrap<'_, A, H> {
    type Error = A::Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, A::Error> {
        self.inner.next_key_seed(SeedWrap {
            inner: seed,
            handles: self.handles,
        })
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, A::Error> {
        self.inner.next_value_seed(SeedWrap {
            inner: seed,
            handles: self.handles,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}
impl<'a, 'de, A: EnumAccess<'de>, H: RecvFrame> EnumAccess<'de> for EnumWrap<'a, A, H> {
    type Error = A::Error;
    type Variant = VariantWrap<'a, A::Variant, H>;
    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), A::Error> {
        let (v, a) = self.inner.variant_seed(SeedWrap {
            inner: seed,
            handles: self.handles,
        })?;
        Ok((
            v,
            VariantWrap {
                inner: a,
                handles: self.handles,
            },
        ))
    }
}
impl<'de, A: VariantAccess<'de>, H: RecvFrame> VariantAccess<'de> for VariantWrap<'_, A, H> {
    type Error = A::Error;
    fn unit_variant(self) -> Result<(), A::Error> {
        self.inner.unit_variant()
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, A::Error> {
        self.inner.newtype_variant_seed(SeedWrap {
            inner: seed,
            handles: self.handles,
        })
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value, A::Error> {
        self.inner.tuple_variant(
            len,
            VisitorWrap {
                inner: v,
                handles: self.handles,
            },
        )
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, A::Error> {
        self.inner.struct_variant(
            fields,
            VisitorWrap {
                inner: v,
                handles: self.handles,
            },
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::OsHandle;
    use ::serde::{Deserialize, Serialize};
    use bytes::{Buf, BufMut};
    use nix::unistd::pipe;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

    struct Frame<'a>(Vec<BorrowedFd<'a>>);
    impl<'a> SendFrame<'a> for Frame<'a> {
        fn attach_fd(&mut self, fd: BorrowedFd<'a>) -> io::Result<u32> {
            let index = self.0.len() as u32;
            self.0.push(fd);
            Ok(index)
        }
        async fn finish<B: Buf>(self, _buffer: &mut B) -> io::Result<()> {
            Ok(())
        }
    }

    struct TestReceiver(Vec<Option<OwnedFd>>);
    impl RecvFrame for TestReceiver {
        fn take_fd(&mut self, index: u32) -> io::Result<OwnedFd> {
            self.0
                .get_mut(index as usize)
                .and_then(Option::take)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid fd"))
        }
        async fn recv<B: BufMut>(&mut self, _buffer: &mut B) -> io::Result<usize> {
            unreachable!()
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Ordinary {
        Unit,
        Tuple(u32, String),
        Struct { values: Vec<u8> },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Newtype(u64);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Compatibility {
        some: Option<Newtype>,
        none: Option<u8>,
        map: std::collections::BTreeMap<String, Ordinary>,
        variants: Vec<Ordinary>,
    }

    #[test]
    fn ordinary_values_are_postcard_compatible() {
        let value = Compatibility {
            some: Some(Newtype(42)),
            none: None,
            map: [("key".into(), Ordinary::Tuple(7, "value".into()))]
                .into_iter()
                .collect(),
            variants: vec![
                Ordinary::Unit,
                Ordinary::Struct {
                    values: vec![1, 2, 3],
                },
            ],
        };
        let mut frame = Frame(Vec::new());
        let encoded = to_extend(&value, &mut frame, Vec::new()).unwrap();
        assert_eq!(encoded, postcard::to_stdvec(&value).unwrap());
        let mut handles = TestReceiver(Vec::new());
        assert_eq!(
            from_bytes::<Compatibility, _>(&encoded, &mut handles).unwrap(),
            value
        );
    }

    #[derive(Serialize)]
    struct Sending<'a> {
        handles: Option<Vec<OsHandle<BorrowedFd<'a>>>>,
    }
    #[derive(Deserialize)]
    struct Receiving {
        handles: Option<Vec<OsHandle<OwnedFd>>>,
    }

    #[test]
    fn nested_handles_round_trip_through_context() {
        let (fd, _) = pipe().unwrap();
        let value = Sending {
            handles: Some(vec![OsHandle::new(fd.as_fd())]),
        };
        let mut frame = Frame(Vec::new());
        let encoded = to_extend(&value, &mut frame, Vec::new()).unwrap();
        assert_eq!(frame.0.len(), 1);
        let mut handles = TestReceiver(vec![Some(fd.as_fd().try_clone_to_owned().unwrap())]);
        let decoded: Receiving = from_bytes(&encoded, &mut handles).unwrap();
        assert_eq!(decoded.handles.unwrap().len(), 1);
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut encoded = postcard::to_stdvec(&42u32).unwrap();
        encoded.push(0);
        let mut handles = TestReceiver(Vec::new());
        assert!(
            matches!(from_bytes::<u32, _>(&encoded, &mut handles), Err(Error::Message(message)) if message == "trailing bytes in payload")
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};

    use ::serde::{Deserialize, Serialize};
    use bytes::{Buf, BufMut};

    use super::*;
    use crate::OsHandle;

    struct Frame(Option<usize>);

    impl SendFrame<'_> for Frame {
        fn attach_handle(&mut self, handle: BorrowedHandle<'_>) -> io::Result<usize> {
            use std::os::windows::io::AsRawHandle;
            let raw = handle.as_raw_handle() as usize;
            self.0 = Some(raw);
            Ok(raw)
        }

        async fn finish<B: Buf>(self, _buffer: &mut B) -> io::Result<()> {
            Ok(())
        }
    }

    struct TestReceiver(Option<OwnedHandle>);

    impl RecvFrame for TestReceiver {
        fn take_handle(&mut self, value: usize) -> io::Result<OwnedHandle> {
            use std::os::windows::io::AsRawHandle;
            let handle = self.0.take().unwrap();
            assert_eq!(handle.as_raw_handle() as usize, value);
            Ok(handle)
        }

        async fn recv<B: BufMut>(&mut self, _buffer: &mut B) -> io::Result<usize> {
            unreachable!()
        }
    }

    #[derive(Serialize)]
    struct Sending {
        handle: OsHandle<OwnedHandle>,
    }

    #[derive(Deserialize)]
    struct Receiving {
        handle: OsHandle<OwnedHandle>,
    }

    #[test]
    fn handle_round_trips_through_context() {
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let value = Sending {
            handle: OsHandle::new(OwnedHandle::from(file)),
        };
        let mut frame = Frame(None);
        let encoded = to_extend(&value, &mut frame, Vec::new()).unwrap();
        let raw = value.handle.into_inner().into_raw_handle();
        assert_eq!(frame.0, Some(raw as usize));
        let mut receiver = TestReceiver(Some(unsafe { OwnedHandle::from_raw_handle(raw) }));
        let received: Receiving = from_bytes(&encoded, &mut receiver).unwrap();
        drop(received.handle);
    }
}
