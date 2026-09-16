//! Low level binary reader for LuaJIT bytecode dumps.
//!
//! A dump stores multi byte quantities in the endianness of the machine that
//! produced it, whereas ULEB128 encoded values are byte oriented and therefore
//! endianness independent.

use crate::error::{Error, Result};

/// A cursor over a bytecode dump.
///
/// Every method is bounds checked and reports [`Error::Truncated`] instead of
/// panicking.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    big_endian: bool,
}

impl<'a> Reader<'a> {
    /// Creates a reader assuming little endian multi byte quantities. The real
    /// endianness only becomes known after the dump header has been read.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            big_endian: false,
        }
    }

    /// Switches the endianness used by [`Reader::read_u16`] and
    /// [`Reader::read_u32`].
    pub fn set_big_endian(&mut self, big_endian: bool) {
        self.big_endian = big_endian;
    }

    /// Current read offset in bytes from the start of the dump.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Moves the read cursor to an absolute offset.
    pub fn seek(&mut self, pos: usize) -> Result<()> {
        if pos > self.data.len() {
            return Err(Error::Truncated {
                offset: pos,
                needed: 0,
                available: 0,
            });
        }
        self.pos = pos;
        Ok(())
    }

    /// Number of bytes left to read.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Whether the whole input has been consumed.
    pub fn is_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Total length of the input.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the input is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Whether there are `count` more bytes available.
    pub fn has(&self, count: usize) -> bool {
        self.remaining() >= count
    }

    /// Reads a single byte.
    pub fn read_u8(&mut self) -> Result<u8> {
        let byte = *self.data.get(self.pos).ok_or_else(|| self.truncated(1))?;
        self.pos += 1;
        Ok(byte)
    }

    /// Reads a fixed width unsigned integer in the dump endianness.
    pub fn read_uint(&mut self, width: usize) -> Result<u32> {
        match width {
            1 => Ok(u32::from(self.read_u8()?)),
            2 => Ok(u32::from(self.read_u16()?)),
            4 => self.read_u32(),
            other => Err(Error::Malformed(format!(
                "unsupported integer width {other}"
            ))),
        }
    }

    /// Reads a 16 bit unsigned integer in the dump endianness.
    pub fn read_u16(&mut self) -> Result<u16> {
        let bytes = self.read_bytes(2)?;
        let bytes = [bytes[0], bytes[1]];
        Ok(if self.big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    /// Reads a 32 bit unsigned integer in the dump endianness.
    pub fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_bytes(4)?;
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        Ok(if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    }

    /// Reads `count` bytes.
    pub fn read_bytes(&mut self, count: usize) -> Result<&'a [u8]> {
        if !self.has(count) {
            return Err(self.truncated(count));
        }
        let slice = &self.data[self.pos..self.pos + count];
        self.pos += count;
        Ok(slice)
    }

    /// Reads a NUL terminated string.
    ///
    /// LuaJIT does not guarantee the terminator is present, so hitting the end
    /// of the input simply ends the string.
    pub fn read_zstring(&mut self) -> Result<&'a [u8]> {
        let start = self.pos;
        while let Some(&byte) = self.data.get(self.pos) {
            self.pos += 1;
            if byte == 0 {
                return Ok(&self.data[start..self.pos - 1]);
            }
        }
        Ok(&self.data[start..self.pos])
    }

    /// Reads a ULEB128 encoded 32 bit value.
    pub fn read_uleb128(&mut self) -> Result<u32> {
        let mut value = 0u32;
        let mut shift = 0u32;
        loop {
            if shift > 28 {
                return Err(Error::Malformed(
                    "ULEB128 value does not fit in 32 bits".into(),
                ));
            }
            let byte = self.read_u8()?;
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    /// Reads LuaJIT's 33 bit ULEB128 variant used for number constants.
    ///
    /// Returns `(is_number, low_word)`. When `is_number` is set the value is the
    /// low 32 bits of a `double` and the high word follows as a plain ULEB128;
    /// otherwise it is a 32 bit signed integer.
    pub fn read_uleb128_33(&mut self) -> Result<(bool, u32)> {
        let first = self.read_u8()?;
        let is_number = first & 1 != 0;
        let mut value = u32::from(first >> 1);
        if value >= 0x40 {
            value &= 0x3f;
            let mut shift: i32 = -1;
            let mut read = 0;
            loop {
                let byte = self.read_u8()?;
                shift += 7;
                if shift > 28 {
                    return Err(Error::Malformed(
                        "ULEB128/33 value does not fit in 32 bits".into(),
                    ));
                }
                value |= u32::from(byte & 0x7f) << shift;
                read += 1;
                if byte & 0x80 == 0 {
                    break;
                }
                if read >= 5 {
                    return Err(Error::Malformed("ULEB128/33 value is too long".into()));
                }
            }
        }
        Ok((is_number, value))
    }

    fn truncated(&self, needed: usize) -> Error {
        Error::Truncated {
            offset: self.pos,
            needed,
            available: self.remaining(),
        }
    }
}

/// Reassembles a `double` from the two 32 bit halves as written by LuaJIT.
///
/// The low word is always written first, regardless of the endianness of the
/// target, so the low word always provides the least significant bits.
pub fn assemble_double(low: u32, high: u32) -> f64 {
    f64::from_bits((u64::from(high) << 32) | u64::from(low))
}

/// Reinterprets a 32 bit value as a signed integer.
pub fn as_signed_32(value: u32) -> i32 {
    value as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uleb128_decodes_multibyte_values() {
        let mut reader = Reader::new(&[0xe5, 0x8e, 0x26]);
        assert_eq!(reader.read_uleb128().unwrap(), 624_485);
        assert!(reader.is_eof());
    }

    #[test]
    fn uleb128_rejects_overlong_values() {
        let mut reader = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert!(reader.read_uleb128().is_err());
    }

    #[test]
    fn uleb128_reports_truncation() {
        let mut reader = Reader::new(&[0x80]);
        assert!(matches!(
            reader.read_uleb128(),
            Err(Error::Truncated { offset: 1, .. })
        ));
    }

    #[test]
    fn uint_honours_endianness() {
        let mut reader = Reader::new(&[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(reader.read_uint(4).unwrap(), 0x0403_0201);

        let mut reader = Reader::new(&[0x01, 0x02, 0x03, 0x04]);
        reader.set_big_endian(true);
        assert_eq!(reader.read_uint(4).unwrap(), 0x0102_0304);
    }

    #[test]
    fn zstring_stops_at_terminator_or_eof() {
        let mut reader = Reader::new(b"ab\0cd");
        assert_eq!(reader.read_zstring().unwrap(), b"ab");
        assert_eq!(reader.read_zstring().unwrap(), b"cd");
    }

    #[test]
    fn double_is_reassembled_low_word_first() {
        let value = assemble_double(0x0000_0000, 0x3ff0_0000);
        assert_eq!(value, 1.0);
    }
}
