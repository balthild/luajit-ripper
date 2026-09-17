//! A bytecode listing that reproduces LuaJIT's own `luajit -bl` output.
//!
//! The format is the one implemented by LuaJIT's `jit.bc` module, which means
//! the output of this module can be compared line by line against
//! `luajit -bl` for the same chunk. That makes it a very direct check of the
//! instruction decoder.
//!
//! ```text
//! -- BYTECODE -- file.lua:1-3
//! 0001    KSTR     1   0      ; "Hello, "
//! 0002    RET1     1   2
//! ```

use std::fmt::Write;

use crate::bytecode::constants::{Const, NumConst};
use crate::bytecode::opcodes::Mode;
use crate::bytecode::{Chunk, Ins, Opcode, Prototype};

// MARK: listing

/// Renders the whole chunk, nested functions included, the way `luajit -bl`
/// does.
pub fn dump(chunk: &Chunk) -> String {
    let mut out = String::new();
    write_chunk(&mut out, chunk);
    out
}

/// Renders the whole chunk into an existing buffer.
pub fn write_chunk(out: &mut String, chunk: &Chunk) {
    write_prototype(out, chunk, chunk.root);
}

/// Renders a single prototype and, before it, all of its children.
///
/// LuaJIT lists nested functions depth first, walking the constant array
/// backwards, so the innermost functions come first and the main chunk comes
/// last.
pub fn write_prototype(out: &mut String, chunk: &Chunk, prototype: &Prototype) {
    let children: Vec<&Prototype> = prototype.children().collect();
    for child in children.into_iter().rev() {
        write_prototype(out, chunk, child);
    }

    let _ = writeln!(
        out,
        "-- BYTECODE -- {}-{}",
        location(chunk, prototype.first_line),
        prototype.first_line + prototype.num_lines
    );

    let targets = branch_targets(prototype);
    for (index, instruction) in prototype.instructions.iter().enumerate().skip(1) {
        let pc = index as u32;
        write_instruction(out, chunk, prototype, pc, instruction, targets[index]);
    }
    out.push('\n');
}

/// Marks every address that some jump instruction branches to.
fn branch_targets(prototype: &Prototype) -> Vec<bool> {
    let mut targets = vec![false; prototype.instructions.len()];
    for (index, instruction) in prototype.instructions.iter().enumerate().skip(1) {
        if instruction.def().c != Mode::Jump {
            continue;
        }
        let target = index as i64 + i64::from(instruction.cd) - 0x7fff;
        if let Some(slot) = usize::try_from(target)
            .ok()
            .and_then(|t| targets.get_mut(t))
        {
            *slot = true;
        }
    }
    targets
}

fn write_instruction(
    out: &mut String,
    chunk: &Chunk,
    prototype: &Prototype,
    pc: u32,
    instruction: &Ins,
    is_target: bool,
) {
    let def = instruction.def();
    let prefix = if is_target { "=>" } else { "  " };
    // Operand `A` is only printed when the instruction has one. Upvalue names
    // never appear here, they go into the comment instead.
    let a_field = match def.a {
        Mode::None => String::new(),
        _ => instruction.a.to_string(),
    };
    let mut line = format!("{pc:04} {prefix} {:<6} {a_field:>3} ", def.name);

    // Jump operands are printed as an absolute target.
    if def.c == Mode::Jump {
        let target = i64::from(pc) + i64::from(instruction.cd) - 0x7fff;
        let _ = writeln!(line, "=> {target:04}");
        out.push_str(&line);
        return;
    }

    let abc = def.b != Mode::None;
    // The listing shows operands exactly as they are encoded. Constant indices
    // are stored negated, so they have to be negated again here; the `cd` field
    // holds the resolved index.
    let operand = if def.is_kgc_operand() {
        prototype.constants.kgc.len() as u32 - 1 - instruction.cd
    } else {
        instruction.cd
    };
    let operand = if abc { operand & 0xff } else { operand };
    if !abc && def.c == Mode::None {
        // No operands at all, e.g. the function headers.
        line.push('\n');
        out.push_str(&line);
        return;
    }

    let comment = comment_for(chunk, prototype, instruction);
    let comment = prepend_upvalue_name(comment, prototype, instruction);

    match (abc, comment) {
        (true, Some(comment)) => {
            let _ = writeln!(line, "{:>3} {:>3}  ; {comment}", instruction.b, operand);
        }
        (true, None) => {
            let _ = writeln!(line, "{:>3} {:>3}", instruction.b, operand);
        }
        (false, Some(comment)) => {
            let _ = writeln!(line, "{operand:>3}      ; {comment}");
        }
        (false, None) => {
            let literal = if def.c == Mode::LitS {
                instruction.lits()
            } else {
                operand as i32
            };
            let _ = writeln!(line, "{literal:>3}");
        }
    }
    out.push_str(&line);
}

/// Builds the trailing `; ...` comment of an instruction, if it has one.
fn comment_for(chunk: &Chunk, prototype: &Prototype, instruction: &Ins) -> Option<String> {
    match instruction.def().c {
        Mode::Str => {
            let constant = prototype.constants.kgc_at(instruction.cd)?;
            match constant {
                Const::Str(value) => Some(quote_string(value)),
                _ => None,
            }
        }
        Mode::Num => {
            let mut value = prototype.constants.knum_at(instruction.cd)?;
            if instruction.op == Opcode::TSETM {
                // `TSETM` stores the array size as a biased double.
                if let NumConst::Float(number) = value {
                    value = NumConst::Float(number - 4_503_599_627_370_496.0);
                }
            }
            Some(match value {
                NumConst::Int(number) => number.to_string(),
                NumConst::Float(number) => format_double(number),
            })
        }
        Mode::Func => {
            let child = prototype.constants.kgc_at(instruction.cd)?.as_child()?;
            Some(location(chunk, child.first_line))
        }
        Mode::Uv => prototype.upvalue_name(instruction.cd).map(str::to_string),
        _ => None,
    }
}

