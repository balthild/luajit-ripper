//! Prototype constants: upvalue references, GC constants and number
//! constants.

use oxc_allocator::{Allocator, ArenaVec};

use super::arena_bytes;
use super::prototype::Prototype;
use super::reader::{Reader, as_signed_32, assemble_double};
use crate::error::{Error, Result};

// MARK: constants

/// GC constant tag: a child prototype.
pub const KGC_CHILD: u32 = 0;
/// GC constant tag: a template table.
pub const KGC_TAB: u32 = 1;
/// GC constant tag: a 64 bit signed integer (`cdata`).
pub const KGC_I64: u32 = 2;
/// GC constant tag: a 64 bit unsigned integer (`cdata`).
pub const KGC_U64: u32 = 3;
/// GC constant tag: a complex double (`cdata`).
pub const KGC_COMPLEX: u32 = 4;
/// Lowest GC constant tag that encodes a string; the tag carries the length.
pub const KGC_STR: u32 = 5;

/// Table item tag: `nil`.
pub const KTAB_NIL: u32 = 0;
/// Table item tag: `false`.
pub const KTAB_FALSE: u32 = 1;
/// Table item tag: `true`.
pub const KTAB_TRUE: u32 = 2;
/// Table item tag: a 32 bit signed integer.
pub const KTAB_INT: u32 = 3;
/// Table item tag: a double.
pub const KTAB_NUM: u32 = 4;
/// Lowest table item tag that encodes a string; the tag carries the length.
pub const KTAB_STR: u32 = 5;

/// A constant of a GC type.
#[derive(Debug, PartialEq)]
pub enum Const<'a> {
    /// A child prototype, referenced by `FNEW`.
    Child(&'a Prototype<'a>),
    /// A template table, referenced by `TDUP`.
    Table(Table<'a>),
    /// A 64 bit signed integer constant.
    Int64(i64),
    /// A 64 bit unsigned integer constant.
    Uint64(u64),
    /// A complex double constant.
    Complex(f64, f64),
    /// A string constant.
    Str(&'a [u8]),
}

impl<'a> Const<'a> {
    /// Interprets this constant as a string, if it is one.
    pub fn as_str(&self) -> Option<&'a [u8]> {
        match self {
            Const::Str(value) => Some(value),
            _ => None,
        }
    }

    /// Interprets this constant as a template table, if it is one.
    pub fn as_table(&'a self) -> Option<&'a Table<'a>> {
        match self {
            Const::Table(value) => Some(value),
            _ => None,
        }
    }

    /// Interprets this constant as a child prototype, if it is one.
    pub fn as_child(&self) -> Option<&'a Prototype<'a>> {
        match self {
            Const::Child(value) => Some(value),
            _ => None,
        }
    }
}

/// A key or value inside a template table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstKey<'a> {
    /// `nil`.
    Nil,
    /// A hash value that LuaJIT uses as a marker meaning "this key must be
    /// preserved even though its value is nil".
    KeyMarker,
    /// `false`.
    False,
    /// `true`.
    True,
    /// A 32 bit signed integer.
    Int(i32),
    /// A double.
    Float(f64),
    /// A string.
    Str(&'a [u8]),
}

impl ConstKey<'_> {
    /// Interprets this key as a string, if it is one.
    pub fn as_str(&self) -> Option<&[u8]> {
        match self {
            ConstKey::Str(value) => Some(value),
            _ => None,
        }
    }
}

/// A template table.
#[derive(Debug, PartialEq)]
pub struct Table<'a> {
    /// The array part, holes included.
    pub array: ArenaVec<'a, ConstKey<'a>>,
    /// The hash part, kept in dump order so that output stays stable.
    pub hash: ArenaVec<'a, (ConstKey<'a>, ConstKey<'a>)>,
}

impl<'a> Table<'a> {
    /// Creates an empty template table.
    pub fn new_in(alloc: &'a Allocator) -> Self {
        Table {
            array: ArenaVec::new_in(&alloc),
            hash: ArenaVec::new_in(&alloc),
        }
    }
}

/// A numeric constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumConst {
    /// A 32 bit signed integer.
    Int(i32),
    /// A double.
    Float(f64),
}

