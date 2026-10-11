//! An upper bound on the length of a value's compact JSON (`serde_json`),
//! found without writing any of it.
//!
//! Projections are accounted by their JSON length (AT21). Writing the whole
//! projection to learn that length cost more than half of building it, so
//! the bound walks the same `Serialize` calls and adds up what each would
//! write: strings and integers exactly (escapes included), punctuation with
//! at most one spare comma per container, and every finite float as the
//! longest text `serde_json` gives one (24 bytes). The bound is never below
//! the real length, so a value whose bound fits a budget fits it; only a
//! value whose bound does not fit needs the exact length.

use std::fmt;

use serde::Serialize;
use serde::ser::{self, Impossible};

/// Longest text `serde_json` writes for a finite float: a sign, 17
/// significant digits, the point and a three-digit exponent
/// (`-2.2250738585072014e-308`).
const MAX_FLOAT_TEXT: usize = 24;
/// `null`, also what `serde_json` writes for a non-finite float.
const NULL_TEXT: usize = 4;
/// `\u00XX`, the longest escape of one byte.
const MAX_ESCAPE_TEXT: usize = 6;

/// An upper bound on `serde_json::to_vec(value)?.len()`; `None` where
/// `serde_json` would refuse the value (a map key that is not a scalar).
pub fn json_length_bound<T: Serialize + ?Sized>(value: &T) -> Option<usize> {
    let mut bound = Bound::default();
    value.serialize(&mut bound).ok()?;
    Some(bound.total)
}

/// The value cannot be written as JSON.
#[derive(Debug)]
pub struct Unrepresentable;

impl fmt::Display for Unrepresentable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("value has no JSON form")
    }
}

impl std::error::Error for Unrepresentable {}

impl ser::Error for Unrepresentable {
    fn custom<M: fmt::Display>(_message: M) -> Self {
        Self
    }
}

#[derive(Debug, Default)]
struct Bound {
    total: usize,
}

impl Bound {
    fn add(&mut self, bytes: usize) {
        self.total = self.total.saturating_add(bytes);
    }

    /// One element of a container: the value and the comma before the next.
    fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unrepresentable> {
        self.add(1);
        value.serialize(&mut *self)
    }

    /// `"key":` with its comma, then the value.
    fn field<T: Serialize + ?Sized>(
        &mut self,
        key: &str,
        value: &T,
    ) -> Result<(), Unrepresentable> {
        self.add(quoted_len(key).saturating_add(2));
        value.serialize(&mut *self)
    }

    /// `{"variant":`, closed with `}` by the caller's container.
    fn open_variant(&mut self, variant: &str) {
        self.add(quoted_len(variant).saturating_add(3));
    }
}

/// Length of `text` as a JSON string, quotes and escapes included.
fn quoted_len(text: &str) -> usize {
    text.bytes().map(escaped_len).fold(2, usize::saturating_add)
}

/// What `serde_json` writes for one byte of a string.
fn escaped_len(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
        0..=0x1f => MAX_ESCAPE_TEXT,
        _ => 1,
    }
}

fn unsigned_len(value: u128) -> usize {
    value.checked_ilog10().map_or(1, |digits| {
        usize::try_from(digits)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    })
}

fn signed_len(value: i128) -> usize {
    unsigned_len(value.unsigned_abs()) + usize::from(value < 0)
}

fn float_len(finite: bool) -> usize {
    if finite { MAX_FLOAT_TEXT } else { NULL_TEXT }
}

impl ser::Serializer for &mut Bound {
    type Ok = ();
    type Error = Unrepresentable;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_bool(self, value: bool) -> Result<(), Unrepresentable> {
        self.add(if value { 4 } else { 5 });
        Ok(())
    }

    fn serialize_i8(self, value: i8) -> Result<(), Unrepresentable> {
        self.serialize_i128(i128::from(value))
    }

    fn serialize_i16(self, value: i16) -> Result<(), Unrepresentable> {
        self.serialize_i128(i128::from(value))
    }

    fn serialize_i32(self, value: i32) -> Result<(), Unrepresentable> {
        self.serialize_i128(i128::from(value))
    }

    fn serialize_i64(self, value: i64) -> Result<(), Unrepresentable> {
        self.serialize_i128(i128::from(value))
    }

    fn serialize_i128(self, value: i128) -> Result<(), Unrepresentable> {
        self.add(signed_len(value));
        Ok(())
    }

