//! Prototype debug information: line numbers, upvalue names and local variable
//! info.

use oxc_allocator::{Allocator, ArenaVec};

use super::arena_str;
use super::reader::Reader;
use crate::error::{Error, Result};

// MARK: debug info

/// Terminator of the variable info list.
pub const VARNAME_END: u8 = 0;

/// Lowest tag value that starts a real (visible) variable name.
#[allow(non_upper_case_globals)]
pub const VARNAME__MAX: u8 = 7;

/// Names LuaJIT gives to the hidden control variables it creates for loops.
/// Index `0` is the terminator and has no name.
pub const INTERNAL_VARNAMES: [Option<&str>; 7] = [
    None,
    Some("<index>"),
    Some("<limit>"),
    Some("<step>"),
    Some("<generator>"),
    Some("<state>"),
    Some("<control>"),
];

/// Whether a variable info entry is a real local or a synthetic loop variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    /// A local declared in the source.
    Visible,
    /// A hidden variable created by the compiler.
    Internal,
}

/// One entry of the variable info list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariableInfo<'a> {
    /// First instruction address the variable is live at.
    pub start_addr: u32,
    /// First instruction address the variable is dead at.
    pub end_addr: u32,
    /// Whether the variable is visible in the source.
    pub kind: VarKind,
    /// Name of the variable.
    pub name: &'a str,
}

/// Debug information of a single prototype.
///
/// All fields are empty for stripped dumps.
#[derive(Debug, PartialEq, Eq)]
pub struct DebugInfo<'a> {
    /// Source line for every instruction address, including the synthesised
    /// function header at address 0.
    pub addr_to_line: ArenaVec<'a, u32>,
    /// Names of the upvalues captured by this prototype.
    pub upvalue_names: ArenaVec<'a, &'a str>,
    /// Ranges and names of the local variables.
    pub variable_info: ArenaVec<'a, VariableInfo<'a>>,
}

impl<'a> DebugInfo<'a> {
    /// Creates empty debug information, which is what a stripped dump has.
    pub fn new_in(alloc: &'a Allocator) -> Self {
        DebugInfo {
            addr_to_line: ArenaVec::new_in(&alloc),
            upvalue_names: ArenaVec::new_in(&alloc),
            variable_info: ArenaVec::new_in(&alloc),
        }
    }

    /// Whether this prototype carries no debug information at all.
    pub fn is_empty(&self) -> bool {
        self.addr_to_line.is_empty()
            && self.upvalue_names.is_empty()
            && self.variable_info.is_empty()
    }

    /// Source line of the instruction at `addr`, or `0` when unknown.
    pub fn line_for(&self, addr: u32) -> u32 {
        self.addr_to_line.get(addr as usize).copied().unwrap_or(0)
    }

    /// Name of upvalue `slot`, if it is known.
    pub fn upvalue_name(&self, slot: u32) -> Option<&'a str> {
        self.upvalue_names.get(slot as usize).copied()
    }

    /// Looks up the name of the local that lives in `slot` at `addr`.
    ///
    /// `slot` is not a register number but an ordinal into the list of
    /// variables that are alive at `addr`, in declaration order. This mirrors
    /// LuaJIT's own `lj_debug_slotname`.
    ///
    /// With `alt_mode` a variable that dies exactly at `addr` is still
    /// considered alive, which recovers a few names that are otherwise lost.
    pub fn local_name(
        &self,
        addr: u32,
        mut slot: u32,
        alt_mode: bool,
    ) -> Option<&VariableInfo<'a>> {
        for info in &self.variable_info {
            if info.start_addr > addr {
                break;
            }
            if info.end_addr <= addr {
                if alt_mode && info.end_addr == addr {
                    if slot == 0 {
                        return Some(info);
                    }
                    slot -= 1;
                }
                continue;
            }
            if slot == 0 {
                return Some(info);
            }
            slot -= 1;
        }
        None
    }
}

// MARK: reading

/// Reads the debug information of a prototype, storing it in `alloc`.
///
/// `block_end` is the offset one past the end of the prototype's data, which is
/// exactly where the debug blob ends.
pub fn read<'a>(
    alloc: &'a Allocator,
    reader: &mut Reader<'_>,
    first_line: u32,
    num_lines: u32,
    num_bc: usize,
    num_uv: usize,
    block_end: usize,
) -> Result<&'a DebugInfo<'a>> {
    let line_width = if num_lines >= 65_536 {
        4
    } else if num_lines >= 256 {
        2
    } else {
        1
    };

    let mut addr_to_line = ArenaVec::with_capacity_in(num_bc + 1, &alloc);
    // Address 0 is the synthesised function header and never has a line.
    addr_to_line.push(0);
    for _ in 0..num_bc {
        let delta = reader.read_uint(line_width)?;
        addr_to_line.push(first_line.wrapping_add(delta));
    }

    let mut upvalue_names = ArenaVec::with_capacity_in(num_uv.min(reader.remaining()), &alloc);
    for _ in 0..num_uv {
        let name = reader.read_zstring()?;
        upvalue_names.push(arena_str(alloc, name));
    }

    let variable_info = read_variable_info(alloc, reader, block_end)?;

    Ok(alloc.alloc(DebugInfo {
        addr_to_line,
        upvalue_names,
        variable_info,
    }))
}

