//! Fixture tests: compile small Lua sources with the local LuaJIT and check
//! that this crate's parser understands the resulting dumps.
//!
//! The dumps are not committed: they are rebuilt for every run so that they
//! always match the LuaJIT that is actually installed. Set `LUAJIT` to point at
//! a specific binary. When LuaJIT is unavailable these tests do nothing.

mod support;

use std::process::Command;

use support::*;
/// Collects every prototype of a chunk, including nested ones.
fn all_prototypes(
    root: &'static luajit_ripper::bytecode::Prototype<'static>,
) -> Vec<&'static luajit_ripper::bytecode::Prototype<'static>> {
    let mut out = vec![root];
    let mut index = 0;
    while index < out.len() {
        let children: Vec<_> = out[index].children().collect();
        out.extend(children);
        index += 1;
    }
    out
}

#[test]
fn every_fixture_parses() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    for name in fixture_names() {
        let source = fixtures_dir().join(format!("{name}.lua"));

        for debug in [true, false] {
            let dump = compile(&luajit, &name, &source, debug);
            let chunk = parse_dump(&dump);

            assert_eq!(chunk.header.flags.stripped, !debug, "{name}: strip flag");
            assert_eq!(
                chunk.header.name.is_some(),
                debug,
                "{name}: only unstripped dumps carry a chunk name"
            );
            assert_eq!(
                chunk.root.debug.is_empty(),
                !debug,
                "{name}: debug information presence"
            );

            for prototype in all_prototypes(chunk.root) {
                assert_eq!(
                    prototype.instructions.len(),
                    prototype.body().len() + 1,
                    "{name}: the synthesised header is part of the instruction list"
                );
                if debug {
                    assert_eq!(
                        prototype.debug.addr_to_line.len(),
                        prototype.instructions.len(),
                        "{name}: the line map covers every instruction"
                    );
                    assert_eq!(
                        prototype.debug.upvalue_names.len(),
                        prototype.constants.upvalue_refs.len(),
                        "{name}: one name per upvalue reference"
                    );
                }
            }

            assert!(
                chunk.root.is_variadic(),
                "{name}: the main chunk is always a vararg function"
            );
        }
    }
}

#[test]
fn debug_information_matches_the_source() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local alpha = 1\nlocal beta = alpha + 1\nreturn beta\n";
    let dump = compile_source(&luajit, "debug_info", source, true);
    let chunk = parse_dump(&dump);

    let names: Vec<&str> = chunk
        .root
        .debug
        .variable_info
        .iter()
        .map(|info| info.name)
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    assert_eq!(chunk.root.line_for(1), 1);

    // The root prototype of a chunk has no upvalues.
    assert!(chunk.root.constants.upvalue_refs.is_empty());
}

#[test]
fn constant_kinds_are_decoded() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    use luajit_ripper::bytecode::{Const, NumConst};

    let source = "local a = 1\nlocal b = 1.5\nlocal c = \"text\"\nreturn a, b, c\n";
    let dump = compile_source(&luajit, "constants", source, true);
    let chunk = parse_dump(&dump);

    assert!(
        chunk
            .root
            .constants
            .kgc
            .iter()
            .any(|constant| constant == &Const::Str(b"text"[..].into())),
        "the string constant should be present"
    );
    assert!(
        chunk.root.constants.knum.contains(&NumConst::Float(1.5)),
        "the fraction constant should be present"
    );
}

#[test]
fn compiling_is_deterministic() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    // The chunk name is embedded in the dump, so both runs have to use the very
    // same source path for the bytes to be comparable.
    let source = "local t = { 1, 2, 3, key = \"value\", other = 2.5 }\nreturn t\n";
    let first = compile_source(&luajit, "deterministic", source, true);
    let second = compile_source(&luajit, "deterministic", source, true);
    assert_eq!(first, second, "compiling the same source twice must match");
}

#[test]
fn every_fixture_builds_a_control_flow_graph() {
    use luajit_ripper::ast::nodes::Node;

    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    for name in fixture_names() {
        let source = fixtures_dir().join(format!("{name}.lua"));
        for debug in [true, false] {
            let dump = compile(&luajit, &name, &source, debug);
            let chunk = parse_dump(&dump);
            let root = luajit_ripper::ast::builder::build(arena(), chunk)
                .unwrap_or_else(|error| panic!("{name} (debug={debug}): {error}"));

            let statements = match &*root.borrow() {
                Node::FunctionDefinition(definition) => definition.statements,
                other => panic!(
                    "{name}: expected a function definition, found {}",
                    other.kind()
                ),
            };

            let blocks = match &*statements.borrow() {
                Node::Statements(blocks) => blocks.iter().copied().collect::<Vec<_>>(),
                other => panic!("{name}: expected blocks, found {}", other.kind()),
            };
            assert!(!blocks.is_empty(), "{name}: no blocks");

            for (index, block) in blocks.iter().enumerate() {
                let Node::Block(block) = &*block.borrow() else {
                    panic!("{name}: expected a block");
                };
                assert_eq!(block.index as usize, index, "{name}: block index");
                assert!(
                    block.warp.is_some(),
                    "{name}: block {} has no warp",
                    block.index
                );
                assert!(
                    block.last_body_address <= block.last_address,
                    "{name}: block {} body runs past its end",
                    block.index
                );
            }
        }
    }
}

#[test]
fn bit_operators_use_the_current_opcode_numbering() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = fixtures_dir().join("bitops.lua");
    let dump = compile(&luajit, "bitops", &source, true);
    let chunk = parse_dump(&dump);

    assert!(
        chunk.header.flags.bitop,
        "a chunk using bit operators sets BCDUMP_F_BITOP"
    );

    // The dump must contain bit operator opcodes. With the pre-bitoperator
    // numbering these bytes would decode as `[JI]FUNC*` instructions, which are
    // never stored in a dump, so seeing them proves the modern numbering.
    let base = luajit_ripper::bytecode::Opcode::BNOT as u8;
    let mut seen: Vec<u8> = Vec::new();
    for prototype in all_prototypes(chunk.root) {
        for instruction in prototype.instructions.iter() {
            if instruction.op.is_bitop() {
                seen.push(instruction.op as u8 - base);
            }
        }
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen,
        (0..7).collect::<Vec<u8>>(),
        "all bit ops should appear"
    );
}

#[test]
fn listing_matches_luajit() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    for name in fixture_names() {
        let source = fixtures_dir().join(format!("{name}.lua"));

        // `luajit -bl` lists the freshly compiled function, which still has its
        // debug information, so our dump has to keep it as well.
        let dump = compile(&luajit, &name, &source, true);
        let chunk = parse_dump(&dump);
        let actual = luajit_ripper::listing::dump(chunk);

        let listed = Command::new(&luajit)
            .arg("-bl")
            .arg(&source)
            .output()
            .expect("luajit should be runnable");
        assert!(listed.status.success(), "{name}: luajit -bl failed");
        let expected = String::from_utf8_lossy(&listed.stdout);

        if actual != expected {
            for (number, (a, b)) in actual.lines().zip(expected.lines()).enumerate() {
                assert_eq!(a, b, "{name}: listing differs at line {}", number + 1);
            }
            assert_eq!(
                actual.lines().count(),
                expected.lines().count(),
                "{name}: listing length differs"
            );
        }
    }
}