    fn serialize_u8(self, value: u8) -> Result<(), Unrepresentable> {
        self.serialize_u128(u128::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<(), Unrepresentable> {
        self.serialize_u128(u128::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<(), Unrepresentable> {
        self.serialize_u128(u128::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<(), Unrepresentable> {
        self.serialize_u128(u128::from(value))
    }

    fn serialize_u128(self, value: u128) -> Result<(), Unrepresentable> {
        self.add(unsigned_len(value));
        Ok(())
    }

    fn serialize_f32(self, value: f32) -> Result<(), Unrepresentable> {
        self.add(float_len(value.is_finite()));
        Ok(())
    }

    fn serialize_f64(self, value: f64) -> Result<(), Unrepresentable> {
        self.add(float_len(value.is_finite()));
        Ok(())
    }

    fn serialize_char(self, value: char) -> Result<(), Unrepresentable> {
        self.serialize_str(value.encode_utf8(&mut [0; 4]))
    }

    fn serialize_str(self, value: &str) -> Result<(), Unrepresentable> {
        self.add(quoted_len(value));
        Ok(())
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<(), Unrepresentable> {
        // An array of numbers: at most three digits and a comma each.
        self.add(value.len().saturating_mul(4).saturating_add(2));
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Unrepresentable> {
        self.serialize_unit()
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Unrepresentable> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Unrepresentable> {
        self.add(NULL_TEXT);
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Unrepresentable> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), Unrepresentable> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Unrepresentable> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), Unrepresentable> {
        self.open_variant(variant);
        value.serialize(self)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self, Unrepresentable> {
        self.add(2);
        Ok(self)
    }

    fn serialize_tuple(self, len: usize) -> Result<Self, Unrepresentable> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self, Unrepresentable> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self, Unrepresentable> {
        self.open_variant(variant);
        self.serialize_seq(Some(len))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self, Unrepresentable> {
        self.add(2);
        Ok(self)
    }

    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<Self, Unrepresentable> {
        self.serialize_map(Some(len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self, Unrepresentable> {
        self.open_variant(variant);
        self.serialize_map(Some(len))
    }
}

impl ser::SerializeSeq for &mut Bound {
    type Ok = ();
    type Error = Unrepresentable;

    fn serialize_element<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), Unrepresentable> {
        self.element(value)
    }

    fn end(self) -> Result<(), Unrepresentable> {
        Ok(())
    }
}

impl ser::SerializeTuple for &mut Bound {
    type Ok = ();
    type Error = Unrepresentable;

    fn serialize_element<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), Unrepresentable> {
        self.element(value)
    }

    fn end(self) -> Result<(), Unrepresentable> {
        Ok(())
    }
}

impl ser::SerializeTupleStruct for &mut Bound {
    type Ok = ();
    type Error = Unrepresentable;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unrepresentable> {
        self.element(value)
    }

    fn end(self) -> Result<(), Unrepresentable> {
        Ok(())
    }
}

impl ser::SerializeTupleVariant for &mut Bound {
    type Ok = ();
    type Error = Unrepresentable;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unrepresentable> {
        self.element(value)
    }

    fn end(self) -> Result<(), Unrepresentable> {
        Ok(())
    }
}

impl ser::SerializeMap for &mut Bound {
    type Ok = ();
    type Error = Unrepresentable;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Unrepresentable> {
        // The key, its colon and the comma before the next entry.
        self.add(2);
        key.serialize(KeyBound { bound: self })
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unrepresentable> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Unrepresentable> {
        Ok(())
    }
}

impl ser::SerializeStruct for &mut Bound {
    type Ok = ();
    type Error = Unrepresentable;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Unrepresentable> {
        self.field(key, value)
    }

    fn end(self) -> Result<(), Unrepresentable> {
        Ok(())
    }
}

impl ser::SerializeStructVariant for &mut Bound {
    type Ok = ();
    type Error = Unrepresentable;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Unrepresentable> {
        self.field(key, value)
    }

    fn end(self) -> Result<(), Unrepresentable> {
        Ok(())
    }
}

/// A map key: `serde_json` writes scalars as strings (numbers quoted) and
/// refuses everything else, and so does the bound.
struct KeyBound<'a> {
    bound: &'a mut Bound,
}

impl KeyBound<'_> {
    /// A scalar of `text` bytes written inside quotes.
    fn quoted(self, text: usize) -> Result<(), Unrepresentable> {
        self.bound.add(text.saturating_add(2));
        Ok(())
    }
}

impl ser::Serializer for KeyBound<'_> {
    type Ok = ();
    type Error = Unrepresentable;
    type SerializeSeq = Impossible<(), Unrepresentable>;
    type SerializeTuple = Impossible<(), Unrepresentable>;
    type SerializeTupleStruct = Impossible<(), Unrepresentable>;
    type SerializeTupleVariant = Impossible<(), Unrepresentable>;
    type SerializeMap = Impossible<(), Unrepresentable>;
    type SerializeStruct = Impossible<(), Unrepresentable>;
    type SerializeStructVariant = Impossible<(), Unrepresentable>;

    fn serialize_bool(self, value: bool) -> Result<(), Unrepresentable> {
        self.quoted(if value { 4 } else { 5 })
    }

    fn serialize_i8(self, value: i8) -> Result<(), Unrepresentable> {
        self.serialize_i128(i128::from(value))
    }

    fn serialize_i16(self, value: i16) -> Result<(), Unrepresentable> {
        self.serialize_i128(i128::from(value))
    }

    fn serialize_i32(self, value: i32) -> Result<(), Unrepresentable> {
        self.serialize_i128(i128::from(value))
    }

    fn serialize_i64(self, value: i64) -> Result<(), Unrepresentable> {
        self.serialize_i128(i128::from(value))
    }

    fn serialize_i128(self, value: i128) -> Result<(), Unrepresentable> {
        self.quoted(signed_len(value))
    }

    fn serialize_u8(self, value: u8) -> Result<(), Unrepresentable> {
        self.serialize_u128(u128::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<(), Unrepresentable> {
        self.serialize_u128(u128::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<(), Unrepresentable> {
        self.serialize_u128(u128::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<(), Unrepresentable> {
        self.serialize_u128(u128::from(value))
    }

    fn serialize_u128(self, value: u128) -> Result<(), Unrepresentable> {
        self.quoted(unsigned_len(value))
    }

    fn serialize_f32(self, value: f32) -> Result<(), Unrepresentable> {
        if value.is_finite() {
            self.quoted(MAX_FLOAT_TEXT)
        } else {
            Err(Unrepresentable)
        }
    }

    fn serialize_f64(self, value: f64) -> Result<(), Unrepresentable> {
        if value.is_finite() {
            self.quoted(MAX_FLOAT_TEXT)
        } else {
            Err(Unrepresentable)
        }
    }

    fn serialize_char(self, value: char) -> Result<(), Unrepresentable> {
        self.serialize_str(value.encode_utf8(&mut [0; 4]))
    }

    fn serialize_str(self, value: &str) -> Result<(), Unrepresentable> {
        self.bound.add(quoted_len(value));
        Ok(())
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<(), Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_none(self) -> Result<(), Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Unrepresentable> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), Unrepresentable> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Unrepresentable> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<(), Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Unrepresentable> {
        Err(Unrepresentable)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Unrepresentable> {
        Err(Unrepresentable)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::Serialize;

    use super::*;

    fn exact<T: Serialize>(value: &T) -> usize {
        serde_json::to_vec(value).unwrap().len()
    }

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Shape {
        Empty,
        Point(f64),
        Pair(i32, u8),
        Box { width: u16, label: String },
    }

    #[derive(Serialize)]
    struct Unit;

    #[derive(Serialize)]
    struct Newtype(u64);

    #[derive(Serialize)]
    struct Everything {
        flag: bool,
        off: bool,
        small: i8,
        negative: i64,
        large: u128,
        zero: u32,
        ratio: f64,
        tiny: f32,
        nan: f64,
        letter: char,
        text: String,
        escaped: &'static str,
        missing: Option<u8>,
        present: Option<String>,
        unit: (),
        unit_struct: Unit,
        newtype: Newtype,
        shapes: Vec<Shape>,
        empty: Vec<u8>,
        tuple: (u8, String),
        by_name: BTreeMap<String, f64>,
        by_number: BTreeMap<u32, bool>,
        #[serde(skip)]
        _hidden: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped: Option<u8>,
    }

    fn everything() -> Everything {
        Everything {
            flag: true,
            off: false,
            small: -7,
            negative: i64::MIN,
            large: u128::MAX,
            zero: 0,
            ratio: -2.225_073_858_507_201_4e-308,
            tiny: 0.1,
            nan: f64::NAN,
            letter: '\n',
            text: "GPU 0 · 80 GiB".to_owned(),
            escaped: "quote \" back \\ tab \t bell \u{7} del \u{7f}",
            missing: None,
            present: Some(String::new()),
            unit: (),
            unit_struct: Unit,
            newtype: Newtype(1_000),
            shapes: vec![
                Shape::Empty,
                Shape::Point(1.5),
                Shape::Pair(-1, 255),
                Shape::Box {
                    width: 80,
                    label: "acme".to_owned(),
                },
            ],
            empty: Vec::new(),
            tuple: (9, "x".to_owned()),
            by_name: BTreeMap::from([("a".to_owned(), 1.0), ("b\"".to_owned(), 2.5)]),
            by_number: BTreeMap::from([(10, true), (2, false)]),
            _hidden: "x".repeat(1000),
            skipped: None,
        }
    }

    #[test]
    fn the_bound_is_never_below_the_written_length() {
        let value = everything();
        let bound = json_length_bound(&value).unwrap();
        let exact = exact(&value);
        assert!(bound >= exact, "{bound} < {exact}");
    }

    /// Strings, integers and punctuation are counted exactly; only floats
    /// and one comma per container are spare.
    #[test]
    fn strings_and_integers_are_counted_exactly() {
        for text in [
            "",
            "acme",
            "a\"b\\c",
            "\u{1}\u{1f}",
            "\t\n\r\u{8}\u{c}",
            "é ü 中",
        ] {
            assert_eq!(json_length_bound(text), Some(exact(&text)), "{text:?}");
        }
        for number in [0_i64, 9, 10, -1, -10, i64::MAX, i64::MIN] {
            assert_eq!(json_length_bound(&number), Some(exact(&number)));
        }
        assert_eq!(json_length_bound(&u128::MAX), Some(exact(&u128::MAX)));
        assert_eq!(json_length_bound(&Option::<u8>::None), Some(4));
        assert_eq!(json_length_bound(&Vec::<u8>::new()), Some(2));
        assert_eq!(
            json_length_bound(&vec![1_u8, 2, 3]),
            Some(exact(&vec![1_u8, 2, 3]) + 1)
        );
    }

    #[test]
    fn floats_are_bounded_by_their_longest_text() {
        for value in [
            0.0,
            1.0,
            -0.1,
            f64::MAX,
            f64::MIN_POSITIVE,
            -2.225_073_858_507_201_4e-308,
            1e300,
            f64::EPSILON,
        ] {
            let exact = exact(&value);
            assert!(exact <= MAX_FLOAT_TEXT, "{value}: {exact}");
            assert_eq!(json_length_bound(&value), Some(MAX_FLOAT_TEXT));
        }
        assert_eq!(
            json_length_bound(&f64::INFINITY),
            Some(exact(&f64::INFINITY))
        );
        assert!(exact(&f32::MIN_POSITIVE) <= MAX_FLOAT_TEXT);
    }

    #[test]
    fn keys_serde_json_refuses_have_no_bound() {
        let by_pair = BTreeMap::from([((1_u8, 2_u8), 0_u8)]);
        assert!(serde_json::to_vec(&by_pair).is_err());
        assert_eq!(json_length_bound(&by_pair), None);
        let by_absent = BTreeMap::from([(None::<u8>, 0_u8)]);
        assert!(serde_json::to_vec(&by_absent).is_err());
        assert_eq!(json_length_bound(&by_absent), None);
        let by_present = BTreeMap::from([(Some(1_u8), 0_u8), (Some(20_u8), 1_u8)]);
        assert!(json_length_bound(&by_present).unwrap() >= exact(&by_present));
        let by_integer = BTreeMap::from([(1_u8, 0_u8)]);
        assert!(json_length_bound(&by_integer).unwrap() >= exact(&by_integer));
    }

    #[test]
    fn deep_and_empty_containers_are_bounded() {
        let nested = vec![vec![Vec::<u8>::new(); 3]; 3];
        assert!(json_length_bound(&nested).unwrap() >= exact(&nested));
        let empty: BTreeMap<String, u8> = BTreeMap::new();
        assert_eq!(json_length_bound(&empty), Some(exact(&empty)));
    }
}
