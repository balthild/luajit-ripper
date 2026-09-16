//! Rebuilds `if` statements from conditional branches.
//!
//! This is a port of the if half of ljd's `ast/unwarper.py`.
//!
//! A conditional branch in the graph is a block whose warp has two targets: a
//! `true` branch that falls through and a `false` branch that jumps. The block
//! list between the branch and wherever both branches meet is the body of the
//! `if`, which is what this pass collects.
//!
//! The condition itself is the same shape of region that
//! [`unwarp_expressions`](super::unwarp_expressions) recognises, so it is built
//! with the same matcher.

use std::collections::HashSet;
use std::rc::Rc;

use crate::error::{Error, Result};

use super::super::nodes::*;
use super::super::traverse;
use super::expressions::{compile_expression, invert};
use super::*;

/// Rebuilds the `if` statements of one block list.
pub fn unwarp_ifs(blocks: Vec<NodeRef>) -> Result<Vec<NodeRef>> {
    unwarp_if_region(blocks, None, None)
}

/// Rebuilds the `if` statements of a region.
///
/// `top_end` is the block that ends the part of the list being processed, and
/// `topmost_end` the one that ends the whole outermost expression; branches
/// that leave through either are what tells an `if` from a body.
fn unwarp_if_region(
    blocks: Vec<NodeRef>,
    _top_end: Option<NodeRef>,
    topmost_end: Option<NodeRef>,
) -> Result<Vec<NodeRef>> {
    let mut boundaries: Vec<(usize, usize)> = Vec::new();

    let mut start_index = 0usize;
    while start_index < blocks.len() - 1 {
        let start = blocks[start_index].clone();
        let mut abort_loop = false;

        if let Some(warp) = traverse::block_warp(&start) {
            let borrowed = warp.borrow();
            if is_flow(&borrowed) {
                start_index += 1;
                continue;
            }

            if is_jump(&borrowed) {
                // A jump opening with `slot = false` followed by a block that
                // only assigns `true` is a leftover from an expression in an
                // assignment, not a real branch.
                abort_loop = unreachable_true_assignment(&start, &blocks, start_index);
            }
        }

        let Some((body, end, end_index)) =
            extract_if_body(start_index, &blocks, topmost_end.as_ref())
        else {
            return Err(Error::Unsupported(
                "goto statements are not supported".to_string(),
            ));
        };

        let is_end = body
            .last()
            .and_then(traverse::block_warp)
            .is_some_and(|warp| matches!(&*warp.borrow(), Node::EndWarp(_)));

        // An empty `else` branch is not written.
        if is_end
            && body.len() == 1
            && traverse::block_contents(&body[0]).is_empty()
            && same_optional(
                traverse::block_warp(&start)
                    .and_then(|warp| get_target(&warp.borrow(), true))
                    .as_ref(),
                traverse::block_warp(&body[0])
                    .and_then(|warp| get_target(&warp.borrow(), true))
                    .as_ref(),
            )
        {
            abort_loop = true;
        }

        if !abort_loop && let Err(error) = unwarp_if_statement(&start, &body, &end, &end) {
            if std::env::var("LJR_DEBUG_IF").is_ok() {
                eprintln!(
                    "--- if start {} end {}",
                    block_index(&start),
                    block_index(&end)
                );
                for block in &blocks {
                    eprintln!("{}", debug_block(block));
                }
                let mut out = String::new();
                for block in &blocks {
                    out.push_str(&super::super::dump::dump(block));
                }
                eprintln!("{out}");
            }
            return Err(error);
        }

        if is_end {
            set_end(&start, false);
        } else {
            set_flow_to(&start, &end);
        }

        boundaries.push((start_index, end_index.saturating_sub(1)));
        start_index = end_index;
    }

    Ok(remove_processed_blocks(&blocks, &boundaries))
}

