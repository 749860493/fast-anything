// ------------------------------------------------------------------ C values, no JSON
//
// fa_typed takes the C function's arguments as plain C values. They reach the port through serde's
// value deserializers, so its parameter types read them as they read JSON, and the result comes back
// through a serializer that takes one number or bool. The rest is for verify: JSON arguments turned
// into the C values the bridge passes, so the replay checks and times what the app calls.

use serde::de::IntoDeserializer;
use serde::de::value::{Error, SeqDeserializer};
use serde::ser::{Impossible, Serialize, Serializer};
use serde_json::{Value, json};
use std::ffi::{CStr, CString, c_char};

/// Argument i, as the port's type.
pub(super) fn typed_arg<'de, T, D>(i: usize, d: D) -> Result<T, String>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de, Error = Error>,
{
    T::deserialize(d).map_err(|e| format!("argument {}: {e}", i + 1))
}

/// The items of a C array (argument i), as the port's type.
pub(super) fn typed_items<'de, T, I>(i: usize, items: I) -> Result<T, String>
where
    T: serde::Deserialize<'de>,
    I: Iterator,
    I::Item: IntoDeserializer<'de, Error>,
{
    typed_arg(i, SeqDeserializer::new(items))
}

/// A C string. NULL and text that isn't UTF-8 aren't answered.
///
/// # Safety
/// p is NULL or a NUL-terminated string.
pub(super) unsafe fn typed_str<'a>(i: usize, p: *const c_char) -> Result<&'a str, String> {
    if p.is_null() {
        return Err(format!("argument {}: NULL", i + 1));
    }
    unsafe { CStr::from_ptr(p) }.to_str().map_err(|_| format!("argument {}: not UTF-8", i + 1))
}

/// A C array (argument i) and its length (the next one). NULL is only fine when it's empty.
///
/// # Safety
/// p points to n readable items, or is NULL.
pub(super) unsafe fn typed_slice<'a, T>(i: usize, p: *const T, n: impl TryInto<usize>) -> Result<&'a [T], String> {
    let Ok(n) = n.try_into() else { return Err(format!("argument {}: a negative length", i + 2)) };
    if n == 0 {
        return Ok(&[]);
    }
    if p.is_null() {
        return Err(format!("argument {}: NULL with length {n}", i + 1));
    }
    Ok(unsafe { std::slice::from_raw_parts(p, n) })
}

/// The port's result, before it becomes the C return value.
#[derive(Clone, Copy)]
pub(super) enum TypedNum {
    I(i64),
    U(u64),
    F(f64),
    B(bool),
}

/// The type fa_typed returns through: long long, unsigned long long, double, or int for a bool.
pub(super) trait TypedWide: Copy + Default {
    fn wide(n: TypedNum) -> Self;
    /// The value as verify compares it with the recorded result.
    fn json(self) -> Value;
}

impl TypedWide for i64 {
    fn wide(n: TypedNum) -> Self {
        match n {
            TypedNum::I(v) => v,
            TypedNum::U(v) => i64::try_from(v).unwrap_or(i64::MAX),
            TypedNum::F(v) => v as i64,
            TypedNum::B(v) => i64::from(v),
        }
    }
    fn json(self) -> Value {
        json!(self)
    }
}

impl TypedWide for u64 {
    fn wide(n: TypedNum) -> Self {
        match n {
            TypedNum::I(v) => v as u64,
            TypedNum::U(v) => v,
            TypedNum::F(v) => v as u64,
            TypedNum::B(v) => u64::from(v),
        }
    }
    fn json(self) -> Value {
        json!(self)
    }
}

impl TypedWide for f64 {
    fn wide(n: TypedNum) -> Self {
        match n {
            TypedNum::I(v) => v as f64,
            TypedNum::U(v) => v as f64,
            TypedNum::F(v) => v,
            TypedNum::B(v) => f64::from(u8::from(v)),
        }
    }
    fn json(self) -> Value {
        json!(self)
    }
}

