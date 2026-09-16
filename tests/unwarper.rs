//! Tests for control flow reconstruction.
//!
//! Every kind of control flow LuaJIT emits should come back out of the
//! unwarper as the statement it was written as. These tests cover each shape
//! on its own, so that a change to one of the passes cannot silently break
//! another.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use luajit_ripper::ast::nodes::Node;
use luajit_ripper::ast::traverse;
use support::*;

type NodeRef = Rc<RefCell<Node>>;

/// Builds the AST of a chunk and unwraps every function in it.
fn build_and_unwarp(
    chunk: &luajit_ripper::bytecode::Chunk,
) -> Result<NodeRef, luajit_ripper::Error> {
    let root = prepare(chunk)?;
    luajit_ripper::ast::unwarper::unwarp_chunk(&root)?;
    Ok(root)
}

/// The kinds of all nodes in the tree.
fn kinds(root: &NodeRef) -> Vec<&'static str> {
    traverse::walk(root)
        .iter()
        .map(|node| node.borrow().kind())
        .collect()
}

/// How many `return` statements without values the tree contains.
fn empty_returns(root: &NodeRef) -> usize {
    traverse::walk(root)
        .iter()
        .filter(|node| {
            matches!(&*node.borrow(), Node::Return(inner)
                if traverse::list_contents(&inner.returns).is_empty())
        })
        .count()
}

#[test]
fn straight_line_code_unwarps_completely() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local a = 1\nlocal b = a + 2\nlocal c = \"x\" .. b\nreturn c\n";
    let chunk = chunk_from_source(&luajit, "straight_line", source);
    let root = build_and_unwarp(&chunk).expect("straight line code must unwarp");

    let kinds = kinds(&root);
    assert!(
        !kinds.contains(&"block"),
        "no block should survive unwarping: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"conditional warp") && !kinds.contains(&"unconditional warp"),
        "no warp should survive unwarping: {kinds:?}"
    );

    let dumped = luajit_ripper::ast::dump::dump(&root);
    eprintln!("{dumped}");
    assert!(dumped.contains("assign"), "{dumped}");
    assert!(dumped.contains("return"), "{dumped}");
    assert!(dumped.contains("binary +"), "{dumped}");
    assert!(dumped.contains("binary .."), "{dumped}");
}

#[test]
fn trailing_empty_return_is_dropped() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    // The inner function ends with an implicit `return`, which LuaJIT encodes
    // as `RET0`; the decompiler drops it again.
    let source = "local function f()\n\tlocal a = 1\n\tlocal b = a\nend\nreturn f\n";
    let chunk = chunk_from_source(&luajit, "trailing_return", source);
    let root = build_and_unwarp(&chunk).expect("straight line code must unwarp");

    assert_eq!(
        empty_returns(&root),
        0,
        "{}",
        luajit_ripper::ast::dump::dump(&root)
    );
}

#[test]
fn a_branch_becomes_an_if_statement() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local a = 1\nif a > 0 then\n\ta = 2\nend\nreturn a\n";
    let chunk = chunk_from_source(&luajit, "branch", source);
    let root = build_and_unwarp(&chunk).expect("an if statement must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    let kinds = kinds(&root);
    assert!(
        !kinds.contains(&"block"),
        "no block should survive: {dumped}"
    );
    assert!(
        !kinds.contains(&"conditional warp"),
        "no branch should survive: {dumped}"
    );
    assert!(
        dumped.contains("if\n"),
        "the branch should be an if: {dumped}"
    );
    assert!(
        dumped.contains("binary >"),
        "the condition should have been rebuilt: {dumped}"
    );
}

#[test]
fn an_if_with_an_else_becomes_a_conditional_expression() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    // Both branches assign the same register, so the whole branch is the value
    // of that register and is written as an expression.
    let source = "local a = 1\nif a > 0 then\n\ta = 2\nelse\n\ta = 3\nend\nreturn a\n";
    let chunk = chunk_from_source(&luajit, "branch_else", source);
    let root = build_and_unwarp(&chunk).expect("the branch must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"if"),
        "nothing should be left: {dumped}"
    );
    assert!(
        dumped.contains("binary and") && dumped.contains("binary or"),
        "the branch should be a logical expression: {dumped}"
    );
}

#[test]
fn short_circuit_conditions_are_rebuilt() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local a = 1\nlocal b = a > 0 and a or 5\nreturn b\n";
    let chunk = chunk_from_source(&luajit, "short_circuit", source);
    let root = build_and_unwarp(&chunk).expect("the expression must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"block"),
        "no block should survive: {dumped}"
    );

    // `and` binds tighter than `or`, so the `and` has to sit inside the `or`.
    let indentation = |needle: &str| -> usize {
        dumped
            .lines()
            .find(|line| line.contains(needle))
            .map(|line| line.len() - line.trim_start().len())
            .unwrap_or_else(|| panic!("{needle} should be in the tree: {dumped}"))
    };
    assert!(
        indentation("binary or") < indentation("binary and"),
        "and should be nested inside or: {dumped}"
    );
}

