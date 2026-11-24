// ------------------------------------------------------------------ NaN and infinity
//
// JSON has no NaN or infinity, so they travel as {"<tag>": "<name>"} (FLOATS holds the tag and the
// names this language uses), and ports keep plain f64 / f32. Where the port takes a map or a struct,
// such an object stays a map.

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{
    Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant, Serializer,
};

fn wire_float(v: &serde_json::Value) -> Option<f64> {
    let map = v.as_object().filter(|m| m.len() == 1)?;
    let name = map.get(FLOATS[0])?.as_str()?;
    match FLOATS.iter().skip(1).position(|n| *n == name)? {
        0 => Some(f64::NAN),
        1 => Some(f64::INFINITY),
        _ => Some(f64::NEG_INFINITY),
    }
}

/// A JSON argument, read like serde_json reads a Value, plus tagged NaN and infinity.
#[derive(Clone, Copy)]
struct WireIn<'de>(&'de serde_json::Value);

impl<'de> Deserializer<'de> for WireIn<'de> {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            serde_json::Value::Array(items) => visitor.visit_seq(WireSeq(items.iter())),
            serde_json::Value::Object(map) => match wire_float(self.0) {
                Some(x) => visitor.visit_f64(x),
                None => visitor.visit_map(WireMap { iter: map.iter(), value: None }),
            },
            other => other.deserialize_any(visitor),
        }
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            serde_json::Value::Object(map) => visitor.visit_map(WireMap { iter: map.iter(), value: None }),
            other => other.deserialize_map(visitor),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.0.is_null() { visitor.visit_none() } else { visitor.visit_some(self) }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _: &'static str, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_enum(name, variants, visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit unit_struct
        seq tuple tuple_struct identifier ignored_any
    }
}

struct WireSeq<'de>(std::slice::Iter<'de, serde_json::Value>);

impl<'de> SeqAccess<'de> for WireSeq<'de> {
    type Error = serde_json::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error> {
        self.0.next().map(|v| seed.deserialize(WireIn(v))).transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

struct WireMap<'de> {
    iter: serde_json::map::Iter<'de>,
    value: Option<&'de serde_json::Value>,
}

impl<'de> MapAccess<'de> for WireMap<'de> {
    type Error = serde_json::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error> {
        let Some((key, value)) = self.iter.next() else { return Ok(None) };
        self.value = Some(value);
        seed.deserialize(WireKey(key)).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Self::Error> {
        let value = self.value.take().ok_or_else(|| de::Error::custom("value before key"))?;
        seed.deserialize(WireIn(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// A map key: a string, or a number written as one (as serde_json reads keys).
struct WireKey<'de>(&'de str);

macro_rules! wire_key_number {
    ($($method:ident $visit:ident $ty:ty),*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            match self.0.parse::<$ty>() {
                Ok(n) => visitor.$visit(n),
                Err(_) => visitor.visit_borrowed_str(self.0),
            }
        }
    )*};
}

impl<'de> Deserializer<'de> for WireKey<'de> {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_borrowed_str(self.0)
    }

    wire_key_number! {
        deserialize_i8 visit_i8 i8, deserialize_i16 visit_i16 i16, deserialize_i32 visit_i32 i32,
        deserialize_i64 visit_i64 i64, deserialize_i128 visit_i128 i128, deserialize_u8 visit_u8 u8,
        deserialize_u16 visit_u16 u16, deserialize_u32 visit_u32 u32, deserialize_u64 visit_u64 u64,
        deserialize_u128 visit_u128 u128, deserialize_f32 visit_f32 f32, deserialize_f64 visit_f64 f64,
        deserialize_bool visit_bool bool
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _: &'static str, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        serde_json::Value::String(self.0.to_string()).deserialize_enum(name, variants, visitor)
    }

    serde::forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

/// A port's result, written like serde_json writes it, plus tagged NaN and infinity.
struct WireOut<'a, T: ?Sized>(&'a T);

impl<T: Serialize + ?Sized> Serialize for WireOut<'_, T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(WireSer(s))
    }
}

struct WireSer<S>(S);

/// A list, map or struct being written: each item goes through WireOut too.
struct WireItems<C>(C);