impl TypedWide for i32 {
    fn wide(n: TypedNum) -> Self {
        i32::from(match n {
            TypedNum::I(v) => v != 0,
            TypedNum::U(v) => v != 0,
            TypedNum::F(v) => v != 0.0,
            TypedNum::B(v) => v,
        })
    }
    fn json(self) -> Value {
        json!(self != 0)
    }
}

/// The port's result as the C return value.
pub(super) fn typed_ret<W: TypedWide, T: Serialize + ?Sized>(result: &T) -> Result<W, String> {
    result.serialize(TypedRet).map(W::wide).map_err(|e| format!("result: {e}"))
}

struct TypedRet;

fn typed_not<T>(what: &str) -> Result<T, Error> {
    Err(serde::ser::Error::custom(format!("{what}, not a number or bool")))
}

macro_rules! typed_number {
    ($($method:ident $ty:ty => $num:ident),*) => {$(
        fn $method(self, v: $ty) -> Result<TypedNum, Error> {
            Ok(TypedNum::$num(v.into()))
        }
    )*};
}

impl Serializer for TypedRet {
    type Ok = TypedNum;
    type Error = Error;
    type SerializeSeq = Impossible<TypedNum, Error>;
    type SerializeTuple = Impossible<TypedNum, Error>;
    type SerializeTupleStruct = Impossible<TypedNum, Error>;
    type SerializeTupleVariant = Impossible<TypedNum, Error>;
    type SerializeMap = Impossible<TypedNum, Error>;
    type SerializeStruct = Impossible<TypedNum, Error>;
    type SerializeStructVariant = Impossible<TypedNum, Error>;

    typed_number! {
        serialize_bool bool => B, serialize_i8 i8 => I, serialize_i16 i16 => I, serialize_i32 i32 => I,
        serialize_i64 i64 => I, serialize_u8 u8 => U, serialize_u16 u16 => U, serialize_u32 u32 => U,
        serialize_u64 u64 => U, serialize_f32 f32 => F, serialize_f64 f64 => F
    }

