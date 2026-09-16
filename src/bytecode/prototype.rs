//! Prototypes: the unit of bytecode a LuaJIT dump stores.

use oxc_allocator::{Allocator, ArenaVec};

use super::constants::{self, Const, Constants};
use super::debuginfo::{self, DebugInfo, VariableInfo};
use super::header::Header;
use super::instructions::Ins;
use super::opcodes::Opcode;
use super::reader::Reader;
use crate::error::{Error, Result};

/// The prototype has child prototypes.
pub const PROTO_CHILD: u8 = 0x01;
/// The prototype is a vararg function.
pub const PROTO_VARARG: u8 = 0x02;
/// The prototype uses FFI `cdata` constants.
pub const PROTO_FFI: u8 = 0x04;
/// The JIT is disabled for this prototype.
pub const PROTO_NOJIT: u8 = 0x08;
/// The bytecode has been patched with `ILOOP` style instructions.
pub const PROTO_ILOOP: u8 = 0x10;
/// The prototype uses bit operator instructions.
pub const PROTO_BITOP: u8 = 0x80;
/// Every prototype flag bit LuaJIT may store in a dump.
pub const PROTO_KNOWN: u8 =
    PROTO_CHILD | PROTO_VARARG | PROTO_FFI | PROTO_NOJIT | PROTO_ILOOP | PROTO_BITOP;

/// Flags of a prototype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProtoFlags(u8);

impl ProtoFlags {
    /// Wraps the raw flag byte.
    pub fn new(bits: u8) -> Self {
        ProtoFlags(bits)
    }

    /// The raw flag byte.
    pub fn raw(self) -> u8 {
        self.0
    }

    /// Whether the prototype has child prototypes.
    pub fn has_child(self) -> bool {
        self.0 & PROTO_CHILD != 0
    }

    /// Whether the prototype is a vararg function.
    pub fn is_variadic(self) -> bool {
        self.0 & PROTO_VARARG != 0
    }

    /// Whether the prototype uses FFI `cdata` constants.
    pub fn has_ffi(self) -> bool {
        self.0 & PROTO_FFI != 0
    }

    /// Whether the JIT is disabled for this prototype.
    pub fn has_nojit(self) -> bool {
        self.0 & PROTO_NOJIT != 0
    }

    /// Whether the JIT is enabled for this prototype.
    pub fn has_jit(self) -> bool {
        self.0 & PROTO_NOJIT == 0
    }

    /// Whether the bytecode was patched by the JIT (`ILOOP` style).
    pub fn has_iloop(self) -> bool {
        self.0 & PROTO_ILOOP != 0
    }

    /// Whether the prototype uses bit operator instructions.
    pub fn has_bitop(self) -> bool {
        self.0 & PROTO_BITOP != 0
    }
}

/// A single LuaJIT function prototype.
#[derive(Debug, PartialEq)]
pub struct Prototype<'a> {
    /// Prototype flags.
    pub flags: ProtoFlags,
    /// Number of fixed parameters.
    pub num_params: u8,
    /// Number of stack slots the function needs.
    pub frame_size: u8,
    /// First source line of the function.
    pub first_line: u32,
    /// Number of source lines the function spans.
    pub num_lines: u32,
    /// The instructions, including the synthesised function header at index 0.
    ///
    /// The dump does not store `[JI]FUNC*` instructions; the reader recreates
    /// them, exactly like LuaJIT's own reader does. Address `n` in the bytecode
    /// therefore corresponds to `instructions[n]`.
    pub instructions: ArenaVec<'a, Ins>,
    /// Upvalue references, GC constants and number constants.
    pub constants: Constants<'a>,
    /// Debug information, empty for stripped dumps.
    pub debug: &'a DebugInfo<'a>,
}

impl<'a> Prototype<'a> {
    /// `true` when the prototype is a vararg function.
    pub fn is_variadic(&self) -> bool {
        self.flags.is_variadic()
    }

    /// The instructions without the synthesised header.
    pub fn body(&self) -> &[Ins] {
        &self.instructions[1..]
    }

    /// Iterates over the child prototypes of this prototype.
    pub fn children(&self) -> impl Iterator<Item = &'a Prototype<'a>> {
        self.constants
            .kgc
            .iter()
            .filter_map(|constant| match constant {
                Const::Child(child) => Some(*child),
                _ => None,
            })
    }

    /// Source line of the instruction at `addr`.
    pub fn line_for(&self, addr: u32) -> u32 {
        self.debug.line_for(addr)
    }

    /// Looks up a local variable name; see [`DebugInfo::local_name`].
    pub fn local_name(&self, addr: u32, slot: u32, alt_mode: bool) -> Option<&VariableInfo<'a>> {
        self.debug.local_name(addr, slot, alt_mode)
    }

    /// Name of the upvalue in `slot`, if debug information is available.
    pub fn upvalue_name(&self, slot: u32) -> Option<&'a str> {
        self.debug.upvalue_name(slot)
    }

    /// The GC constant a `FNEW`, `KSTR`, `TDUP` or `KCDATA` instruction refers
    /// to.
    pub fn kgc(&self, instruction: &Ins) -> Option<&Const<'a>> {
        self.constants.kgc_at(instruction.cd)
    }

    /// The number constant a numeric operand refers to.
    pub fn knum(&self, index: u32) -> Option<constants::NumConst> {
        self.constants.knum_at(index)
    }
}