/// LuaJIT prefixes the comment of an `USET*` instruction with the name of the
/// upvalue it writes to, since operand `A` is not printed for the `ABC` form.
fn prepend_upvalue_name(
    comment: Option<String>,
    prototype: &Prototype,
    instruction: &Ins,
) -> Option<String> {
    if instruction.def().a != Mode::Uv {
        return comment;
    }
    let Some(name) = prototype.upvalue_name(instruction.a) else {
        return comment;
    };
    Some(match comment {
        Some(comment) => format!("{name} ; {comment}"),
        None => name.to_string(),
    })
}

/// Renders `source:line` the way LuaJIT does, with the directory stripped.
fn location(chunk: &Chunk, line: u32) -> String {
    format!("{}:{line}", short_source_name(chunk))
}

// MARK: names

/// Chunk names are `@path/file.lua`, `=literal` or raw; LuaJIT prints the
/// basename for `@` names.
pub fn short_source_name(chunk: &Chunk) -> String {
    let name = chunk.header.chunk_name();
    let name = name.strip_prefix('@').unwrap_or(name);
    match name.rsplit(['/', '\\']).next() {
        Some(tail) if !tail.is_empty() => tail.to_string(),
        _ => name.to_string(),
    }
}

// MARK: values

/// Renders a string constant the way LuaJIT's listing does.
///
/// LuaJIT escapes control characters and truncates the escaped form to 40
/// characters when the constant itself is longer than that.
fn quote_string(value: &[u8]) -> String {
    let mut escaped = Vec::with_capacity(value.len());
    for &byte in value {
        match byte {
            b'\n' => escaped.extend_from_slice(b"\\n"),
            b'\r' => escaped.extend_from_slice(b"\\r"),
            b'\t' => escaped.extend_from_slice(b"\\t"),
            0..=0x1f | 0x7f => {
                escaped.extend_from_slice(format!("\\{byte:03}").as_bytes());
            }
            _ => escaped.push(byte),
        }
    }

    if value.len() > 40 {
        escaped.truncate(40);
        format!("\"{}~\"", String::from_utf8_lossy(&escaped))
    } else {
        format!("\"{}\"", String::from_utf8_lossy(&escaped))
    }
}

/// Formats a double the way Lua's `tostring` does, i.e. with `%.14g`.
pub fn format_double(value: f64) -> String {
    if value.is_nan() {
        return if value.is_sign_negative() {
            "-nan"
        } else {
            "nan"
        }
        .to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_string();
    }

    const PRECISION: usize = 14;
    let scientific = format!("{:.*e}", PRECISION - 1, value);
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("the exponential format always contains an exponent");
    let exponent: i32 = exponent.parse().expect("exponent is an integer");

    if exponent >= PRECISION as i32 || exponent < -4 {
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }

    let decimals = (PRECISION as i32 - 1 - exponent).max(0) as usize;
    let fixed = format!("{value:.decimals$}");
    let fixed = if fixed.contains('.') {
        fixed.trim_end_matches('0').trim_end_matches('.')
    } else {
        fixed.as_str()
    };
    fixed.to_string()
}

// MARK: tests

#[cfg(test)]
mod tests {
    use oxc_allocator::{Allocator, ArenaVec};

    use super::*;
    use crate::bytecode::{Constants, DebugInfo, Header, HeaderFlags, Magic};

    #[test]
    fn doubles_are_formatted_like_lua() {
        assert_eq!(format_double(0.0), "0");
        assert_eq!(format_double(1.0), "1");
        assert_eq!(format_double(1.5), "1.5");
        assert_eq!(format_double(-0.5), "-0.5");
        assert_eq!(format_double(0.1), "0.1");
        assert_eq!(format_double(100.0), "100");
        assert_eq!(format_double(1e20), "1e+20");
        assert_eq!(format_double(1e-10), "1e-10");
        assert_eq!(format_double(123_456_789.0), "123456789");
        assert_eq!(format_double(9_007_199_254_740_992.0), "9.007199254741e+15");
        assert_eq!(format_double(f64::INFINITY), "inf");
    }

    #[test]
    fn short_source_name_strips_directories() {
        let alloc = Allocator::default();
        let header = Header {
            magic: Magic::LuaJit,
            version: 2,
            flags: HeaderFlags::default(),
            name: Some("@dir/sub/file.lua"),
        };
        let debug = alloc.alloc(DebugInfo::new_in(&alloc));
        let root = alloc.alloc(Prototype {
            flags: Default::default(),
            num_params: 0,
            frame_size: 0,
            first_line: 0,
            num_lines: 0,
            instructions: ArenaVec::new_in(&&alloc),
            constants: Constants::new_in(&alloc),
            debug,
        });
        let chunk = Chunk { header, root };
        assert_eq!(short_source_name(&chunk), "file.lua");
    }

    #[test]
    fn strings_are_quoted_and_truncated() {
        assert_eq!(quote_string(b"abc"), "\"abc\"");
        assert_eq!(quote_string(b"a\nb"), "\"a\\nb\"");
        assert_eq!(quote_string(&[0x01]), "\"\\001\"");

        // Long constants are clipped to 40 escaped characters plus `~"`.
        let long = quote_string(&[b'x'; 50]);
        assert_eq!(long, format!("\"{}~\"", "x".repeat(40)));

        // The length test uses the raw constant, not the escaped form.
        let escaped = quote_string(&[b'\n'; 50]);
        assert_eq!(escaped, format!("\"{}~\"", "\\n".repeat(20)));
    }
}