impl NumConst {
    /// The value as a `f64`, widening integers.
    pub fn as_f64(self) -> f64 {
        match self {
            NumConst::Int(value) => f64::from(value),
            NumConst::Float(value) => value,
        }
    }
}

/// All constants of a prototype.
#[derive(Debug, PartialEq)]
pub struct Constants<'a> {
    /// Raw upvalue references, including the `PROTO_UV_LOCAL` and
    /// `PROTO_UV_IMMUTABLE` marker bits.
    pub upvalue_refs: ArenaVec<'a, u16>,
    /// Constants of a GC type, in dump order.
    pub kgc: ArenaVec<'a, Const<'a>>,
    /// Numeric constants, in dump order.
    pub knum: ArenaVec<'a, NumConst>,
}

impl<'a> Constants<'a> {
    /// Creates an empty constant table.
    pub fn new_in(alloc: &'a Allocator) -> Self {
        Constants {
            upvalue_refs: ArenaVec::new_in(&alloc),
            kgc: ArenaVec::new_in(&alloc),
            knum: ArenaVec::new_in(&alloc),
        }
    }

    /// The GC constant at `index`.
    pub fn kgc_at(&self, index: u32) -> Option<&Const<'a>> {
        self.kgc.get(index as usize)
    }

    /// The number constant at `index`.
    pub fn knum_at(&self, index: u32) -> Option<NumConst> {
        self.knum.get(index as usize).copied()
    }

    /// The string constant at `index`, if it is a string.
    pub fn string_at(&self, index: u32) -> Option<&'a [u8]> {
        self.kgc_at(index).and_then(Const::as_str)
    }
}

// MARK: reading

/// Reads the upvalue references, GC constants and number constants of a
/// prototype.
///
/// `children` is the stack of already read prototypes; `KGC_CHILD` constants
/// pop from it.
pub fn read<'a>(
    alloc: &'a Allocator,
    reader: &mut Reader<'_>,
    num_uv: usize,
    num_kgc: usize,
    num_kn: usize,
    children: &mut ArenaVec<&'a Prototype<'a>>,
) -> Result<Constants<'a>> {
    let mut upvalue_refs = ArenaVec::with_capacity_in(num_uv.min(reader.remaining() / 2), &alloc);
    for _ in 0..num_uv {
        upvalue_refs.push(reader.read_u16()?);
    }

    let mut kgc = ArenaVec::with_capacity_in(num_kgc.min(reader.remaining()), &alloc);
    for _ in 0..num_kgc {
        kgc.push(read_kgc(alloc, reader, children)?);
    }

    let mut knum = ArenaVec::with_capacity_in(num_kn.min(reader.remaining()), &alloc);
    for _ in 0..num_kn {
        knum.push(read_knum(reader)?);
    }

    Ok(Constants {
        upvalue_refs,
        kgc,
        knum,
    })
}

fn read_kgc<'a>(
    alloc: &'a Allocator,
    reader: &mut Reader<'_>,
    children: &mut ArenaVec<&'a Prototype<'a>>,
) -> Result<Const<'a>> {
    let tag = reader.read_uleb128()?;
    if tag >= KGC_STR {
        let length = (tag - KGC_STR) as usize;
        return Ok(Const::Str(arena_bytes(alloc, reader.read_bytes(length)?)));
    }
    match tag {
        KGC_TAB => Ok(Const::Table(read_table(alloc, reader)?)),
        KGC_CHILD => {
            let child = children.pop().ok_or_else(|| {
                Error::Malformed("child prototype constant without a preceding prototype".into())
            })?;
            Ok(Const::Child(child))
        }
        KGC_I64 => {
            let low = reader.read_uleb128()?;
            let high = reader.read_uleb128()?;
            Ok(Const::Int64(assemble_i64(low, high)))
        }
        KGC_U64 => {
            let low = reader.read_uleb128()?;
            let high = reader.read_uleb128()?;
            Ok(Const::Uint64(assemble_u64(low, high)))
        }
        KGC_COMPLEX => {
            let real = read_number(reader)?;
            let imaginary = read_number(reader)?;
            Ok(Const::Complex(real, imaginary))
        }
        other => Err(Error::Malformed(format!(
            "unknown GC constant type {other}"
        ))),
    }
}