/// Whether the block opens with a constant that an expression left behind.
/// One line describing a block, its warp and where the warp goes.
fn debug_block(block: &NodeRef) -> String {
    let index = block_index(block);
    let range = traverse::block_range(block).unwrap_or((0, 0));
    let contents = traverse::block_contents(block).len();
    let detail = traverse::block_warp(block)
        .map(|warp| {
            let borrowed = warp.borrow();
            let targets = traverse::block_targets(&borrowed)
                .iter()
                .map(|target| block_index(target).to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!("{} -> {targets}", borrowed.kind())
        })
        .unwrap_or_else(|| "none".to_string());

    format!(
        "  block {index} [{}..{}] {contents} stmt {detail}",
        range.0, range.1
    )
}

fn unreachable_true_assignment(start: &NodeRef, blocks: &[NodeRef], start_index: usize) -> bool {
    let contents = traverse::block_contents(start);
    if contents.len() != 1 {
        return false;
    }
    let Node::Assignment(assignment) = &*contents[0].borrow() else {
        return false;
    };
    let destinations = traverse::list_contents(&assignment.destinations);
    let expressions = traverse::list_contents(&assignment.expressions);
    if destinations.len() != 1 {
        return false;
    }
    if start_index + 1 >= blocks.len() {
        return false;
    }
    let is_false = expressions.first().is_some_and(|expression| {
        matches!(&*expression.borrow(), Node::Primitive(primitive)
            if primitive.kind == PrimitiveKind::False)
    });
    if !is_false {
        return false;
    }

    let next_block = blocks[start_index + 1].clone();
    if warpins(&next_block) != 0 {
        return false;
    }
    let contents = traverse::block_contents(&next_block);
    if contents.len() != 1 {
        return false;
    }

    contents[0].borrow().kind().eq("assignment")
        && matches!(&*contents[0].borrow(), Node::Assignment(inner)
            if traverse::list_contents(&inner.expressions)
                .first()
                .is_some_and(|expression| matches!(&*expression.borrow(), Node::Primitive(primitive)
                    if primitive.kind == PrimitiveKind::True)))
}
fn unwarp_if_statement(
    start: &NodeRef,
    body: &[NodeRef],
    end: &NodeRef,
    topmost_end: &NodeRef,
) -> Result<()> {
    let (expression, body, false_target) = extract_if_expression(start, body, end, topmost_end)?;

    let if_node = node(Node::If(Box::new(If {
        expression,
        then_block: statements(Vec::new()),
        elseifs: Vec::new(),
        else_block: statements(Vec::new()),
        meta: Meta::default(),
    })));

    if !Rc::ptr_eq(&false_target, end) && !Rc::ptr_eq(&false_target, topmost_end) {
        // There is an `else` branch.
        let else_start_index = traverse::position(&body, &false_target)
            .ok_or_else(|| internal("an else branch is not in the body"))?;

        let then_body = body[..else_start_index].to_vec();
        let else_body = body[else_start_index..].to_vec();

        let then_warp_out = then_body
            .last()
            .and_then(traverse::block_warp)
            .ok_or_else(|| internal("an if body without a warp"))?;
        if !is_jump(&then_warp_out.borrow()) {
            return Err(internal("the then branch does not leave the if"));
        }

        let else_warp_out = else_body
            .last()
            .and_then(traverse::block_warp)
            .ok_or_else(|| internal("an if body without a warp"))?;

        set_end(then_body.last().expect("checked above"), false);
        let then_blocks = unwarp_if_region(
            then_body.clone(),
            then_body.last().cloned(),
            Some(topmost_end.clone()),
        )?;

        set_end(else_body.last().expect("checked above"), false);
        let else_blocks = unwarp_if_region(
            else_body.clone(),
            else_body.last().cloned(),
            Some(topmost_end.clone()),
        )?;

        let so_far = statements(then_blocks.clone());
        let _ = &so_far;

        // An empty `then` with a full `else` is written with the condition
        // inverted, unless the `else` is itself an `elseif` chain.
        let then_is_empty = then_blocks.len() == 1
            && traverse::block_contents(&then_blocks[0]).len() == 1
            && matches!(
                &*traverse::block_contents(&then_blocks[0])[0].borrow(),
                Node::NoOp(_)
            );
        let targets_match = same_optional(
            get_target(&then_warp_out.borrow(), true).as_ref(),
            get_target(&else_warp_out.borrow(), true).as_ref(),
        );

        let invert_branches = targets_match
            && then_is_empty
            && (else_blocks.len() != 1
                || !traverse::block_contents(&else_blocks[0])
                    .last()
                    .is_some_and(|last| matches!(&*last.borrow(), Node::If(_))));

        let (expression, then_blocks, else_blocks) = if invert_branches {
            let expression = match &*if_node.borrow() {
                Node::If(inner) => inner.expression.clone(),
                _ => unreachable!(),
            };
            (invert(&expression)?, else_blocks, Vec::new())
        } else {
            let expression = match &*if_node.borrow() {
                Node::If(inner) => inner.expression.clone(),
                _ => unreachable!(),
            };
            (expression, then_blocks, else_blocks)
        };

        if let Node::If(inner) = &mut *if_node.borrow_mut() {
            inner.expression = expression;
            inner.then_block = statements(then_blocks);
            inner.else_block = statements(else_blocks);
        }
    } else {
        let warp_out = body
            .last()
            .and_then(traverse::block_warp)
            .ok_or_else(|| internal("an if body without a warp"))?;
        // The body leaves the branch one way or another; an unconditional
        // warp that jumps somewhere else was checked when the region was cut.
        if !matches!(
            &*warp_out.borrow(),
            Node::EndWarp(_) | Node::UnconditionalWarp(_)
        ) {
            return Err(internal("the if body does not leave the branch"));
        }

        set_end(body.last().expect("checked above"), false);
        let then_blocks = unwarp_if_region(
            body.to_vec(),
            body.last().cloned(),
            Some(topmost_end.clone()),
        )?;

        if let Node::If(inner) = &mut *if_node.borrow_mut() {
            inner.then_block = statements(then_blocks);
        }
    }

    let mut contents = traverse::block_contents(start);
    contents.push(if_node);
    traverse::set_block_contents(start, contents);
    Ok(())
}

/// Splits the region into the part that holds the condition and the body.
fn extract_if_expression(
    start: &NodeRef,
    body: &[NodeRef],
    end: &NodeRef,
    topmost_end: &NodeRef,
) -> Result<(NodeRef, Vec<NodeRef>, NodeRef)> {
    // Everything before the first block with contents belongs to the
    // condition. A body with no contents at all leaves the last block to be
    // the body, which is what ljd does.
    let mut split = body.len().saturating_sub(1);
    for (index, block) in body.iter().enumerate() {
        if !traverse::block_contents(block).is_empty() {
            split = index;
            break;
        }
    }
    if split >= body.len() {
        return Err(internal("an if without a body"));
    }

    let expression = std::iter::once(start.clone())
        .chain(body[..split].iter().cloned())
        .collect::<Vec<_>>();
    let body = body[split..].to_vec();

    // A jump out of the region marks the block after it as a way out, which is
    // how the end of the condition is found.
    let mut falses: HashSet<usize> = HashSet::new();
    for index in 0..body.len().saturating_sub(1) {
        let block = body[index].clone();
        let Some(warp) = traverse::block_warp(&block) else {
            continue;
        };
        let borrowed = warp.borrow();
        if !is_jump(&borrowed) {
            continue;
        }
        let Some(target) = get_target(&borrowed, false) else {
            continue;
        };
        if !Rc::ptr_eq(&target, end) && !Rc::ptr_eq(&target, topmost_end) {
            continue;
        }
        falses.insert(node_key(&body[index + 1]));
    }
    falses.insert(node_key(end));
    falses.insert(node_key(topmost_end));

    let (false_target, expression_end) = search_expression_end(&expression, &falses)?;

    let body = expression[expression_end..]
        .iter()
        .cloned()
        .chain(body)
        .collect::<Vec<_>>();
    let expression = expression[..expression_end].to_vec();
    if expression.is_empty() {
        return Err(internal("an if without a condition"));
    }

    let true_end = body
        .first()
        .cloned()
        .ok_or_else(|| internal("an if without a body"))?;

    let expression = compile_expression(&expression, None, Some(&true_end), Some(&false_target))?;

    Ok((expression, body, false_target))
}

/// Finds where the condition ends: the first block that leaves through a way
/// out of the region.
fn search_expression_end(
    expression: &[NodeRef],
    falses: &HashSet<usize>,
) -> Result<(NodeRef, usize)> {
    let mut expression_end = None;
    let mut false_target: Option<NodeRef> = None;

    for (index, block) in expression.iter().enumerate() {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let Some(target) = get_target(&warp.borrow(), true) else {
            continue;
        };
        if !falses.contains(&node_key(&target)) {
            continue;
        }

        match &false_target {
            None => {
                false_target = Some(target);
                expression_end = Some(index + 1);
            }
            Some(current) if Rc::ptr_eq(current, &target) => {
                expression_end = Some(index + 1);
            }
            Some(_) => break,
        }
    }

    match (false_target, expression_end) {
        (Some(false_target), Some(expression_end)) => Ok((false_target, expression_end)),
        _ => Err(internal("an if condition does not leave through a branch")),
    }
}

/// Drops the blocks that became part of an `if`.
fn remove_processed_blocks(blocks: &[NodeRef], boundaries: &[(usize, usize)]) -> Vec<NodeRef> {
    let mut remains: Vec<NodeRef> = Vec::new();
    let mut last_end_index: i64 = -1;

    for (start, end) in boundaries {
        let up_to_index = if start == end { *start } else { start + 1 };
        let from = (last_end_index + 1) as usize;
        if from <= up_to_index && up_to_index <= blocks.len() {
            remains.extend(blocks[from..up_to_index].iter().cloned());
        }
        last_end_index = *end as i64;
    }

    let from = (last_end_index + 1) as usize;
    if from <= blocks.len() {
        remains.extend(blocks[from..].iter().cloned());
    }

    remains
}
