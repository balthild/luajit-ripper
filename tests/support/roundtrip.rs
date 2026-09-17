//! Comparing what a round trip reads and writes.
//!
//! There are two ways to check a decompiler against its own output, and they
//! answer different questions:
//!
//! * [`structural_differences`] compares two dumps of the same chunk. It is a
//!   measurement rather than an assertion, because compiling written source
//!   back legitimately changes the bytecode; see its documentation.
//! * [`round_trip`] feeds the written source back through the decompiler and
//!   asks whether the output settles. That is a claim strong enough to assert,
//!   and it is what the tests hold the decompiler to.

use luajit_ripper::bytecode::{Const, Constants, Ins, NumConst, Prototype};

use super::luajit;

// MARK: convergence

/// Compiles a source, decompiles it, and keeps decompiling what each pass wrote.
///
/// The first entry is the decompilation of the source, and every later entry the
/// decompilation of the source the one before it wrote. The list stops at the
/// first pass that writes what the one before it wrote, or after `passes`
/// entries, so a list of two or more ending in a repeat is one that settled.
///
/// Every pass compiles with debug information, so that the next one sees the
/// same variable names; without it the second pass would fall back to plain
/// register names and could never agree with the first.
pub fn round_trip(
    compiler: &str,
    name: &str,
    source: &str,
    passes: usize,
) -> Result<Vec<String>, String> {
    let dump = luajit::compile_source(compiler, name, source, true);
    let mut texts = vec![
        luajit_ripper::decompile(&dump, &Default::default())
            .map_err(|error| format!("pass 1: {error}"))?,
    ];

    for pass in 2..=passes {
        let last = texts.last().expect("there is always a first pass");
        let recompiled = luajit::compile_source(compiler, name, last, true);
        let text = luajit_ripper::decompile(&recompiled, &Default::default())
            .map_err(|error| format!("pass {pass}: {error}"))?;

        let done = text == *last;
        texts.push(text);
        if done {
            break;
        }
    }

    Ok(texts)
}

/// Runs a round trip with the local LuaJIT.
///
/// `None` means there is no LuaJIT to compile with, which is a reason to skip
/// rather than a failure; `Some(Err(..))` is a round trip that could not be run
/// to the end.
pub fn round_trips(name: &str, source: &str, passes: usize) -> Option<Result<Vec<String>, String>> {
    let compiler = luajit::luajit()?;
    Some(round_trip(&compiler, name, source, passes))
}

/// Whether the last pass wrote what the one before it wrote.
pub fn settled(texts: &[String]) -> bool {
    texts.len() >= 2 && texts[texts.len() - 1] == texts[texts.len() - 2]
}

/// How many round trips it took to settle, where `1` is the first one.
///
/// Only meaningful for a list that settled.
pub fn settled_after(texts: &[String]) -> usize {
    texts.len() - 1
}

/// The last two passes, which is where a round trip that never settled differs.
pub fn describe(texts: &[String]) -> String {
    match texts {
        [.., before, after] => first_difference(before, after),
        _ => format!("  only {} pass(es) ran", texts.len()),
    }
}

/// The first line that differs between two sources, with a little context.
pub fn first_difference(first: &str, second: &str) -> String {
    let left: Vec<&str> = first.lines().collect();
    let right: Vec<&str> = second.lines().collect();
    for (number, (a, b)) in left.iter().zip(right.iter()).enumerate() {
        if a != b {
            return format!("  line {}: {a:?} became {b:?}", number + 1);
        }
    }
    format!("  {} lines became {} lines", left.len(), right.len())
}

// MARK: structure

/// Lists where two dumps of the same chunk describe different programs.
///
/// This is what a round trip is checked with: the source the decompiler writes
/// is compiled again, and the dump that comes out of that is compared against
/// the one it was written from. The comparison is structural, so what is checked
/// is the program and not the layout:
///
/// * The constant tables are compared as sets, because a compiler is free to
///   order them however it likes.
/// * An operand that differs is looked up on both sides, so that the same
///   constant in a different slot counts as equal.
/// * Nested functions are compared one by one, by position.
///
/// What is *not* allowed to differ is the number and the shape of the
/// instructions, the parameters of a function, whether it is vararg, and the
/// values the constants hold.
///
/// This is a measurement tool, not an assertion. Rewriting a chunk into source
/// and compiling that source back legitimately produces different bytecode: a
/// table built field by field becomes a `TDUP` of a template when it is written
/// as a constructor, a `nil` written into a table literal is dropped, and
/// constant slots and register numbers move. About a fifth of a real world
/// corpus differs for reasons like these, so what this reports is a list to look
/// at, not a set of failures. What a round trip is held to is the weaker but
/// sound claim that it settles; see [`round_trip`].
pub fn structural_differences(original: &Prototype, recompiled: &Prototype) -> Vec<String> {
    let mut problems = Vec::new();
    compare(original, recompiled, "root", &mut problems);
    problems
}

