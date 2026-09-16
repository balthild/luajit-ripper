//! A compact textual dump of the AST.
//!
//! This is a debugging aid: it makes the shape of the tree (and of the control
//! flow graph) visible without going through the Lua writer.

use std::fmt::Write;

use super::nodes::*;
use super::traverse;

/// Renders the AST rooted at `root`.
pub fn dump(root: NodeRef<'_>) -> String {
    let mut out = String::new();
    write_node(&mut out, root, 0);
    out
}

/// Renders one node, recursively, one node per line.
pub fn write_node(out: &mut String, node: NodeRef<'_>, depth: usize) {
    let indent = "  ".repeat(depth);
    let borrowed = node.borrow();

    match &*borrowed {
        Node::Statements(items)
        | Node::Expressions(items)
        | Node::Variables(items)
        | Node::Identifiers(items)
        | Node::Records(items) => {
            let _ = writeln!(out, "{indent}{} [{}]", borrowed.kind(), items.len());
            for item in items {
                write_node(out, item, depth + 1);
            }
        }
        Node::Identifier(inner) => {
            let _ = writeln!(
                out,
                "{indent}identifier {} ({:?}{})",
                inner.name_or_slot(),
                inner.kind,
                inner.id.map(|id| format!(", id {id}")).unwrap_or_default()
            );
        }
        Node::Constant(inner) => {
            let _ = writeln!(out, "{indent}constant {:?}", inner.value);
        }
        Node::Primitive(inner) => {
            let _ = writeln!(out, "{indent}primitive {:?}", inner.kind);
        }
        Node::Break => out.push_str(&format!("{indent}break\n")),
        Node::NoOp(_) => out.push_str(&format!("{indent}no-op\n")),
        Node::Vararg => out.push_str(&format!("{indent}vararg\n")),
        Node::MulTres => out.push_str(&format!("{indent}multres\n")),
        Node::EndWarp(_) => out.push_str(&format!("{indent}end-warp\n")),
        Node::Assignment(inner) => {
            let _ = writeln!(out, "{indent}assign ({:?})", inner.kind);
            write_node(out, inner.destinations, depth + 1);
            write_node(out, inner.expressions, depth + 1);
        }
        Node::FunctionCall(inner) => {
            let _ = writeln!(
                out,
                "{indent}call{}",
                if inner.is_method { " (method)" } else { "" }
            );
            write_node(out, inner.function, depth + 1);
            write_node(out, inner.arguments, depth + 1);
        }
        Node::Return(inner) => {
            let _ = writeln!(out, "{indent}return");
            write_node(out, inner.returns, depth + 1);
        }
        Node::If(inner) => {
            let _ = writeln!(out, "{indent}if");
            write_node(out, inner.expression, depth + 1);
            write_node(out, inner.then_block, depth + 1);
            for branch in &inner.elseifs {
                write_node(out, branch, depth + 1);
            }
            if !traverse::block_contents(inner.else_block).is_empty()
                || matches!(&*inner.else_block.borrow(), Node::Statements(items) if !items.is_empty())
            {
                let _ = writeln!(out, "{indent}else");
                write_node(out, inner.else_block, depth + 1);
            }
        }
        Node::ElseIf(inner) => {
            let _ = writeln!(out, "{indent}elseif");
            write_node(out, inner.expression, depth + 1);
            write_node(out, inner.then_block, depth + 1);
        }
        Node::While(inner) => {
            let _ = writeln!(out, "{indent}while");
            write_node(out, inner.expression, depth + 1);
            write_node(out, inner.statements, depth + 1);
        }
        Node::RepeatUntil(inner) => {
            let _ = writeln!(out, "{indent}repeat");
            write_node(out, inner.statements, depth + 1);
            let _ = writeln!(out, "{indent}until");
            write_node(out, inner.expression, depth + 1);
        }
        Node::NumericFor(inner) => {
            let _ = writeln!(out, "{indent}numeric-for");
            write_node(out, inner.variable, depth + 1);
            write_node(out, inner.expressions, depth + 1);
            write_node(out, inner.statements, depth + 1);
        }
        Node::IteratorFor(inner) => {
            let _ = writeln!(out, "{indent}iterator-for");
            write_node(out, inner.identifiers, depth + 1);
            write_node(out, inner.expressions, depth + 1);
            write_node(out, inner.statements, depth + 1);
        }
        Node::FunctionDefinition(inner) => {
            let _ = writeln!(out, "{indent}function");
            write_node(out, inner.arguments, depth + 1);
            write_node(out, inner.statements, depth + 1);
        }
        Node::TableElement(inner) => {
            let _ = writeln!(out, "{indent}index");
            write_node(out, inner.table, depth + 1);
            write_node(out, inner.key, depth + 1);
        }
        Node::TableConstructor(inner) => {
            let _ = writeln!(out, "{indent}table");
            write_node(out, inner.array, depth + 1);
            write_node(out, inner.records, depth + 1);
        }
        Node::BinaryOperator(inner) => {
            let _ = writeln!(out, "{indent}binary {}", inner.kind.as_str());
            write_node(out, inner.left, depth + 1);
            write_node(out, inner.right, depth + 1);
        }
        Node::UnaryOperator(inner) => {
            let _ = writeln!(out, "{indent}unary {}", inner.kind.as_str());
            write_node(out, inner.operand, depth + 1);
        }
        Node::ArrayRecord(inner) => {
            let _ = writeln!(out, "{indent}array-record");
            write_node(out, inner.value, depth + 1);
        }
        Node::TableRecord(inner) => {
            let _ = writeln!(out, "{indent}table-record");
            write_node(out, inner.key, depth + 1);
            write_node(out, inner.value, depth + 1);
        }
        Node::Block(inner) => {
            let _ = writeln!(
                out,
                "{indent}block #{} [{}..{}] body<= {} warps {} loop={}",
                inner.index,
                inner.first_address,
                inner.last_address,
                inner.last_body_address,
                inner.warpins_count,
                inner.is_loop
            );
            for statement in &inner.contents {
                write_node(out, statement, depth + 1);
            }
            if let Some(warp) = &inner.warp {
                write_node(out, warp, depth + 1);
            }
        }
        Node::UnconditionalWarp(inner) => {
            let _ = writeln!(
                out,
                "{indent}warp {} -> {} uclo={}",
                match inner.kind {
                    UnconditionalWarpKind::Jump => "jump",
                    UnconditionalWarpKind::Flow => "flow",
                },
                inner
                    .target
                    .as_ref()
                    .map(|block| describe_block(block))
                    .unwrap_or_else(|| "?".to_string()),
                inner.is_uclo
            );
        }
        Node::ConditionalWarp(inner) => {
            let _ = writeln!(
                out,
                "{indent}warp if slot{} true={} false={}",
                inner
                    .slot
                    .map(|slot| slot.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                inner
                    .true_target
                    .as_ref()
                    .map(|block| describe_block(block))
                    .unwrap_or_else(|| "?".to_string()),
                inner
                    .false_target
                    .as_ref()
                    .map(|block| describe_block(block))
                    .unwrap_or_else(|| "?".to_string())
            );
            if let Some(condition) = &inner.condition {
                write_node(out, condition, depth + 1);
            }
        }
        Node::IteratorWarp(inner) => {
            let _ = writeln!(
                out,
                "{indent}warp iterator body={} out={}",
                inner
                    .body
                    .as_ref()
                    .map(|block| describe_block(block))
                    .unwrap_or_else(|| "?".to_string()),
                inner
                    .way_out
                    .as_ref()
                    .map(|block| describe_block(block))
                    .unwrap_or_else(|| "?".to_string())
            );
            write_node(out, inner.variables, depth + 1);
            write_node(out, inner.controls, depth + 1);
        }
        Node::NumericLoopWarp(inner) => {
            let _ = writeln!(
                out,
                "{indent}warp numeric-for body={} out={}",
                inner
                    .body
                    .as_ref()
                    .map(|block| describe_block(block))
                    .unwrap_or_else(|| "?".to_string()),
                inner
                    .way_out
                    .as_ref()
                    .map(|block| describe_block(block))
                    .unwrap_or_else(|| "?".to_string())
            );
            write_node(out, inner.index, depth + 1);
            write_node(out, inner.controls, depth + 1);
        }
    }
}

fn describe_block(block: NodeRef<'_>) -> String {
    match &*block.borrow() {
        Node::Block(inner) => format!("block#{}", inner.index),
        other => other.kind().to_string(),
    }
}