impl<S: Serializer> Serializer for WireSer<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = WireItems<S::SerializeSeq>;
    type SerializeTuple = WireItems<S::SerializeTuple>;
    type SerializeTupleStruct = WireItems<S::SerializeTupleStruct>;
    type SerializeTupleVariant = WireItems<S::SerializeTupleVariant>;
    type SerializeMap = WireItems<S::SerializeMap>;
    type SerializeStruct = WireItems<S::SerializeStruct>;
    type SerializeStructVariant = WireItems<S::SerializeStructVariant>;

    fn serialize_f64(self, v: f64) -> Result<S::Ok, S::Error> {
        if v.is_finite() {
            return self.0.serialize_f64(v);
        }
        let name = if v.is_nan() {
            FLOATS[1]
        } else if v > 0.0 {
            FLOATS[2]
        } else {
            FLOATS[3]
        };
        let mut map = self.0.serialize_map(Some(1))?;
        map.serialize_entry(FLOATS[0], name)?;
        map.end()
    }

    fn serialize_f32(self, v: f32) -> Result<S::Ok, S::Error> {
        if v.is_finite() { self.0.serialize_f32(v) } else { self.serialize_f64(v as f64) }
    }

    fn serialize_bool(self, v: bool) -> Result<S::Ok, S::Error> {
        self.0.serialize_bool(v)
    }
    fn serialize_i8(self, v: i8) -> Result<S::Ok, S::Error> {
        self.0.serialize_i8(v)
    }
    fn serialize_i16(self, v: i16) -> Result<S::Ok, S::Error> {
        self.0.serialize_i16(v)
    }
    fn serialize_i32(self, v: i32) -> Result<S::Ok, S::Error> {
        self.0.serialize_i32(v)
    }
    fn serialize_i64(self, v: i64) -> Result<S::Ok, S::Error> {
        self.0.serialize_i64(v)
    }
    fn serialize_i128(self, v: i128) -> Result<S::Ok, S::Error> {
        self.0.serialize_i128(v)
    }
    fn serialize_u8(self, v: u8) -> Result<S::Ok, S::Error> {
        self.0.serialize_u8(v)
    }
    fn serialize_u16(self, v: u16) -> Result<S::Ok, S::Error> {
        self.0.serialize_u16(v)
    }
    fn serialize_u32(self, v: u32) -> Result<S::Ok, S::Error> {
        self.0.serialize_u32(v)
    }
    fn serialize_u64(self, v: u64) -> Result<S::Ok, S::Error> {
        self.0.serialize_u64(v)
    }
    fn serialize_u128(self, v: u128) -> Result<S::Ok, S::Error> {
        self.0.serialize_u128(v)
    }
    fn serialize_char(self, v: char) -> Result<S::Ok, S::Error> {
        self.0.serialize_char(v)
    }
    fn serialize_str(self, v: &str) -> Result<S::Ok, S::Error> {
        self.0.serialize_str(v)
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<S::Ok, S::Error> {
        self.0.serialize_bytes(v)
    }
    fn serialize_none(self) -> Result<S::Ok, S::Error> {
        self.0.serialize_none()
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<S::Ok, S::Error> {
        self.0.serialize_some(&WireOut(v))
    }
    fn serialize_unit(self) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit()
    }
    fn serialize_unit_struct(self, name: &'static str) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit_struct(name)
    }
    fn serialize_unit_variant(self, name: &'static str, i: u32, variant: &'static str) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit_variant(name, i, variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, name: &'static str, v: &T) -> Result<S::Ok, S::Error> {
        self.0.serialize_newtype_struct(name, &WireOut(v))
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        i: u32,
        variant: &'static str,
        v: &T,
    ) -> Result<S::Ok, S::Error> {
        self.0.serialize_newtype_variant(name, i, variant, &WireOut(v))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, S::Error> {
        self.0.serialize_seq(len).map(WireItems)
    }
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, S::Error> {
        self.0.serialize_tuple(len).map(WireItems)
    }
    fn serialize_tuple_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeTupleStruct, S::Error> {
        self.0.serialize_tuple_struct(name, len).map(WireItems)
    }
    fn serialize_tuple_variant(
        self,
        name: &'static str,
        i: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, S::Error> {
        self.0.serialize_tuple_variant(name, i, variant, len).map(WireItems)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
        self.0.serialize_map(len).map(WireItems)
    }
    fn serialize_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeStruct, S::Error> {
        self.0.serialize_struct(name, len).map(WireItems)
    }
    fn serialize_struct_variant(
        self,
        name: &'static str,
        i: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, S::Error> {
        self.0.serialize_struct_variant(name, i, variant, len).map(WireItems)
    }
    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

impl<C: SerializeSeq> SerializeSeq for WireItems<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), C::Error> {
        self.0.serialize_element(&WireOut(v))
    }
    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTuple> SerializeTuple for WireItems<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), C::Error> {
        self.0.serialize_element(&WireOut(v))
    }
    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTupleStruct> SerializeTupleStruct for WireItems<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), C::Error> {
        self.0.serialize_field(&WireOut(v))
    }
    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeTupleVariant> SerializeTupleVariant for WireItems<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), C::Error> {
        self.0.serialize_field(&WireOut(v))
    }
    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeMap> SerializeMap for WireItems<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, k: &T) -> Result<(), C::Error> {
        self.0.serialize_key(k)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), C::Error> {
        self.0.serialize_value(&WireOut(v))
    }
    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeStruct> SerializeStruct for WireItems<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), C::Error> {
        self.0.serialize_field(key, &WireOut(v))
    }
    fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
        self.0.skip_field(key)
    }
    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}

impl<C: SerializeStructVariant> SerializeStructVariant for WireItems<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), C::Error> {
        self.0.serialize_field(key, &WireOut(v))
    }
    fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
        self.0.skip_field(key)
    }
    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}