fn read_knum(reader: &mut Reader<'_>) -> Result<NumConst> {
    let (is_number, low) = reader.read_uleb128_33()?;
    if is_number {
        let high = reader.read_uleb128()?;
        Ok(NumConst::Float(assemble_double(low, high)))
    } else {
        Ok(NumConst::Int(as_signed_32(low)))
    }
}

fn read_table<'a>(alloc: &'a Allocator, reader: &mut Reader<'_>) -> Result<Table<'a>> {
    let array_count = reader.read_uleb128()? as usize;
    let hash_count = reader.read_uleb128()? as usize;

    // Every item costs at least one byte, so anything beyond that is bogus.
    let available = reader.remaining();
    if array_count > available || hash_count > available {
        return Err(Error::Malformed(
            "template table size exceeds the input".into(),
        ));
    }

    let mut array = ArenaVec::with_capacity_in(array_count, &alloc);
    for _ in 0..array_count {
        array.push(read_table_item(alloc, reader, false)?);
    }

    let mut hash = ArenaVec::with_capacity_in(hash_count, &alloc);
    for _ in 0..hash_count {
        let key = read_table_item(alloc, reader, false)?;
        let value = read_table_item(alloc, reader, true)?;
        hash.push((key, value));
    }

    Ok(Table { array, hash })
}

fn read_table_item<'a>(
    alloc: &'a Allocator,
    reader: &mut Reader<'_>,
    is_hash_value: bool,
) -> Result<ConstKey<'a>> {
    let tag = reader.read_uleb128()?;
    if tag >= KTAB_STR {
        let length = (tag - KTAB_STR) as usize;
        return Ok(ConstKey::Str(arena_bytes(
            alloc,
            reader.read_bytes(length)?,
        )));
    }
    match tag {
        KTAB_INT => Ok(ConstKey::Int(as_signed_32(reader.read_uleb128()?))),
        KTAB_NUM => Ok(ConstKey::Float(read_number(reader)?)),
        KTAB_TRUE => Ok(ConstKey::True),
        KTAB_FALSE => Ok(ConstKey::False),
        KTAB_NIL => Ok(if is_hash_value {
            ConstKey::KeyMarker
        } else {
            ConstKey::Nil
        }),
        other => Err(Error::Malformed(format!("unknown table item type {other}"))),
    }
}

fn read_number(reader: &mut Reader<'_>) -> Result<f64> {
    let low = reader.read_uleb128()?;
    let high = reader.read_uleb128()?;
    Ok(assemble_double(low, high))
}

/// Reassembles a 64 bit signed integer written as two 32 bit halves.
pub fn assemble_i64(low: u32, high: u32) -> i64 {
    assemble_u64(low, high) as i64
}

