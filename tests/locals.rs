//! Tests for local variable recovery.
//!
//! LuaJIT keeps every value in a register; the source level variable a register
//! holds is only known from the debug information. These tests check that the
//! names come back, and that a debug free dump degrades gracefully instead of
//! inventing names.

mod support;

use luajit_ripper::ast::nodes::{AssignmentKind, IdentifierKind, Node};
use luajit_ripper::ast::{locals, traverse};
use support::*;

/// The names of all identifiers in the tree, without the synthetic ones.
fn names(root: NodeRef<'_>) -> Vec<String> {
    traverse::walk(root)
        .iter()
        .filter_map(|node| match &*node.borrow() {
            Node::Identifier(identifier) => Some(identifier.name_or_slot().into_owned()),
            _ => None,
        })
        .collect()
}

/// The names of the variables the local definitions introduce.
fn declared_locals(root: NodeRef<'_>) -> Vec<String> {
    traverse::walk(root)
        .iter()
        .filter_map(|node| match &*node.borrow() {
            Node::Assignment(inner) if inner.kind == AssignmentKind::LocalDefinition => Some(
                traverse::list_contents(inner.destinations)
                    .iter()
                    .filter_map(|destination| match &*destination.borrow() {
                        Node::Identifier(identifier) => {
                            Some(identifier.name_or_slot().into_owned())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

/// How many assignments declare a local.
fn local_definitions(root: NodeRef<'_>) -> usize {
    traverse::walk(root)
        .iter()
        .filter(|node| {
            matches!(&*node.borrow(), Node::Assignment(inner)
                if inner.kind == AssignmentKind::LocalDefinition)
        })
        .count()
}

fn build(chunk: &'static luajit_ripper::bytecode::Chunk<'static>) -> NodeRef<'static> {
    prepare(chunk).expect("the passes should run")
}

#[test]
fn local_names_are_recovered() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source =
        "local alpha = 1\nlocal beta = alpha + 2\nlocal gamma = \"x\" .. beta\nreturn gamma\n";
    let chunk = chunk_from_source(&luajit, "locals_names", source);
    let root = build(chunk);

    locals::mark_locals(root, false);
    locals::mark_local_definitions(arena(), root);

    assert_eq!(
        declared_locals(root),
        vec!["alpha", "beta", "gamma"],
        "the three locals should be named and declared in order"
    );

    // The concatenation needs a temporary register, which has no name in the
    // source and stays a slot until the slot handling passes inline it.
    let names = names(root);
    for expected in ["alpha", "beta", "gamma"] {
        assert!(
            names.iter().any(|name| name == expected),
            "{expected} should have been recovered, got {names:?}"
        );
    }
}

#[test]
fn locals_are_declared_where_they_start() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local alpha = 1\nlocal beta = alpha + 2\nalpha = beta\nreturn alpha\n";
    let chunk = chunk_from_source(&luajit, "locals_definitions", source);
    let root = build(chunk);

    locals::mark_locals(root, false);
    assert_eq!(
        local_definitions(root),
        0,
        "nothing is a declaration before the definitions are marked"
    );

    locals::mark_local_definitions(arena(), root);

    assert_eq!(
        local_definitions(root),
        2,
        "both locals should be declared exactly once"
    );
}

#[test]
fn a_dump_without_debug_information_keeps_its_slots() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local alpha = 1\nreturn alpha\n";
    let dump = compile_source(&luajit, "locals_stripped", source, false);
    let chunk = parse_dump(&dump);
    let root = build(chunk);

    locals::mark_locals(root, false);
    locals::mark_local_definitions(arena(), root);

    let names = names(root);
    assert!(
        names.iter().all(|name| name.starts_with("slot")),
        "there is nothing to recover from a stripped dump: {names:?}"
    );
}

/// The name of the first identifier of a given kind in the tree.
fn first_name_of_kind(root: NodeRef<'_>, kind: IdentifierKind) -> Option<String> {
    traverse::walk(root)
        .iter()
        .find_map(|node| match &*node.borrow() {
            Node::Identifier(identifier) if identifier.kind == kind => {
                Some(identifier.name_or_slot().into_owned())
            }
            _ => None,
        })
}

/// The names of the index of every numeric loop head.
fn numeric_loop_indices(root: NodeRef<'_>) -> Vec<String> {
    traverse::walk(root)
        .iter()
        .filter_map(|node| match &*node.borrow() {
            Node::NumericLoopWarp(warp) => Some(warp.index),
            _ => None,
        })
        .filter_map(|index| match &*index.borrow() {
            Node::Identifier(identifier) => Some(identifier.name_or_slot().into_owned()),
            _ => None,
        })
        .collect()
}

/// The names of the variables of every generic loop head.
fn iterator_loop_variables(root: NodeRef<'_>) -> Vec<String> {
    traverse::walk(root)
        .iter()
        .filter_map(|node| match &*node.borrow() {
            Node::IteratorWarp(warp) => Some(traverse::list_contents(warp.variables)),
            _ => None,
        })
        .flatten()
        .filter_map(|variable| match &*variable.borrow() {
            Node::Identifier(identifier) => Some(identifier.name_or_slot().into_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn loop_variables_are_named() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local sum = 0\nfor i = 1, 10 do\n\tsum = sum + i\nend\nreturn sum\n";
    let chunk = chunk_from_source(&luajit, "locals_numeric_for", source);
    let root = build(chunk);
    locals::mark_locals(root, false);
    assert_eq!(
        numeric_loop_indices(root),
        vec!["i"],
        "the numeric loop index should be named"
    );

    // The variables of a generic loop are named like any other register: the
    // loop header keeps them alive while the body is being walked.
    let source = "local t = {}\nfor k, v in pairs(t) do\n\tprint(k, v)\nend\nreturn t\n";
    let chunk = chunk_from_source(&luajit, "locals_generic_for", source);
    let root = build(chunk);
    locals::mark_locals(root, false);
    assert_eq!(
        iterator_loop_variables(root),
        vec!["k", "v"],
        "both loop variables should be named"
    );
}

#[test]
fn arguments_upvalues_and_nested_functions_are_named() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source =
        "local captured = 1\nlocal function f(a, b)\n\treturn a + b + captured\nend\nreturn f\n";
    let chunk = chunk_from_source(&luajit, "locals_upvalue", source);
    let root = build(chunk);
    locals::mark_locals(root, false);

    let names = names(root);
    for expected in ["captured", "f", "a", "b"] {
        assert!(
            names.iter().any(|name| name == expected),
            "{expected} should have been recovered, got {names:?}"
        );
    }
    assert_eq!(
        first_name_of_kind(root, IdentifierKind::Upvalue),
        Some("captured".to_string()),
        "the upvalue should be named from the upvalue table"
    );
}

#[test]
fn shadowed_locals_are_named_separately() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local a = 1\ndo\n\tlocal a = 2\n\tprint(a)\nend\nreturn a\n";
    let chunk = chunk_from_source(&luajit, "locals_shadow", source);
    let root = build(chunk);
    locals::mark_locals(root, false);
    locals::mark_local_definitions(arena(), root);

    assert_eq!(
        declared_locals(root),
        vec!["a", "a"],
        "both variables named a should be declared"
    );
}
