//! The LuaJIT bytecode dump model: header, prototypes, instructions and
//! constants.
//!
//! The layout implemented here is the one documented in LuaJIT's
//! `lj_bcdump.h`:
//!
//! ```text
//! dump   = header proto+ 0U
//! header = ESC 'L' 'J' versionB flagsU [namelenU nameB*]
//! proto  = lengthU pdata
//! pdata  = phead bcinsW* uvdataH* kgc* knum* [debugB*]
//! phead  = flagsB numparamsB framesizeB numuvB numkgcU numknU numbcU
//!          [debuglenU [firstlineU numlineU]]
//! ```

pub mod constants;
pub mod debuginfo;
pub mod header;
pub mod instructions;
pub mod opcodes;
pub mod prototype;
pub mod reader;

pub use constants::{Const, ConstKey, Constants, NumConst, Table};
pub use debuginfo::{DebugInfo, VarKind, VariableInfo};
pub use header::{Header, HeaderFlags, Magic};
pub use instructions::{Ins, SLOT_FALSE, SLOT_TRUE};
pub use opcodes::{Mode, OpDef, Opcode};
use oxc_allocator::Allocator;
pub use prototype::{ProtoFlags, Prototype};
pub use reader::Reader;

use crate::error::{Error, Result};

/// Parses a LuaJIT 2.1 bytecode dump into `alloc`.
///
/// This never panics: malformed input is reported as an [`Error`].
pub fn parse<'a>(alloc: &'a Allocator, data: &[u8]) -> Result<Chunk<'a>> {
    let mut reader = Reader::new(data);
    let header = header::read(alloc, &mut reader)?;
    let mut prototypes = prototype::read_all(alloc, &mut reader, &header)?;

    if prototypes.len() != 1 {
        return Err(Error::Malformed(format!(
            "expected a single root prototype, found {}",
            prototypes.len()
        )));
    }

    Ok(Chunk {
        header,
        root: prototypes.pop().expect("checked to hold one prototype"),
    })
}

/// Copies `bytes` into the arena as a string, replacing invalid UTF-8.
pub(crate) fn arena_str<'a>(alloc: &'a Allocator, bytes: &[u8]) -> &'a str {
    match core::str::from_utf8(bytes) {
        Ok(text) => alloc.alloc_str(text),
        Err(_) => {
            let lossy = String::from_utf8_lossy(bytes);
            alloc.alloc_str(&lossy)
        }
    }
}

/// Copies `bytes` into the arena.
pub(crate) fn arena_bytes<'a>(alloc: &'a Allocator, bytes: &[u8]) -> &'a [u8] {
    alloc.alloc_slice_copy(bytes)
}

/// A parsed bytecode dump.
///
/// Every prototype of the dump lives in the arena it was parsed into; the root
/// is a reference into that arena, like every child prototype a constant points
/// at.
#[derive(Debug)]
pub struct Chunk<'a> {
    /// Header of the dump.
    pub header: Header<'a>,
    /// The root prototype, which is the main chunk of the file.
    pub root: &'a Prototype<'a>,
}