/// Reassembles a 64 bit unsigned integer written as two 32 bit halves.
pub fn assemble_u64(low: u32, high: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

// MARK: tests

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(data: &[u8]) -> Reader<'_> {
        Reader::new(data)
    }

    /// Encodes `value` as ULEB128, so that tests do not have to spell out the
    /// byte sequences by hand.
    fn uleb(value: u32) -> Vec<u8> {
        uleb64(u64::from(value))
    }

    fn uleb64(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut value = value;
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                return out;
            }
        }
    }

    /// Encodes a double the way `bcwrite_knum` does: a 33 bit ULEB128 holding
    /// twice the low word plus the "is a number" flag, then the high word.
    fn knum_double(value: f64) -> Vec<u8> {
        let bits = value.to_bits();
        let low = bits as u32;
        let high = (bits >> 32) as u32;
        let mut out = uleb64(2 * u64::from(low) + 1);
        out.extend_from_slice(&uleb(high));
        out
    }

    /// Encodes an integer the way `bcwrite_knum` does.
    fn knum_int(value: i32) -> Vec<u8> {
        uleb64(2 * u64::from(value as u32))
    }

    /// Encodes a double the way `bcwrite_kgc` does for `cdata` constants: the
    /// two 32 bit halves as plain ULEB128 values.
    fn kgc_double(value: f64) -> Vec<u8> {
        let bits = value.to_bits();
        let mut out = uleb(bits as u32);
        out.extend_from_slice(&uleb((bits >> 32) as u32));
        out
    }

    #[test]
    fn reads_string_constants() {
        let alloc = Allocator::default();
        let mut children = ArenaVec::new_in(&&alloc);
        // tag 5 + 3 = length 3, then "abc"
        let data = [8, b'a', b'b', b'c'];
        let mut r = reader(&data);
        let constant = read_kgc(&alloc, &mut r, &mut children).unwrap();
        assert_eq!(constant, Const::Str(&b"abc"[..]));
        assert!(r.is_eof());
    }

    #[test]
    fn reads_64_bit_integer_constants() {
        let alloc = Allocator::default();
        let mut children = ArenaVec::new_in(&&alloc);
        let mut data = vec![KGC_I64 as u8];
        data.extend_from_slice(&uleb(0xffff_ffff));
        data.extend_from_slice(&uleb(0xffff_ffff));
        let mut r = reader(&data);
        assert_eq!(
            read_kgc(&alloc, &mut r, &mut children).unwrap(),
            Const::Int64(-1)
        );
        assert!(r.is_eof());
    }

    #[test]
    fn reads_complex_constants() {
        let alloc = Allocator::default();
        let mut children = ArenaVec::new_in(&&alloc);
        let mut data = vec![KGC_COMPLEX as u8];
        data.extend_from_slice(&kgc_double(1.5));
        data.extend_from_slice(&kgc_double(-2.0));
        let mut r = reader(&data);
        assert_eq!(
            read_kgc(&alloc, &mut r, &mut children).unwrap(),
            Const::Complex(1.5, -2.0)
        );
        assert!(r.is_eof());
    }

    #[test]
    fn reads_number_constants() {
        let mut data = knum_double(1.0);
        let mut r = reader(&data);
        assert_eq!(read_knum(&mut r).unwrap(), NumConst::Float(1.0));
        assert!(r.is_eof());

        data = knum_double(-0.5);
        let mut r = reader(&data);
        assert_eq!(read_knum(&mut r).unwrap(), NumConst::Float(-0.5));
        assert!(r.is_eof());

        // Integers clear the low bit and are sign extended.
        data = knum_int(-1);
        let mut r = reader(&data);
        assert_eq!(read_knum(&mut r).unwrap(), NumConst::Int(-1));
        assert!(r.is_eof());

        data = knum_int(0x7fff_ffff);
        let mut r = reader(&data);
        assert_eq!(read_knum(&mut r).unwrap(), NumConst::Int(0x7fff_ffff));
        assert!(r.is_eof());
    }

    #[test]
    fn reads_template_tables() {
        let alloc = Allocator::default();
        let data = [2, 1, 0, 0, 3, 2, 6, b'k'];
        let mut r = reader(&data);
        let table = read_table(&alloc, &mut r).unwrap();
        assert_eq!(table.array, [ConstKey::Nil, ConstKey::Nil]);
        assert_eq!(table.hash, [(ConstKey::Int(2), ConstKey::Str(&b"k"[..]))]);
        assert!(r.is_eof());
    }

    #[test]
    fn nil_hash_values_become_key_markers() {
        let alloc = Allocator::default();
        let data = [0, 1, 6, b'k', 0];
        let mut r = reader(&data);
        let table = read_table(&alloc, &mut r).unwrap();
        assert_eq!(
            table.hash,
            [(ConstKey::Str(&b"k"[..]), ConstKey::KeyMarker)]
        );
    }

    #[test]
    fn rejects_unknown_constant_tags() {
        let alloc = Allocator::default();
        let mut children = ArenaVec::new_in(&&alloc);
        let mut r = reader(&[9]);
        assert!(read_kgc(&alloc, &mut r, &mut children).is_err());
        let mut r = reader(&[8]);
        assert!(read_table_item(&alloc, &mut r, false).is_err());
    }

    #[test]
    fn rejects_oversized_template_tables() {
        let alloc = Allocator::default();
        let mut data = uleb(u32::MAX);
        data.push(0);
        let mut r = reader(&data);
        assert!(read_table(&alloc, &mut r).is_err());
    }
}