/// Reads every prototype of a dump.
///
/// LuaJIT writes child prototypes before their parent and in reverse constant
/// order, so a plain stack reproduces the nesting.
pub fn read_all<'a>(
    alloc: &'a Allocator,
    reader: &mut Reader<'_>,
    header: &Header<'_>,
) -> Result<ArenaVec<'a, &'a Prototype<'a>>> {
    let mut stack: ArenaVec<&'a Prototype<'a>> = ArenaVec::new_in(&alloc);

    while !reader.is_eof() {
        let size = reader.read_uleb128()? as usize;
        if size == 0 {
            if reader.is_eof() {
                break;
            }
            return Err(Error::Malformed(
                "zero sized prototype in the middle of the dump".into(),
            ));
        }
        if size > reader.remaining() {
            return Err(Error::Truncated {
                offset: reader.pos(),
                needed: size,
                available: reader.remaining(),
            });
        }

        let start = reader.pos();
        let block_end = start + size;
        let prototype = read_one(alloc, reader, header, &mut stack, block_end)?;

        let consumed = reader.pos() - start;
        if consumed != size {
            return Err(Error::Malformed(format!(
                "prototype declares {size} bytes but occupies {consumed}"
            )));
        }

        stack.push(prototype);
    }

    Ok(stack)
}

fn read_one<'a>(
    alloc: &'a Allocator,
    reader: &mut Reader<'_>,
    header: &Header<'_>,
    stack: &mut ArenaVec<&'a Prototype<'a>>,
    block_end: usize,
) -> Result<&'a Prototype<'a>> {
    let raw_flags = reader.read_u8()?;
    let unknown = raw_flags & !PROTO_KNOWN;
    if unknown != 0 {
        return Err(Error::UnsupportedProtoFlags(unknown));
    }
    let flags = ProtoFlags::new(raw_flags);

    let num_params = reader.read_u8()?;
    let frame_size = reader.read_u8()?;
    let num_uv = usize::from(reader.read_u8()?);
    let num_kgc = reader.read_uleb128()? as usize;
    let num_kn = reader.read_uleb128()? as usize;
    let num_bc = reader.read_uleb128()? as usize;

    let debug_size = if header.flags.stripped {
        0
    } else {
        reader.read_uleb128()? as usize
    };
    let (first_line, num_lines) = if debug_size == 0 {
        (0, 0)
    } else {
        (reader.read_uleb128()?, reader.read_uleb128()?)
    };

    // Every section costs at least this much, so anything above the remaining
    // input is guaranteed to be bogus. This keeps malformed dumps from causing
    // huge allocations or unbounded loops.
    let minimum = 4usize
        .saturating_mul(num_bc)
        .saturating_add(2usize.saturating_mul(num_uv))
        .saturating_add(num_kgc)
        .saturating_add(num_kn)
        .saturating_add(debug_size);
    if minimum > reader.remaining() {
        return Err(Error::Malformed(format!(
            "prototype declares more data ({minimum} bytes minimum) than the dump still holds ({} bytes)",
            reader.remaining()
        )));
    }

    let header_op = if flags.is_variadic() {
        Opcode::FUNCV
    } else {
        Opcode::FUNCF
    };
    let mut instructions = ArenaVec::with_capacity_in(num_bc + 1, &alloc);
    instructions.push(Ins::new_ad(header_op, u32::from(frame_size), 0));
    for _ in 0..num_bc {
        let word = reader.read_u32()?;
        instructions.push(Ins::decode(word, num_kgc)?);
    }

    let constants = constants::read(alloc, reader, num_uv, num_kgc, num_kn, stack)?;

    let debug = if debug_size == 0 {
        alloc.alloc(DebugInfo::new_in(alloc))
    } else {
        debuginfo::read(
            alloc, reader, first_line, num_lines, num_bc, num_uv, block_end,
        )?
    };

    Ok(alloc.alloc(Prototype {
        flags,
        num_params,
        frame_size,
        first_line,
        num_lines,
        instructions,
        constants,
        debug,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_interpreted_bit_by_bit() {
        let flags = ProtoFlags::new(PROTO_CHILD | PROTO_VARARG | PROTO_NOJIT);
        assert!(flags.has_child());
        assert!(flags.is_variadic());
        assert!(!flags.has_jit());
        assert!(!flags.has_ffi());
        assert!(!flags.has_bitop());
    }

    #[test]
    fn jit_is_enabled_unless_the_flag_is_set() {
        assert!(ProtoFlags::default().has_jit());
    }
}
