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
pub use prototype::{ProtoFlags, Prototype};
pub use reader::Reader;

use crate::error::{Error, Result};

/// A parsed bytecode dump.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// Header of the dump.
    pub header: Header,
    /// The root prototype, which is the main chunk of the file.
    pub root: Prototype,
}

/// Parses a LuaJIT 2.1 bytecode dump.
///
/// This never panics: malformed input is reported as an [`Error`].
pub fn parse(data: &[u8]) -> Result<Chunk> {
    let mut reader = Reader::new(data);
    let header = header::read(&mut reader)?;
    let mut prototypes = prototype::read_all(&mut reader, &header)?;

    if prototypes.len() != 1 {
        return Err(Error::Malformed(format!(
            "expected a single root prototype, found {}",
            prototypes.len()
        )));
    }

    Ok(Chunk {
        header,
        root: prototypes.remove(0),
    })
}