fn read_variable_info<'a>(
    alloc: &'a Allocator,
    reader: &mut Reader<'_>,
    block_end: usize,
) -> Result<ArenaVec<'a, VariableInfo<'a>>> {
    let mut infos = ArenaVec::new_in(&alloc);
    let mut last_addr = 0u32;

    while reader.pos() < block_end {
        let tag = reader.read_u8()?;
        let (kind, name) = if tag >= VARNAME__MAX {
            let suffix = reader.read_zstring()?;
            let mut name = String::with_capacity(suffix.len() + 1);
            name.push(tag as char);
            name.push_str(&String::from_utf8_lossy(suffix));
            (VarKind::Visible, alloc.alloc_str(&name))
        } else if tag == VARNAME_END {
            return Ok(infos);
        } else {
            let name = INTERNAL_VARNAMES[tag as usize].unwrap_or("<unknown>");
            (VarKind::Internal, name)
        };

        let start_addr = last_addr.wrapping_add(reader.read_uleb128()?);
        let end_addr = start_addr.wrapping_add(reader.read_uleb128()?);
        last_addr = start_addr;

        if reader.pos() > block_end {
            break;
        }

        infos.push(VariableInfo {
            start_addr,
            end_addr,
            kind,
            name,
        });
    }

    Err(Error::Malformed(
        "variable info is not terminated before the end of the prototype".into(),
    ))
}

// MARK: tests

#[cfg(test)]
mod tests {
    use super::*;

    fn info<'a>(alloc: &'a Allocator, start: u32, end: u32, name: &str) -> VariableInfo<'a> {
        VariableInfo {
            start_addr: start,
            end_addr: end,
            kind: VarKind::Visible,
            name: alloc.alloc_str(name),
        }
    }

    /// Builds debug info whose only content is the given variable list.
    fn debug<'a>(
        alloc: &'a Allocator,
        infos: impl IntoIterator<Item = VariableInfo<'a>>,
    ) -> DebugInfo<'a> {
        DebugInfo {
            variable_info: ArenaVec::from_iter_in(infos, &alloc),
            ..DebugInfo::new_in(alloc)
        }
    }

    #[test]
    fn local_lookup_counts_live_variables() {
        let alloc = Allocator::default();
        let debug = debug(
            &alloc,
            [
                info(&alloc, 2, 10, "a"),
                info(&alloc, 4, 6, "b"),
                info(&alloc, 7, 20, "c"),
            ],
        );

        // Only `a` is alive at address 3.
        assert_eq!(debug.local_name(3, 0, false).unwrap().name, "a");
        assert!(debug.local_name(3, 1, false).is_none());

        // Both `a` and `b` are alive at address 5, in declaration order.
        assert_eq!(debug.local_name(5, 0, false).unwrap().name, "a");
        assert_eq!(debug.local_name(5, 1, false).unwrap().name, "b");
        assert!(debug.local_name(5, 2, false).is_none());

        // `b` has died at address 6, so `c` takes its ordinal.
        assert_eq!(debug.local_name(7, 1, false).unwrap().name, "c");
    }

    #[test]
    fn alt_mode_accepts_variables_dying_at_the_address() {
        let alloc = Allocator::default();
        let debug = debug(&alloc, [info(&alloc, 2, 10, "a")]);
        assert!(debug.local_name(10, 0, false).is_none());
        assert_eq!(debug.local_name(10, 0, true).unwrap().name, "a");
    }

    #[test]
    fn line_lookup_is_total() {
        let alloc = Allocator::default();
        let debug = DebugInfo {
            addr_to_line: ArenaVec::from_iter_in([0, 3, 4], &&alloc),
            ..DebugInfo::new_in(&alloc)
        };
        assert_eq!(debug.line_for(1), 3);
        assert_eq!(debug.line_for(99), 0);
    }

    #[test]
    fn reads_visible_and_internal_names() {
        let alloc = Allocator::default();
        // "x" (two bytes: 'x' then NUL), live from 1 to 5, then the terminator.
        let data = [b'x', 0, 1, 4, VARNAME_END];
        let mut reader = Reader::new(&data);
        let infos = read_variable_info(&alloc, &mut reader, data.len()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "x");
        assert_eq!(infos[0].kind, VarKind::Visible);
        assert_eq!((infos[0].start_addr, infos[0].end_addr), (1, 5));
    }

    #[test]
    fn reads_internal_names() {
        let alloc = Allocator::default();
        let data = [1, 2, 3, VARNAME_END];
        let mut reader = Reader::new(&data);
        let infos = read_variable_info(&alloc, &mut reader, data.len()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "<index>");
        assert_eq!(infos[0].kind, VarKind::Internal);
    }

    #[test]
    fn extents_are_delta_encoded_on_the_start_address() {
        let alloc = Allocator::default();
        // Two variables: a@1..2 and b@1..3 (start deltas are relative).
        let data = [b'a', 0, 1, 1, b'b', 0, 0, 2, VARNAME_END];
        let mut reader = Reader::new(&data);
        let infos = read_variable_info(&alloc, &mut reader, data.len()).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(
            (infos[1].start_addr, infos[1].end_addr),
            (infos[0].start_addr, infos[0].start_addr + 2)
        );
    }

    #[test]
    fn rejects_unterminated_variable_info() {
        let alloc = Allocator::default();
        let data = [b'x', 0, 1, 4];
        let mut reader = Reader::new(&data);
        assert!(read_variable_info(&alloc, &mut reader, data.len()).is_err());
    }
}
