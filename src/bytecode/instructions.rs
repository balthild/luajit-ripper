//! Decoded bytecode instructions.

use super::opcodes::{BCBIAS_J, OpDef, Opcode};
use crate::error::{Error, Result};

/// Placeholder register number for a condition that is the constant `false`.
///
/// The value is far above any real register, so it can never collide with one.
pub const SLOT_FALSE: u32 = 2_000_000_000;
/// Placeholder register number for a condition that is the constant `true`.
pub const SLOT_TRUE: u32 = 2_000_000_001;

/// A single decoded bytecode instruction.
///
/// LuaJIT stores instructions in two layouts: `ABC` (all three operands used)
/// and `AD` (the second operand field holds a 16 bit `D`). The field `cd`
/// always holds the third operand, whether that is a `C` or a `D`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ins {
    /// Opcode.
    pub op: Opcode,
    /// First operand.
    pub a: u32,
    /// Second operand, only meaningful for the `ABC` layout.
    pub b: u32,
    /// Third operand: `C` for the `ABC` layout, `D` for the `AD` layout.
    ///
    /// For operands that reference a GC constant this holds the resolved
    /// (non negated) index into the prototype's GC constants.
    pub cd: u32,
}

impl Ins {
    /// Decodes a raw instruction word.
    ///
    /// `kgc_count` is the number of GC constants of the owning prototype; it is
    /// needed because LuaJIT stores GC constant indices negated.
    pub fn decode(word: u32, kgc_count: usize) -> Result<Ins> {
        let op_byte = (word & 0xff) as u8;
        let op = Opcode::from_u8(op_byte).ok_or_else(|| {
            Error::Malformed(format!(
                "unknown opcode 0x{op_byte:02x} in instruction 0x{word:08x}"
            ))
        })?;

        let def = op.def();
        let a = (word >> 8) & 0xff;
        let (b, cd) = if def.has_d() {
            (0, (word >> 16) & 0xffff)
        } else {
            ((word >> 24) & 0xff, (word >> 16) & 0xff)
        };

        let cd = if def.is_kgc_operand() {
            let index = kgc_count as i64 - i64::from(cd) - 1;
            if index < 0 || index >= kgc_count as i64 {
                return Err(Error::Malformed(format!(
                    "{} references constant {cd} but the prototype only has {kgc_count}",
                    def.name
                )));
            }
            index as u32
        } else {
            cd
        };

        Ok(Ins { op, a, b, cd })
    }

    /// Builds an `AD` instruction.
    pub fn new_ad(op: Opcode, a: u32, d: u32) -> Ins {
        Ins { op, a, b: 0, cd: d }
    }

    /// Builds an `ABC` instruction.
    pub fn new_abc(op: Opcode, a: u32, b: u32, c: u32) -> Ins {
        Ins { op, a, b, cd: c }
    }

