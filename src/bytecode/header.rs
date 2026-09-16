//! Header of a LuaJIT bytecode dump.

use oxc_allocator::Allocator;

use super::arena_str;
use super::reader::Reader;
use crate::error::{Error, Result};

/// Magic bytes of a standard LuaJIT bytecode dump.
pub const MAGIC_LUAJIT: [u8; 3] = [0x1b, b'L', b'J'];
/// Magic bytes of a FatShark obfuscated dump.
pub const MAGIC_FATSHARK: [u8; 3] = [0x1b, b'F', b'S'];

/// Highest version byte statically known to use LuaJIT's dump format.
pub const MAX_VERSION: u8 = 0x82;

/// Dump is stored big endian.
pub const FLAG_BIG_ENDIAN: u32 = 0x01;
/// Dump carries no debug information and no chunk name.
pub const FLAG_STRIPPED: u32 = 0x02;
/// Dump uses FFI `cdata` constants.
pub const FLAG_HAS_FFI: u32 = 0x04;
/// Dump was produced by a GC64 (`LJ_FR2`) build.
pub const FLAG_FR2: u32 = 0x08;
/// Dump contains bit operator bytecode.
pub const FLAG_BITOP: u32 = 0x10;
/// All flag bits LuaJIT knows about.
pub const FLAG_KNOWN: u32 = 0x1f;

/// Which kind of dump the header describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Magic {
    /// A standard `luajit -b` dump.
    LuaJit,
    /// A FatShark obfuscated dump; the rest of the file is stock LuaJIT.
    FatShark,
}

/// Interpretation of the dump header flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeaderFlags {
    /// Multi byte quantities are stored big endian.
    pub big_endian: bool,
    /// The dump has no debug information and no chunk name.
    pub stripped: bool,
    /// The chunk uses FFI `cdata` constants.
    pub has_ffi: bool,
    /// The dump was produced by a GC64 build, which reserves an extra slot per
    /// call frame.
    pub fr2: bool,
    /// The chunk contains bit operator instructions.
    pub bitop: bool,
}

/// The parsed header of a bytecode dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header<'a> {
    /// Which flavour of dump this is.
    pub magic: Magic,
    /// Bytecode revision. `2` is LuaJIT 2.1.
    pub version: u8,
    /// Dump flags.
    pub flags: HeaderFlags,
    /// Chunk name, absent for stripped dumps.
    pub name: Option<&'a str>,
}

impl Header<'_> {
    /// Chunk name, falling back to `=?` for stripped dumps.
    pub fn chunk_name(&self) -> &str {
        self.name.unwrap_or("=?")
    }
}

/// Reads and validates the dump header.
pub fn read<'a>(alloc: &'a Allocator, reader: &mut Reader<'_>) -> Result<Header<'a>> {
    let magic_bytes = reader.read_bytes(3)?;
    let magic = if magic_bytes == MAGIC_LUAJIT {
        Magic::LuaJit
    } else if magic_bytes == MAGIC_FATSHARK {
        Magic::FatShark
    } else {
        return Err(Error::BadMagic);
    };

    let version = reader.read_u8()?;
    if version != 2 && version != MAX_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }

    let bits = reader.read_uleb128()?;
    let unknown = bits & !FLAG_KNOWN;
    if unknown != 0 {
        return Err(Error::UnsupportedFlags(unknown));
    }
    let flags = HeaderFlags {
        big_endian: bits & FLAG_BIG_ENDIAN != 0,
        stripped: bits & FLAG_STRIPPED != 0,
        has_ffi: bits & FLAG_HAS_FFI != 0,
        fr2: bits & FLAG_FR2 != 0,
        bitop: bits & FLAG_BITOP != 0,
    };

    reader.set_big_endian(flags.big_endian);

    let name = if flags.stripped {
        None
    } else {
        let length = reader.read_uleb128()? as usize;
        if length > reader.remaining() {
            return Err(Error::Truncated {
                offset: reader.pos(),
                needed: length,
                available: reader.remaining(),
            });
        }
        let bytes = reader.read_bytes(length)?;
        Some(arena_str(alloc, bytes))
    };

    Ok(Header {
        magic,
        version,
        flags,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_plain_header() {
        let alloc = Allocator::default();
        let mut data = vec![0x1b, b'L', b'J', 2, 0x08, 0x03];
        data.extend_from_slice(b"@a/");
        let mut reader = Reader::new(&data);
        let header = read(&alloc, &mut reader).unwrap();
        assert_eq!(header.magic, Magic::LuaJit);
        assert_eq!(header.version, 2);
        assert!(header.flags.fr2);
        assert!(!header.flags.big_endian);
        assert_eq!(header.name, Some("@a/"));
    }

    #[test]
    fn stripped_headers_have_no_name() {
        let alloc = Allocator::default();
        let data = [0x1b, b'L', b'J', 2, 0x02];
        let mut reader = Reader::new(&data);
        let header = read(&alloc, &mut reader).unwrap();
        assert!(header.flags.stripped);
        assert_eq!(header.name, None);
        assert_eq!(header.chunk_name(), "=?");
    }

    #[test]
    fn rejects_bad_magic() {
        let alloc = Allocator::default();
        let data = [0x00, b'L', b'J', 2, 0x00];
        let mut reader = Reader::new(&data);
        assert!(matches!(read(&alloc, &mut reader), Err(Error::BadMagic)));
    }

    #[test]
    fn rejects_unknown_versions() {
        // Version 1 is LuaJIT 2.0, which uses a different opcode table.
        let alloc = Allocator::default();
        let data = [0x1b, b'L', b'J', 1, 0x00];
        let mut reader = Reader::new(&data);
        assert!(matches!(
            read(&alloc, &mut reader),
            Err(Error::UnsupportedVersion(1))
        ));
    }

    #[test]
    fn rejects_unknown_flag_bits() {
        let alloc = Allocator::default();
        let data = [0x1b, b'L', b'J', 2, 0x20];
        let mut reader = Reader::new(&data);
        assert!(matches!(
            read(&alloc, &mut reader),
            Err(Error::UnsupportedFlags(0x20))
        ));
    }
}