/// A ternary inside a nested branch has to be found even when the register it
/// is stored in is read more than once.
///
/// The value of `old < cur and lang("add") or lang("reduce")` is computed into
/// a register, and that register is read both by the branch that tests it and
/// by the statement that stores it. Deciding that the first read is inlinable
/// is what turns the branch into an expression; keeping the definition because
/// of the second read ends up producing an `if` the later passes cannot
/// understand.
#[test]
fn nested_branches_with_a_ternary_unwarp() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = concat!(
        "local function refresh(self, kind)\n",
        "\tcall()\n",
        "\tif kind == 1 then\n",
        "\t\ta(self, false)\n",
        "\t\tself.text = \"a\"\n",
        "\telseif kind == 2 then\n",
        "\t\ta(self, false)\n",
        "\t\tlocal old = getOld()\n",
        "\t\tlocal cur = getCur()\n",
        "\t\tself.text = old < cur and lang(\"add\") or lang(\"reduce\")\n",
        "\t\tsetOld()\n",
        "\telseif kind == 3 then\n",
        "\t\ta(self, true)\n",
        "\t\tself.text = lang(\"unlock\")\n",
        "\telse\n",
        "\t\tprint(kind)\n",
        "\tend\n",
        "end\n",
        "return refresh\n",
    );
    let chunk = chunk_from_source(&luajit, "nested_branches", source);
    let root = build_and_unwarp(&chunk).expect("the branch must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"block"),
        "no block should survive: {dumped}"
    );
    assert!(
        !kinds(&root).contains(&"conditional warp"),
        "no branch should survive: {dumped}"
    );
    assert!(
        dumped.contains("binary and") && dumped.contains("binary or"),
        "the ternary should be a logical expression: {dumped}"
    );
}

#[test]
fn while_loop_is_rebuilt() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local a = 0\nwhile a < 10 do\n\ta = a + 1\nend\nreturn a\n";
    let chunk = chunk_from_source(&luajit, "while_loop", source);
    let root = build_and_unwarp(&chunk).expect("the loop must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"block"),
        "no block should survive: {dumped}"
    );
    assert!(dumped.contains("while"), "{dumped}");
    assert!(
        dumped.contains("binary <"),
        "the condition should be rebuilt: {dumped}"
    );
}

#[test]
fn numeric_for_loop_is_rebuilt() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local s = 0\nfor i = 1, 10 do\n\ts = s + i\nend\nreturn s\n";
    let chunk = chunk_from_source(&luajit, "numeric_for", source);
    let root = build_and_unwarp(&chunk).expect("the loop must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"block"),
        "no block should survive: {dumped}"
    );
    assert!(dumped.contains("numeric-for"), "{dumped}");
    assert!(
        dumped.contains("identifier i (Local)"),
        "the loop variable should be named: {dumped}"
    );
}

#[test]
fn iterator_for_loop_is_rebuilt() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local s = 0\nfor k, v in pairs(t) do\n\ts = s + v\nend\nreturn s\n";
    let chunk = chunk_from_source(&luajit, "iterator_for", source);
    let root = build_and_unwarp(&chunk).expect("the loop must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"block"),
        "no block should survive: {dumped}"
    );
    assert!(dumped.contains("iterator-for"), "{dumped}");
    assert!(
        dumped.contains("identifier k (Local)") && dumped.contains("identifier v (Local)"),
        "the loop variables should be named: {dumped}"
    );

    // The call that produces the values has to be part of the loop, not a
    // statement before it.
    let mut loops = 0;
    let mut controls_are_a_call = false;
    for node in traverse::walk(&root) {
        if let Node::IteratorFor(inner) = &*node.borrow() {
            loops += 1;
            let expressions = traverse::list_contents(&inner.expressions);
            controls_are_a_call = expressions.len() == 1
                && matches!(&*expressions[0].borrow(), Node::FunctionCall(_));
        }
    }
    assert_eq!(loops, 1, "{dumped}");
    assert!(controls_are_a_call, "{dumped}");
}

#[test]
fn repeat_until_loop_is_rebuilt() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local a = 0\nrepeat\n\ta = a + 1\nuntil a > 10\nreturn a\n";
    let chunk = chunk_from_source(&luajit, "repeat_until", source);
    let root = build_and_unwarp(&chunk).expect("the loop must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"block"),
        "no block should survive: {dumped}"
    );
    assert!(dumped.contains("repeat"), "{dumped}");
    assert!(dumped.contains("binary >"), "{dumped}");
}

#[test]
fn a_loop_can_be_left_with_break() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local a = 0\nwhile true do\n\ta = a + 1\n\tif a > 10 then\n\t\tbreak\n\tend\nend\nreturn a\n";
    let chunk = chunk_from_source(&luajit, "loop_break", source);
    let root = build_and_unwarp(&chunk).expect("the loop must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"block"),
        "no block should survive: {dumped}"
    );
    assert!(kinds(&root).contains(&"break"), "{dumped}");
    assert!(dumped.contains("while"), "{dumped}");
    assert!(dumped.contains("if"), "{dumped}");
}

#[test]
fn nested_loops_are_rebuilt() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local s = 0\nfor i = 1, 3 do\n\tfor j = 1, 3 do\n\t\ts = s + i * j\n\tend\nend\nreturn s\n";
    let chunk = chunk_from_source(&luajit, "nested_loops", source);
    let root = build_and_unwarp(&chunk).expect("the loops must unwarp");

    let dumped = luajit_ripper::ast::dump::dump(&root);
    assert!(
        !kinds(&root).contains(&"block"),
        "no block should survive: {dumped}"
    );
    assert_eq!(
        kinds(&root)
            .iter()
            .filter(|kind| **kind == "numeric for")
            .count(),
        2,
        "{dumped}"
    );
}
