//! Tests for the temporary register elimination.
//!
//! The compiler moves almost every computed value through a register; these
//! tests pin down which of those moves the pass undoes, and — just as
//! important — which ones it has to leave alone.

mod support;

use luajit_ripper::ast::ext::*;
use luajit_ripper::ast::nodes::{IdentifierKind, Node};
use support::ArenaNode;

/// Every identifier that is still a plain register.
fn remaining_slots(root: ArenaNode<'_>) -> Vec<String> {
    root.iter_nodes()
        .filter_map(|node| match &*node.borrow() {
            Node::Identifier(identifier) if identifier.kind == IdentifierKind::Slot => {
                Some(identifier.name_or_slot().into_owned())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn temporaries_are_inlined_into_the_value_they_hold() {
    let Some(luajit) = support::luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source =
        "local alpha = 1\nlocal beta = alpha + 2\nlocal gamma = \"x\" .. beta\nreturn gamma\n";
    let chunk = support::chunk_from_source(&luajit, "slots_inline", source);
    let root = support::prepare(chunk).expect("the passes should run");

    assert!(
        remaining_slots(root).is_empty(),
        "every register should have been inlined: {:?}",
        remaining_slots(root)
    );

    let dumped = luajit_ripper::ast::dump::dump(root);
    // The concatenation is built through two scratch registers, so it is the
    // best evidence that the values really replaced the registers they went
    // through.
    assert!(
        dumped.contains("binary .."),
        "the concatenation should have survived: {dumped}"
    );
}

#[test]
fn a_table_constructor_still_gets_its_writes() {
    let Some(luajit) = support::luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    // The constructor is read before it is written to, so the writes cannot be
    // folded into it: `f(t)` must see the empty table.
    let source = "local t = {}\nf(t)\nt.x = 1\nreturn t\n";
    let chunk = support::chunk_from_source(&luajit, "slots_ctor_read_first", source);
    let root = support::prepare(chunk).expect("the passes should run");

    let mut kept = 0;
    for node in root.iter_nodes() {
        if let Node::Assignment(inner) = &*node.borrow()
            && inner
                .destinations
                .is_variables_and(|items| items.iter().any(|item| item.is_table_element()))
        {
            kept += 1;
        }
    }

    assert_eq!(kept, 1, "the write to the table has to stay a statement");
}

#[test]
fn the_registers_of_a_generic_loop_become_its_variables() {
    let Some(luajit) = support::luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local t = {}\nfor k, v in pairs(t) do\n\tprint(k, v)\nend\nreturn t\n";
    let chunk = support::chunk_from_source(&luajit, "slots_iterator", source);
    let root = support::prepare(chunk).expect("the passes should run");

    let warps: Vec<ArenaNode> = root
        .iter_nodes()
        .filter(|node| node.is_iterator_warp())
        .collect();
    assert_eq!(warps.len(), 1, "the loop should still have its header");

    let (variables, controls) = match &*warps[0].borrow() {
        Node::IteratorWarp(inner) => (
            inner.variables.list_contents(),
            inner.controls.list_contents(),
        ),
        _ => unreachable!(),
    };

    let names: Vec<String> = variables
        .iter()
        .filter_map(|variable| match &*variable.borrow() {
            Node::Identifier(identifier) => Some(identifier.name_or_slot().into_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["k", "v"]);

    // The three registers the iterator needs are replaced by the call that
    // produced them.
    assert_eq!(controls.len(), 1, "the loop should iterate over one value");
    assert!(
        controls[0].is_function_call(),
        "the loop should iterate over the call itself"
    );
}

#[test]
fn multiple_results_stay_a_single_call() {
    let Some(luajit) = support::luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source =
        "local function produce()\n\treturn 1, 2\nend\nlocal a, b = produce()\nreturn a, b\n";
    let chunk = support::chunk_from_source(&luajit, "slots_multres", source);
    let root = support::prepare(chunk).expect("the passes should run");

    let mut calls = 0;
    for node in root.iter_nodes() {
        if let Node::Assignment(inner) = &*node.borrow()
            && inner.destinations.list_contents().len() == 2
        {
            calls += 1;
            assert!(
                inner.expressions.list_first().is_function_call(),
                "the call has to stay the value of the assignment"
            );
        }
    }

    assert_eq!(calls, 1, "both results should come from one assignment");
    assert!(
        !root.iter_nodes().any(|node| node.is_mul_tres()),
        "no marker for multiple results should be left"
    );
}

#[test]
fn a_call_that_keeps_its_table_does_not_become_a_method() {
    let Some(luajit) = support::luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    // `f(t)` passes the table as an ordinary argument, so `t` cannot be moved
    // into the call as its receiver.
    let source = "local t = {}\nlocal function f(x)\n\treturn x\nend\nreturn f(t)\n";
    let chunk = support::chunk_from_source(&luajit, "slots_not_a_method", source);
    let root = support::prepare(chunk).expect("the passes should run");

    for node in root.iter_nodes() {
        if let Node::FunctionCall(inner) = &*node.borrow() {
            assert!(
                !inner.is_method,
                "an ordinary argument must not turn into a receiver"
            );
        }
    }
}