    fn serialize_i128(self, v: i128) -> Result<TypedNum, Error> {
        match i64::try_from(v) {
            Ok(v) => Ok(TypedNum::I(v)),
            Err(_) => u64::try_from(v).map(TypedNum::U).or_else(|_| typed_not("an i128 past 64 bits")),
        }
    }
    fn serialize_u128(self, v: u128) -> Result<TypedNum, Error> {
        u64::try_from(v).map(TypedNum::U).or_else(|_| typed_not("a u128 past 64 bits"))
    }
    fn serialize_char(self, _: char) -> Result<TypedNum, Error> {
        typed_not("a char")
    }
    fn serialize_str(self, _: &str) -> Result<TypedNum, Error> {
        typed_not("a string")
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<TypedNum, Error> {
        typed_not("bytes")
    }
    fn serialize_none(self) -> Result<TypedNum, Error> {
        typed_not("None")
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<TypedNum, Error> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<TypedNum, Error> {
        typed_not("()")
    }
    fn serialize_unit_struct(self, name: &'static str) -> Result<TypedNum, Error> {
        typed_not(name)
    }
    fn serialize_unit_variant(self, name: &'static str, _: u32, _: &'static str) -> Result<TypedNum, Error> {
        typed_not(name)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<TypedNum, Error> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<TypedNum, Error> {
        typed_not(name)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        typed_not("a list")
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Error> {
        typed_not("a tuple")
    }
    fn serialize_tuple_struct(self, name: &'static str, _: usize) -> Result<Self::SerializeTupleStruct, Error> {
        typed_not(name)
    }
    fn serialize_tuple_variant(
        self,
        name: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        typed_not(name)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Error> {
        typed_not("a map")
    }
    fn serialize_struct(self, name: &'static str, _: usize) -> Result<Self::SerializeStruct, Error> {
        typed_not(name)
    }
    fn serialize_struct_variant(
        self,
        name: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        typed_not(name)
    }
}

// ------------------------------------------------------------------ for verify

/// Recorded argument i, read as a C value.
pub(super) fn typed_json<T>(args: &[Value], i: usize, read: impl Fn(&Value) -> Option<T>) -> Result<T, String> {
    let v = args.get(i).ok_or_else(|| format!("missing argument {}", i + 1))?;
    read(v).ok_or_else(|| format!("argument {}: {v} doesn't fit the C type", i + 1))
}

pub(super) fn typed_int<T: TryFrom<i64>>(v: &Value) -> Option<T> {
    T::try_from(v.as_i64()?).ok()
}

pub(super) fn typed_uint<T: TryFrom<u64>>(v: &Value) -> Option<T> {
    T::try_from(v.as_u64()?).ok()
}

pub(super) fn typed_f32(v: &Value) -> Option<f32> {
    v.as_f64().map(|x| x as f32)
}

/// A bool, recorded as true/false (or 0/1 in an array).
pub(super) fn typed_bool(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| v.as_i64().map(|n| n != 0))
}

/// A string, or NULL for null.
pub(super) fn typed_cstring(v: &Value) -> Option<Option<CString>> {
    if v.is_null() { Some(None) } else { CString::new(v.as_str()?).ok().map(Some) }
}

pub(super) fn typed_ptr(s: &Option<CString>) -> *const c_char {
    s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr())
}

pub(super) fn typed_vec<T>(v: &Value, read: impl Fn(&Value) -> Option<T>) -> Option<Vec<T>> {
    v.as_array()?.iter().map(read).collect()
}

/// The length (argument i) isn't past the end of the array that holds `have` items.
pub(super) fn typed_fits(i: usize, n: impl TryInto<usize>, have: usize) -> Result<(), String> {
    match n.try_into() {
        Ok(n) if n > have => Err(format!("argument {}: {n} is past the array's {have} items", i + 1)),
        _ => Ok(()),
    }
}

/// fa_call's reply, from a call with the recorded arguments turned into C values.
///
/// # Safety
/// input must point to len readable bytes and out_len to a writable usize.
pub(super) unsafe fn typed_call(
    input: *const u8,
    len: usize,
    out_len: *mut usize,
    call: impl Fn(&[Value]) -> Result<Value, String>,
) -> *mut u8 {
    let input = unsafe { std::slice::from_raw_parts(input, len) };
    let run = std::panic::AssertUnwindSafe(|| {
        let args: Vec<Value> = serde_json::from_slice(input).map_err(|e| e.to_string())?;
        call(&args)
    });
    let reply = match std::panic::catch_unwind(run) {
        Ok(Ok(value)) => json!({ "ok": value }),
        Ok(Err(message)) => json!({ "error": message }),
        Err(panic) => {
            let text = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned());
            json!({ "error": format!("panic: {}", text.unwrap_or_default()) })
        }
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default().into_boxed_slice();
    unsafe { *out_len = bytes.len() };
    Box::into_raw(bytes) as *mut u8
}

/// Nanoseconds per call of run over every recorded input (a JSON list of argument lists), best of
/// at least 3 passes, more while the budget (seconds) lasts. -1 if an input isn't C values.
/// Each call is timed on its own, the way the C recorder times the original, so both per-call
/// numbers carry the same clock cost and a few-nanosecond function doesn't look faster in Rust.
///
/// # Safety
/// input must point to len readable bytes.
pub(super) unsafe fn typed_time<A, R>(
    input: *const u8,
    len: usize,
    budget: f64,
    decode: impl Fn(&[Value]) -> Result<A, String>,
    run: impl Fn(&A) -> R,
) -> f64 {
    let input = unsafe { std::slice::from_raw_parts(input, len) };
    let Ok(calls) = serde_json::from_slice::<Vec<Vec<Value>>>(input) else { return -1.0 };
    let Ok(calls) = calls.iter().map(|a| decode(a)).collect::<Result<Vec<A>, String>>() else { return -1.0 };
    if calls.is_empty() {
        return -1.0;
    }
    let (mut best, mut spent, mut passes) = (f64::INFINITY, 0.0, 0);
    while passes < 3 || (spent < budget && passes < 2000) {
        let mut took = 0.0;
        for a in &calls {
            let t = std::time::Instant::now();
            std::hint::black_box(run(std::hint::black_box(a)));
            took += t.elapsed().as_secs_f64();
        }
        best = best.min(took);
        spent += took;
        passes += 1;
    }
    best / calls.len() as f64 * 1e9
}
