//! Tests for the recovery mode of the unwarper.
//!
//! Not every control flow graph can be turned back into statements. The strict
//! mode reports the first region it cannot structure; the recovering mode marks
//! that region and carries on, so that the rest of the function is still
//! written. These tests pin both halves of that contract.

mod support;

use luajit_ripper::ast::ext::*;
use luajit_ripper::ast::nodes::*;
use luajit_ripper::bytecode::DebugInfo;
use luajit_ripper::lua::writer;
use oxc_allocator::{ArenaBox, ArenaVec};

/// Builds a function definition holding `contents`.
fn function(contents: Vec<ArenaNode<'static>>) -> ArenaNode<'static> {
    let alloc = support::arena();
    ArenaNode::new(
        alloc,
        Node::FunctionDefinition(ArenaBox::new_in(
            FunctionDefinition {
                arguments: ArenaNode::new_identifiers(alloc, Vec::new()),
                statements: ArenaNode::new_statements(alloc, contents),
                upvalues: ArenaVec::new_in(&alloc),
                debug: alloc.alloc(DebugInfo::new_in(alloc)),
                instruction_count: 0,
                meta: Meta::default(),
            },
            &alloc,
        )),
    )
}

/// An assignment of `value` to the local `name`.
fn assignment(name: &str, value: i32) -> ArenaNode<'static> {
    let alloc = support::arena();

    let destination = ArenaNode::new(
        alloc,
        Node::Identifier(ArenaBox::new_in(
            Identifier::new(alloc, IdentifierKind::Local, 0, Meta::default()),
            &alloc,
        )),
    );
    if let Node::Identifier(inner) = &mut *destination.borrow_mut() {
        inner.name = Some(alloc.alloc_str(name));
    }

    let source = ArenaNode::new(
        alloc,
        Node::Constant(ArenaBox::new_in(
            Constant {
                value: ConstantValue::Integer(value),
                meta: Meta::default(),
            },
            &alloc,
        )),
    );

    ArenaNode::new(
        alloc,
        Node::Assignment(ArenaBox::new_in(
            Assignment {
                expressions: ArenaNode::new_expressions(alloc, vec![source]),
                destinations: ArenaNode::new_variables(alloc, vec![destination]),
                kind: AssignmentKind::Normal,
                meta: Meta::default(),
            },
            &alloc,
        )),
    )
}

/// Renders a function definition with the default options.
fn render(function: ArenaNode<'static>) -> String {
    writer::write_function(support::arena(), function, &writer::Options::default())
        .expect("renderable")
}

#[test]
fn a_marked_statement_is_pointed_out_in_the_output() {
    let statement = assignment("a", 1);
    let root = function(vec![statement]);

    assert!(!statement.has_error(), "nothing has been marked yet");
    let clean = render(root);
    assert!(
        !clean.contains("Decompilation error"),
        "an unmarked function must not carry the comment: {clean}"
    );

    statement.mark_error();
    assert!(statement.has_error(), "the mark has to stick");

    let marked = render(root);
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
    statement.mark_error();

    let copy = statement.deep_clone(support::arena());
    assert!(copy.has_error(), "the copy carries the mark");
    assert!(!statement.same_as(copy), "the clone is a node of its own");
}

#[test]
fn marking_a_list_or_a_primitive_is_harmless() {
    // List nodes and `Primitive` have nowhere to keep a mark; asking for one
    // must not panic.
    let list = ArenaNode::new_statements(support::arena(), Vec::new());
    let primitive = ArenaNode::new_primitive(support::arena(), PrimitiveKind::Nil);

    assert!(!list.has_error());
    list.mark_error();
    assert!(!list.has_error());

    primitive.mark_error();
    assert!(!primitive.has_error());
}