/// Names a constant, so that two dumps can be compared without caring about the
/// order of their constant tables.
fn kgc_key(constant: &Const) -> String {
    match constant {
        Const::Child(child) => format!(
            "child({}, {}, {})",
            child.num_params,
            child.is_variadic(),
            child.instructions.len()
        ),
        Const::Table(table) => {
            let mut keys: Vec<String> = table
                .hash
                .iter()
                .map(|(key, value)| format!("{:?}={:?}", key, value))
                .collect();
            keys.sort();
            format!(
                "table(array={:?},hash=[{}])",
                table
                    .array
                    .iter()
                    .map(|value| format!("{value:?}"))
                    .collect::<Vec<_>>(),
                keys.join(",")
            )
        }
        other => format!("{other:?}"),
    }
}

fn knum_key(constant: NumConst) -> String {
    match constant {
        NumConst::Int(value) => format!("int:{value}"),
        // Bit patterns, so that the two spellings of a value that differs only
        // in how it was written still compare equal.
        NumConst::Float(value) => format!("float:{}", value.to_bits()),
    }
}

/// The constant tables of a prototype, in a form that can be looked up.
struct Consts<'a> {
    constants: &'a Constants<'a>,
}

impl<'a> Consts<'a> {
    fn new(constants: &'a Constants<'a>) -> Self {
        Consts { constants }
    }

    fn kgc(&self, index: u32) -> Option<String> {
        self.constants
            .kgc_at(index)
            .map(kgc_key)
            .filter(|_| (index as usize) < self.constants.kgc.len())
    }

    fn knum(&self, index: u32) -> Option<String> {
        self.constants
            .knum_at(index)
            .map(knum_key)
            .filter(|_| (index as usize) < self.constants.knum.len())
    }

    /// Whether two operands that differ are the same constant in a different
    /// place.
    fn same_operand(&self, a: u32, other: &Consts, b: u32) -> bool {
        if let (Some(a), Some(b)) = (self.kgc(a), other.kgc(b))
            && a == b
        {
            return true;
        }
        if let (Some(a), Some(b)) = (self.knum(a), other.knum(b))
            && a == b
        {
            return true;
        }
        false
    }
}

/// Collects the differences between two prototypes into `problems`.
fn compare(original: &Prototype, recompiled: &Prototype, path: &str, problems: &mut Vec<String>) {
    let original_consts = Consts::new(&original.constants);
    let recompiled_consts = Consts::new(&recompiled.constants);

    if original.num_params != recompiled.num_params {
        problems.push(format!(
            "{path}: {original_params} parameters became {recompiled_params}",
            original_params = original.num_params,
            recompiled_params = recompiled.num_params
        ));
    }
    if original.is_variadic() != recompiled.is_variadic() {
        problems.push(format!("{path}: the function stopped being vararg"));
    }

    if original.instructions.len() != recompiled.instructions.len() {
        problems.push(format!(
            "{path}: {original_count} instructions became {recompiled_count}",
            original_count = original.instructions.len(),
            recompiled_count = recompiled.instructions.len()
        ));
        return;
    }

    for (index, (left, right)) in original
        .instructions
        .iter()
        .zip(recompiled.instructions.iter())
        .enumerate()
    {
        if !same_instruction(left, right, &original_consts, &recompiled_consts) {
            problems.push(format!(
                "{path}: instruction {index}: {left:?} became {right:?}"
            ));
            if problems.len() > 8 {
                return;
            }
        }
    }

    // The constants themselves have to describe the same values.
    let mut original_keys: Vec<String> = original.constants.kgc.iter().map(kgc_key).collect();
    let mut recompiled_keys: Vec<String> = recompiled.constants.kgc.iter().map(kgc_key).collect();
    original_keys.sort();
    recompiled_keys.sort();
    if original_keys != recompiled_keys {
        problems.push(format!("{path}: the constants differ"));
    }

    let mut original_numbers: Vec<String> = original
        .constants
        .knum
        .iter()
        .map(|value| knum_key(*value))
        .collect();
    let mut recompiled_numbers: Vec<String> = recompiled
        .constants
        .knum
        .iter()
        .map(|value| knum_key(*value))
        .collect();
    original_numbers.sort();
    recompiled_numbers.sort();
    if original_numbers != recompiled_numbers {
        problems.push(format!("{path}: the number constants differ"));
    }

    let original_children: Vec<&Prototype> = original.children().collect();
    let recompiled_children: Vec<&Prototype> = recompiled.children().collect();
    if original_children.len() != recompiled_children.len() {
        problems.push(format!(
            "{path}: {original_count} nested functions became {recompiled_count}",
            original_count = original_children.len(),
            recompiled_count = recompiled_children.len()
        ));
        return;
    }

    for (index, (left, right)) in original_children
        .iter()
        .zip(recompiled_children.iter())
        .enumerate()
    {
        compare(left, right, &format!("{path}/function {index}"), problems);
    }
}

/// Whether two instructions describe the same operation.
fn same_instruction(
    left: &Ins,
    right: &Ins,
    original_consts: &Consts,
    recompiled_consts: &Consts,
) -> bool {
    if left.op != right.op || left.a != right.a || left.b != right.b {
        return false;
    }
    if left.cd == right.cd {
        return true;
    }

    original_consts.same_operand(left.cd, recompiled_consts, right.cd)
}