    /// The table entry describing this instruction.
    pub fn def(&self) -> &'static OpDef {
        self.op.def()
    }

    /// The jump displacement of this instruction, with the bias removed.
    pub fn jump_offset(&self) -> i32 {
        self.cd as i32 - BCBIAS_J
    }

    /// Absolute address this jump instruction branches to.
    ///
    /// Returns an `i64` because a malformed displacement can point before the
    /// start of the instruction array; callers must validate the result.
    pub fn jump_target(&self, addr: u32) -> i64 {
        i64::from(addr) + i64::from(self.jump_offset()) + 1
    }

    /// Rewrites the jump displacement so that the instruction branches to
    /// `target`.
    pub fn set_jump_target(&mut self, addr: u32, target: u32) {
        let displacement = target as i64 - i64::from(addr) - 1;
        self.cd = (displacement + i64::from(BCBIAS_J)) as u32;
    }

    /// Rewrites the jump displacement directly, bias included.
    pub fn set_jump_offset(&mut self, offset: i32) {
        self.cd = (offset + BCBIAS_J) as u32;
    }

    /// The `KSHORT` literal, sign extended from its 16 bit encoding.
    pub fn lits(&self) -> i32 {
        let value = self.cd as u16;
        if value & 0x8000 != 0 {
            i32::from(value) - 0x1_0000
        } else {
            i32::from(value)
        }
    }

    /// Whether the two instructions denote the same operation.
    pub fn same_op(&self, other: &Ins) -> bool {
        self.op == other.op && self.a == other.a && self.b == other.b && self.cd == other.cd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_abc_layout() {
        // ADDVV a=1, b=2, c=3
        let word = 0x03 << 16 | 0x02 << 24 | 0x01 << 8 | (Opcode::ADDVV as u32);
        let ins = Ins::decode(word, 0).unwrap();
        assert_eq!(ins.op, Opcode::ADDVV);
        assert_eq!((ins.a, ins.b, ins.cd), (1, 2, 3));

        // ADDVN is ABC too: the "N" suffix describes the operand kind, not the
        // instruction layout.
        let word = 0x03 << 16 | 0x02 << 24 | 0x01 << 8 | (Opcode::ADDVN as u32);
        let ins = Ins::decode(word, 4).unwrap();
        assert_eq!((ins.a, ins.b, ins.cd), (1, 2, 3));
    }

    #[test]
    fn decodes_ad_layout() {
        // MOV a=1, d=2
        let word = 0x0002 << 16 | 0x01 << 8 | (Opcode::MOV as u32);
        let ins = Ins::decode(word, 0).unwrap();
        assert_eq!((ins.a, ins.b, ins.cd), (1, 0, 2));

        // KSHORT a=4, d=0x1234
        let word = 0x1234 << 16 | 0x04 << 8 | (Opcode::KSHORT as u32);
        let ins = Ins::decode(word, 0).unwrap();
        assert_eq!((ins.a, ins.b, ins.cd), (4, 0, 0x1234));
    }

    #[test]
    fn resolves_negated_constant_indices() {
        // LuaJIT encodes the index of a GC constant as its bitwise complement.
        // With three constants, index 0 is stored as ~0 shifted into 16 bits
        // and index 2 as 0.
        let word = (2u32) << 16 | 0x01 << 8 | (Opcode::KSTR as u32);
        let ins = Ins::decode(word, 3).unwrap();
        assert_eq!(ins.cd, 0);

        let word = 0x01 << 8 | (Opcode::KSTR as u32);
        let ins = Ins::decode(word, 3).unwrap();
        assert_eq!(ins.cd, 2);

        let word = (3u32) << 16 | 0x01 << 8 | (Opcode::KSTR as u32);
        let ins = Ins::decode(word, 4).unwrap();
        assert_eq!(ins.cd, 0);
    }

    #[test]
    fn rejects_out_of_range_constant_indices() {
        let word = 0xfffe << 16 | (Opcode::KSTR as u32);
        assert!(Ins::decode(word, 1).is_err());
    }

    #[test]
    fn rejects_unknown_opcodes() {
        assert!(Ins::decode(0x68, 0).is_err());
        assert!(Ins::decode(0xffff_ffff, 0).is_err());
    }

    #[test]
    fn jump_displacement_is_biased() {
        let mut ins = Ins::new_ad(Opcode::JMP, 0, BCBIAS_J as u32 + 4);
        assert_eq!(ins.jump_offset(), 4);
        assert_eq!(ins.jump_target(10), 15);
        ins.set_jump_target(10, 15);
        assert_eq!(ins.cd, BCBIAS_J as u32 + 4);
    }

    #[test]
    fn short_literals_are_sign_extended() {
        let ins = Ins::new_ad(Opcode::KSHORT, 0, 0xffff);
        assert_eq!(ins.lits(), -1);
        let ins = Ins::new_ad(Opcode::KSHORT, 0, 0x7fff);
        assert_eq!(ins.lits(), 0x7fff);
    }
}
