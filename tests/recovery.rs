//! Tests for the recovery mode of the unwarper.
//!
//! Not every control flow graph can be turned back into statements. The strict
//! mode reports the first region it cannot structure; the recovering mode marks
//! that region and carries on, so that the rest of the function is still
//! written. These tests pin both halves of that contract.

mod support;

use std::rc::Rc;

use luajit_ripper::ast::nodes::*;
use luajit_ripper::ast::traverse;
use luajit_ripper::bytecode::DebugInfo;
use luajit_ripper::lua::writer;

/// Builds a function definition holding `contents`.
fn function(contents: Vec<NodeRef>) -> NodeRef {
    node(Node::FunctionDefinition(Box::new(FunctionDefinition {
        arguments: identifiers(Vec::new()),
        statements: statements(contents),
        upvalues: Vec::new(),
        debug: Rc::new(DebugInfo::default()),
        instruction_count: 0,
        meta: Meta::default(),
    })))
}

/// An assignment of `value` to the local `name`.
fn assignment(name: &str, value: i32) -> NodeRef {
    let destination = node(Node::Identifier(Box::new(Identifier::new(
        IdentifierKind::Local,
        0,
        Meta::default(),
    ))));
    if let Node::Identifier(inner) = &mut *destination.borrow_mut() {
        inner.name = Some(name.to_string());
    }

    let source = node(Node::Constant(Box::new(Constant {
        value: ConstantValue::Integer(value),
        meta: Meta::default(),
    })));

    node(Node::Assignment(Box::new(Assignment {
        expressions: expressions(vec![source]),
        destinations: variables(vec![destination]),
        kind: AssignmentKind::Normal,
        meta: Meta::default(),
    })))
}

/// Renders a function definition with the default options.
fn render(function: &NodeRef) -> String {
    writer::write_function(function, &writer::Options::default()).expect("renderable")
}

#[test]
fn a_marked_statement_is_pointed_out_in_the_output() {
    let statement = assignment("a", 1);
    let root = function(vec![statement.clone()]);

    assert!(!has_error(&statement), "nothing has been marked yet");
    let clean = render(&root);
    assert!(
        !clean.contains("Decompilation error"),
        "an unmarked function must not carry the comment: {clean}"
    );

    mark_error(&statement);
    assert!(has_error(&statement), "the mark has to stick");

    let marked = render(&root);
    assert!(
        marked.contains("-- Decompilation error in this vicinity:"),
        "the mark has to reach the output: {marked}"
    );
    assert!(
        marked.contains("a = 1"),
        "the statement itself is still written: {marked}"
    );
}

#[test]
fn a_marker_survives_a_deep_clone() {
    // The passes copy subtrees when they have to place one in two spots, and a
    // mark on the original has to come along, or the loss it records would be
    // reported in only one of the two places.
    let statement = assignment("a", 1);
    mark_error(&statement);

    let copy = traverse::deep_clone(&statement);
    assert!(has_error(&copy), "the copy carries the mark");
    assert!(
        !Rc::ptr_eq(&statement, &copy),
        "the clone is a node of its own"
    );
}

#[test]
fn marking_a_list_or_a_primitive_is_harmless() {
    // List nodes and `Primitive` have nowhere to keep a mark; asking for one
    // must not panic.
    let list = statements(Vec::new());
    let primitive = primitive(PrimitiveKind::Nil);

    assert!(!has_error(&list));
    mark_error(&list);
    assert!(!has_error(&list));

    mark_error(&primitive);
    assert!(!has_error(&primitive));
}
