//! The LuaJIT 2.1 opcode table.
//!
//! The numbering follows the current LuaJIT 2.1 opcode enum, i.e. the bit
//! operators occupy `0x59..=0x5F` and the function headers live at
//! `0x60..=0x67`. The function headers are never stored in a dump (the reader
//! synthesises them), but keeping them in the table matches LuaJIT exactly and
//! makes `[JI]FUNC*` decoding well defined.

/// Operand mode of a single instruction element, mirroring LuaJIT's `BCMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Operand is not used.
    None,
    /// A destination slot.
    Dst,
    /// A base slot of a range of slots.
    Base,
    /// A variable slot.
    Var,
    /// A base slot of a range of slots, relative base.
    RBase,
    /// An upvalue index.
    Uv,
    /// An unsigned 8 bit literal.
    Lit,
    /// A signed 16 bit literal.
    LitS,
    /// A primitive type index (`nil`, `false` or `true`).
    Pri,
    /// A numeric constant index (not negated).
    Num,
    /// A string constant index (stored negated).
    Str,
    /// A template table constant index (stored negated).
    Tab,
    /// A child prototype index (stored negated).
    Func,
    /// A jump displacement biased by `BCBIAS_J`.
    Jump,
    /// An FFI `cdata` constant index (stored negated).
    CData,
}

/// Bias applied to jump displacements so that they stay unsigned in the
/// instruction word.
pub const BCBIAS_J: i32 = 0x8000;

macro_rules! define_opcodes {
    ($( $variant:ident = $name:literal, $a:ident, $b:ident, $c:ident; )*) => {
        /// A LuaJIT bytecode opcode.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u8)]
        #[allow(non_camel_case_types)]
        pub enum Opcode {
            $(
                #[doc = concat!("`", $name, "`")]
                $variant,
            )*
        }

        /// The opcode table, indexed by opcode number.
        pub static OPCODES: &[OpDef] = &[
            $(
                OpDef {
                    op: Opcode::$variant,
                    name: $name,
                    a: Mode::$a,
                    b: Mode::$b,
                    c: Mode::$c,
                },
            )*
        ];
    };
}

define_opcodes! {
    ISLT    = "ISLT",    Var,   None,  Var;
    ISGE    = "ISGE",    Var,   None,  Var;
    ISLE    = "ISLE",    Var,   None,  Var;
    ISGT    = "ISGT",    Var,   None,  Var;
    ISEQV   = "ISEQV",   Var,   None,  Var;
    ISNEV   = "ISNEV",   Var,   None,  Var;
    ISEQS   = "ISEQS",   Var,   None,  Str;
    ISNES   = "ISNES",   Var,   None,  Str;
    ISEQN   = "ISEQN",   Var,   None,  Num;
    ISNEN   = "ISNEN",   Var,   None,  Num;
    ISEQP   = "ISEQP",   Var,   None,  Pri;
    ISNEP   = "ISNEP",   Var,   None,  Pri;
    ISTC    = "ISTC",    Dst,   None,  Var;
    ISFC    = "ISFC",    Dst,   None,  Var;
    IST     = "IST",     None,  None,  Var;
    ISF     = "ISF",     None,  None,  Var;
    ISTYPE  = "ISTYPE",  Var,   None,  Lit;
    ISNUM   = "ISNUM",   Var,   None,  Lit;
    MOV     = "MOV",     Dst,   None,  Var;
    NOT     = "NOT",     Dst,   None,  Var;
    UNM     = "UNM",     Dst,   None,  Var;
    LEN     = "LEN",     Dst,   None,  Var;
    ADDVN   = "ADDVN",   Dst,   Var,   Num;
    SUBVN   = "SUBVN",   Dst,   Var,   Num;
    MULVN   = "MULVN",   Dst,   Var,   Num;
    DIVVN   = "DIVVN",   Dst,   Var,   Num;
    MODVN   = "MODVN",   Dst,   Var,   Num;
    ADDNV   = "ADDNV",   Dst,   Var,   Num;
    SUBNV   = "SUBNV",   Dst,   Var,   Num;
    MULNV   = "MULNV",   Dst,   Var,   Num;
    DIVNV   = "DIVNV",   Dst,   Var,   Num;
    MODNV   = "MODNV",   Dst,   Var,   Num;
    ADDVV   = "ADDVV",   Dst,   Var,   Var;
    SUBVV   = "SUBVV",   Dst,   Var,   Var;
    MULVV   = "MULVV",   Dst,   Var,   Var;
    DIVVV   = "DIVVV",   Dst,   Var,   Var;
    MODVV   = "MODVV",   Dst,   Var,   Var;
    POW     = "POW",     Dst,   Var,   Var;
    CAT     = "CAT",     Dst,   RBase, RBase;
    KSTR    = "KSTR",    Dst,   None,  Str;
    KCDATA  = "KCDATA",  Dst,   None,  CData;
    KSHORT  = "KSHORT",  Dst,   None,  LitS;
    KNUM    = "KNUM",    Dst,   None,  Num;
    KPRI    = "KPRI",    Dst,   None,  Pri;
    KNIL    = "KNIL",    Base,  None,  Base;
    UGET    = "UGET",    Dst,   None,  Uv;
    USETV   = "USETV",   Uv,    None,  Var;
    USETS   = "USETS",   Uv,    None,  Str;
    USETN   = "USETN",   Uv,    None,  Num;
    USETP   = "USETP",   Uv,    None,  Pri;
    UCLO    = "UCLO",    RBase, None,  Jump;
    FNEW    = "FNEW",    Dst,   None,  Func;
    TNEW    = "TNEW",    Dst,   None,  Lit;
    TDUP    = "TDUP",    Dst,   None,  Tab;
    GGET    = "GGET",    Dst,   None,  Str;
    GSET    = "GSET",    Var,   None,  Str;
    TGETV   = "TGETV",   Dst,   Var,   Var;
    TGETS   = "TGETS",   Dst,   Var,   Str;
    TGETB   = "TGETB",   Dst,   Var,   Lit;
    TGETR   = "TGETR",   Dst,   Var,   Var;
    TSETV   = "TSETV",   Var,   Var,   Var;
    TSETS   = "TSETS",   Var,   Var,   Str;
    TSETB   = "TSETB",   Var,   Var,   Lit;
    TSETM   = "TSETM",   Base,  None,  Num;
    TSETR   = "TSETR",   Var,   Var,   Var;
    CALLM   = "CALLM",   Base,  Lit,   Lit;
    CALL    = "CALL",    Base,  Lit,   Lit;
    CALLMT  = "CALLMT",  Base,  None,  Lit;
    CALLT   = "CALLT",   Base,  None,  Lit;
    ITERC   = "ITERC",   Base,  Lit,   Lit;
    ITERN   = "ITERN",   Base,  Lit,   Lit;
    VARG    = "VARG",    Base,  Lit,   Lit;
    ISNEXT  = "ISNEXT",  Base,  None,  Jump;
    RETM    = "RETM",    Base,  None,  Lit;
    RET     = "RET",     RBase, None,  Lit;
    RET0    = "RET0",    RBase, None,  Lit;
    RET1    = "RET1",    RBase, None,  Lit;
    FORI    = "FORI",    Base,  None,  Jump;
    JFORI   = "JFORI",   Base,  None,  Jump;
    FORL    = "FORL",    Base,  None,  Jump;
    IFORL   = "IFORL",   Base,  None,  Jump;
    JFORL   = "JFORL",   Base,  None,  Lit;
    ITERL   = "ITERL",   Base,  None,  Jump;
    IITERL  = "IITERL",  Base,  None,  Jump;
    JITERL  = "JITERL",  Base,  None,  Lit;
    LOOP    = "LOOP",    RBase, None,  Jump;
    ILOOP   = "ILOOP",   RBase, None,  Jump;
    JLOOP   = "JLOOP",   RBase, None,  Lit;
    JMP     = "JMP",     RBase, None,  Jump;
    BNOT    = "BNOT",    Dst,   None,  Var;
    BAND    = "BAND",    Dst,   Var,   Var;
    BOR     = "BOR",     Dst,   Var,   Var;
    BXOR    = "BXOR",    Dst,   Var,   Var;
    BSHL    = "BSHL",    Dst,   Var,   Var;
    BSHR    = "BSHR",    Dst,   Var,   Var;
    BSAR    = "BSAR",    Dst,   Var,   Var;
    FUNCF   = "FUNCF",   RBase, None,  None;
    IFUNCF  = "IFUNCF",  RBase, None,  None;
    JFUNCF  = "JFUNCF",  RBase, None,  Lit;
    FUNCV   = "FUNCV",   RBase, None,  None;
    IFUNCV  = "IFUNCV",  RBase, None,  None;
    JFUNCV  = "JFUNCV",  RBase, None,  Lit;
    FUNCC   = "FUNCC",   RBase, None,  None;
    FUNCCW  = "FUNCCW",  RBase, None,  None;
}

use Opcode::*;

impl Opcode {
    /// Looks up an opcode by its encoded byte.
    pub fn from_u8(byte: u8) -> Option<Opcode> {
        OPCODES.get(byte as usize).map(|def| def.op)
    }

    /// The table entry describing this opcode.
    pub fn def(self) -> &'static OpDef {
        &OPCODES[self as usize]
    }

    /// The opcode name as printed by LuaJIT's disassembler.
    pub fn name(self) -> &'static str {
        self.def().name
    }

    /// The five arithmetic opcodes `ADDVN..MODVN`.
    pub fn is_vn_arith(self) -> bool {
        (ADDVN..=MODVN).contains(&self)
    }

    /// The five arithmetic opcodes `ADDNV..MODNV`.
    pub fn is_nv_arith(self) -> bool {
        (ADDNV..=MODNV).contains(&self)
    }

    /// The five arithmetic opcodes `ADDVV..MODVV`.
    pub fn is_vv_arith(self) -> bool {
        (ADDVV..=MODVV).contains(&self)
    }

    /// The unary arithmetic opcodes `MOV..LEN`.
    pub fn is_unary(self) -> bool {
        (MOV..=LEN).contains(&self)
    }

    /// The comparison opcodes `ISLT..ISNEP`.
    pub fn is_comparison(self) -> bool {
        (ISLT..=ISNEP).contains(&self)
    }

    /// The bit operator opcodes `BNOT..BSAR`.
    pub fn is_bitop(self) -> bool {
        (BNOT..=BSAR).contains(&self)
    }

    /// The function header opcodes `FUNCF..FUNCCW`. Never stored in a dump.
    pub fn is_func_header(self) -> bool {
        (self as u8) >= (FUNCF as u8)
    }

    /// Table write opcodes `TSETV..TSETB` plus `TSETR`.
    pub fn is_table_set(self) -> bool {
        matches!(self, TSETV | TSETS | TSETB | TSETR)
    }

    /// Table read opcodes `TGETV..TGETB` plus `TGETR`.
    pub fn is_table_get(self) -> bool {
        matches!(self, TGETV | TGETS | TGETB | TGETR)
    }

    /// Plain call opcodes, including the multi-result and tail call flavours.
    pub fn is_call(self) -> bool {
        matches!(self, CALLM | CALL | CALLMT | CALLT)
    }

    /// Return opcodes.
    pub fn is_return(self) -> bool {
        matches!(self, RETM | RET | RET0 | RET1)
    }

    /// Iterator call opcodes.
    pub fn is_iter_call(self) -> bool {
        matches!(self, ITERC | ITERN)
    }
}

/// A single row of the opcode table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpDef {
    /// The opcode itself.
    pub op: Opcode,
    /// Mnemonic.
    pub name: &'static str,
    /// Mode of the `A` operand.
    pub a: Mode,
    /// Mode of the `B` operand.
    pub b: Mode,
    /// Mode of the `C` (ABC form) or `D` (AD form) operand.
    pub c: Mode,
}

impl OpDef {
    /// Whether this instruction uses the `AD` layout.
    ///
    /// LuaJIT decides the layout purely by whether a `B` operand exists.
    pub fn has_d(self) -> bool {
        self.b == Mode::None
    }

    /// Whether the `C`/`D` operand indexes the GC constants of a prototype
    /// using LuaJIT's negated encoding.
    pub fn is_kgc_operand(self) -> bool {
        matches!(self.c, Mode::Str | Mode::Tab | Mode::Func | Mode::CData)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_indexed_by_opcode_number() {
        for (index, def) in OPCODES.iter().enumerate() {
            assert_eq!(
                def.op as usize, index,
                "opcode {} is out of order",
                def.name
            );
            assert_eq!(Opcode::from_u8(index as u8), Some(def.op));
        }
    }

    #[test]
    fn known_opcode_numbers_match_luajit() {
        assert_eq!(ISLT as u8, 0x00);
        assert_eq!(ISTYPE as u8, 0x10);
        assert_eq!(ISNUM as u8, 0x11);
        assert_eq!(MOV as u8, 0x12);
        assert_eq!(POW as u8, 0x25);
        assert_eq!(JMP as u8, 0x58);
        assert_eq!(BNOT as u8, 0x59);
        assert_eq!(BSAR as u8, 0x5f);
        assert_eq!(FUNCF as u8, 0x60);
        assert_eq!(FUNCCW as u8, 0x67);
        assert_eq!(OPCODES.len(), 0x68);
    }

    #[test]
    fn unknown_opcode_bytes_are_rejected() {
        assert_eq!(Opcode::from_u8(0x68), None);
        assert_eq!(Opcode::from_u8(0xff), None);
    }

    #[test]
    fn operand_layout_follows_luajit() {
        // ABC form: B is used. The "VN"/"NV" suffixes of the arithmetic ops
        // describe which operand is the operand *and* the constant, never the
        // layout: both forms are ABC because `B` holds a register.
        assert!(!ADDVV.def().has_d());
        assert!(!ADDVN.def().has_d());
        assert!(!ADDNV.def().has_d());
        assert!(!CAT.def().has_d());
        assert!(!TSETS.def().has_d());
        // AD form: B is unused, the operand goes into a 16 bit `D`.
        assert!(ISEQS.def().has_d());
        assert!(IST.def().has_d());
        assert!(MOV.def().has_d());
        assert!(KSHORT.def().has_d());
        assert!(JMP.def().has_d());
        assert!(FNEW.def().has_d());
        assert!(TSETM.def().has_d());
        assert!(BNOT.def().has_d());
    }
}
