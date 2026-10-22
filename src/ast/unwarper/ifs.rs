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

use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use super::super::ext::*;
use super::super::nodes::*;
use super::expressions::{compile_expression, invert};
use super::*;
use crate::error::{Error, Result};

// MARK: rebuilding ifs

/// Rebuilds the `if` statements of one block list.
pub fn unwarp_ifs<'a>(
    alloc: &'a Allocator,
    blocks: Vec<ArenaNode<'a>>,
    recovery: Recovery,
) -> Result<Vec<ArenaNode<'a>>> {
    unwarp_if_region(alloc, blocks, None, None, recovery)
}

/// Rebuilds the `if` statements of a region.
///
/// `top_end` is the block that ends the part of the list being processed, and
/// `topmost_end` the one that ends the whole outermost expression; branches
/// that leave through either are what tells an `if` from a body.
fn unwarp_if_region<'a>(
    alloc: &'a Allocator,
    blocks: Vec<ArenaNode<'a>>,
    _top_end: Option<ArenaNode<'a>>,
    topmost_end: Option<ArenaNode<'a>>,
    recovery: Recovery,
) -> Result<Vec<ArenaNode<'a>>> {
    let mut boundaries: Vec<(usize, usize)> = Vec::new();

    let mut start_index = 0usize;
    while start_index < blocks.len() - 1 {
        let start = blocks[start_index];
        let mut abort_loop = false;

        if let Some(warp) = start.block_warp() {
            let borrowed = warp.borrow();
            if borrowed.is_flow() {
                start_index += 1;
                continue;
            }

            if borrowed.is_jump() {
                // A jump opening with `slot = false` followed by a block that
                // only assigns `true` is a leftover from an expression in an
                // assignment, not a real branch.
                abort_loop = unreachable_true_assignment(start, &blocks, start_index);
            }
        }

        let Some((body, end, end_index)) = extract_if_body(start_index, &blocks, topmost_end)
        else {
            return Err(Error::Unsupported(
                "goto statements are not supported".to_string(),
            ));
        };

        let is_end = body
            .last()
            .and_then(|block| block.block_warp())
            .is_end_warp();

        // An empty `else` branch is not written.
        if is_end
            && body.len() == 1
            && body[0].block_contents().is_empty()
            && start
                .block_warp()
                .target(true)
                .same_optional_as(body[0].block_warp().target(true))
        {
            abort_loop = true;
        }

        if !abort_loop
            && let Err(error) = unwarp_if_statement(alloc, start, &body, end, end, recovery)
        {
            if std::env::var("LJR_DEBUG_IF").is_ok() {
                eprintln!(
                    "--- if start {} end {}",
                    start.block_index(),
                    end.block_index()
                );
                for block in &blocks {
                    eprintln!("{}", block.debug_trace_line());
                }
                let mut out = String::new();
                for block in &blocks {
                    out.push_str(&super::super::dump::dump(*block));
                }
                eprintln!("{out}");
            }

            // The region cannot be told apart from straight line code, so it is
            // written as such: the condition keeps its statements and the body
            // is dropped, which is what the block bookkeeping below does. The
            // block is marked so that the loss is visible in the output.
            if recovery == Recovery::Off {
                return Err(error);
            }
            start.mark_error();
        }

        if is_end {
            start.borrow_mut().block_set_end(alloc, false);
        } else {
            start.borrow_mut().block_set_flow_to(alloc, end);
        }

        boundaries.push((start_index, end_index.saturating_sub(1)));
        start_index = end_index;
    }

    Ok(remove_processed_blocks(&blocks, &boundaries))
}

// MARK: debug trace

impl<'a> ArenaNode<'a> {
    /// One line describing this block, its warp and where the warp goes, for
    /// the `LJR_DEBUG_IF` trace.
    fn debug_trace_line(self) -> String {
        let index = self.block_index();
        let range = self.block_range().unwrap_or((0, 0));
        let contents = self.block_contents().len();
        let detail = self
            .block_warp()
            .map(|warp| {
                let borrowed = warp.borrow();
                let targets = borrowed
                    .block_targets()
                    .iter()
                    .map(|target| (*target).block_index().to_string())
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
}

// MARK: unreachable branches

/// Whether the block assigns `false` and is followed by a block that assigns
/// `true` and is reached from nowhere else.
fn unreachable_true_assignment<'a>(
    start: ArenaNode<'a>,
    blocks: &[ArenaNode<'a>],
    start_index: usize,
) -> bool {
    let contents = start.block_contents();
    if contents.len() != 1 {
        return false;
    }
    let Node::Assignment(assignment) = &*contents[0].borrow() else {
        return false;
    };
    let destinations = assignment.destinations.list_contents();
    let expressions = assignment.expressions.list_contents();
    if destinations.len() != 1 {
        return false;
    }
    if start_index + 1 >= blocks.len() {
        return false;
    }
    if !expressions.first().is_primitive_false() {
        return false;
    }

    let next_block = blocks[start_index + 1];
    if next_block.block_warpins() != 0 {
        return false;
    }
    let contents = next_block.block_contents();
    if contents.len() != 1 {
        return false;
    }

    contents[0].is_assignment_and(|inner| inner.expressions.list_first().is_primitive_true())
}

// MARK: statement construction

fn unwarp_if_statement<'a>(
    alloc: &'a Allocator,
    start: ArenaNode<'a>,
    body: &[ArenaNode<'a>],
    end: ArenaNode<'a>,
    topmost_end: ArenaNode<'a>,
    recovery: Recovery,
) -> Result<()> {
    let (expression, body, false_target) =
        extract_if_expression(alloc, start, body, end, topmost_end)?;
    let if_node = ArenaNode::new(
        alloc,
        Node::If(ArenaBox::new_in(
            If {
                expression,
                then_block: ArenaNode::new_statements(alloc, []),
                elseifs: ArenaVec::from_iter_in([], &alloc),
                else_block: ArenaNode::new_statements(alloc, []),
                meta: Meta::default(),
            },
            &alloc,
        )),
    );

    if !false_target.same_as(end) && !false_target.same_as(topmost_end) {
        // There is an `else` branch.
        let else_start_index = body
            .iter()
            .position(|item| *item == false_target)
            .ok_or_else(|| internal("an else branch is not in the body"))?;

        let then_body = body[..else_start_index].to_vec();
        let else_body = body[else_start_index..].to_vec();

        let then_warp_out = then_body
            .last()
            .and_then(|block| block.block_warp())
            .ok_or_else(|| internal("an if body without a warp"))?;
        if !then_warp_out.is_jump() {
            return Err(internal("the then branch does not leave the if"));
        }

        let else_warp_out = else_body
            .last()
            .and_then(|block| block.block_warp())
            .ok_or_else(|| internal("an if body without a warp"))?;

        (*then_body.last().expect("checked above"))
            .borrow_mut()
            .block_set_end(alloc, false);
        let then_blocks = unwarp_if_region(
            alloc,
            then_body.clone(),
            then_body.last().copied(),
            Some(topmost_end),
            recovery,
        )?;

        (*else_body.last().expect("checked above"))
            .borrow_mut()
            .block_set_end(alloc, false);
        let else_blocks = unwarp_if_region(
            alloc,
            else_body.clone(),
            else_body.last().copied(),
            Some(topmost_end),
            recovery,
        )?;

        // An empty `then` with a full `else` is written with the condition
        // inverted, unless the `else` is itself an `elseif` chain.
        let then_is_empty = then_blocks.len() == 1
            && then_blocks[0].block_contents().len() == 1
            && then_blocks[0].block_contents()[0].is_no_op();
        let targets_match = then_warp_out
            .target(true)
            .same_optional_as(else_warp_out.target(true));

        let invert_branches = targets_match
            && then_is_empty
            && (else_blocks.len() != 1 || !else_blocks[0].block_last().is_if());

        let expression = match &*if_node.borrow() {
            Node::If(inner) => inner.expression,
            _ => unreachable!(),
        };
        let (expression, then_blocks, else_blocks) = if invert_branches {
            (invert(alloc, expression)?, else_blocks, Vec::new())
        } else {
            (expression, then_blocks, else_blocks)
        };

        if let Node::If(inner) = &mut *if_node.borrow_mut() {
            inner.expression = expression;
            inner.then_block = ArenaNode::new_statements(alloc, then_blocks);
            inner.else_block = ArenaNode::new_statements(alloc, else_blocks);
        }
    } else {
        let warp_out = body
            .last()
            .and_then(|block| block.block_warp())
            .ok_or_else(|| internal("an if body without a warp"))?;
        // The body leaves the branch one way or another; an unconditional
        // warp that jumps somewhere else was checked when the region was cut.
        if !warp_out.is_end_warp() && !warp_out.is_unconditional_warp() {
            return Err(internal("the if body does not leave the branch"));
        }

        (*body.last().expect("checked above"))
            .borrow_mut()
            .block_set_end(alloc, false);
        let then_blocks = unwarp_if_region(
            alloc,
            body.to_vec(),
            body.last().copied(),
            Some(topmost_end),
            recovery,
        )?;

        if let Node::If(inner) = &mut *if_node.borrow_mut() {
            inner.then_block = ArenaNode::new_statements(alloc, then_blocks);
        }
    }

    let mut contents = start.block_contents();
    contents.push(if_node);
    start.borrow_mut().block_set_contents(alloc, contents);
    Ok(())
}

// MARK: condition

/// Splits the region into the part that holds the condition and the body.
fn extract_if_expression<'a>(
    alloc: &'a Allocator,
    start: ArenaNode<'a>,
    body: &[ArenaNode<'a>],
    end: ArenaNode<'a>,
    topmost_end: ArenaNode<'a>,
) -> Result<(ArenaNode<'a>, Vec<ArenaNode<'a>>, ArenaNode<'a>)> {
    // Everything before the first block with contents belongs to the
    // condition. A body with no contents at all leaves the last block to be
    // the body, which is what ljd does.
    let mut split = body.len().saturating_sub(1);
    for (index, block) in body.iter().enumerate() {
        if !block.block_contents().is_empty() {
            split = index;
            break;
        }
    }
    if split >= body.len() {
        return Err(internal("an if without a body"));
    }

    let expression = std::iter::once(start)
        .chain(body[..split].iter().copied())
        .collect::<Vec<_>>();
    let body = body[split..].to_vec();

    // A jump out of the region marks the block after it as a way out, which is
    // how the end of the condition is found.
    let mut falses: HashSet<usize> = HashSet::new();
    for index in 0..body.len().saturating_sub(1) {
        let block = body[index];
        let Some(warp) = block.block_warp() else {
            continue;
        };
        let borrowed = warp.borrow();
        if !borrowed.is_jump() {
            continue;
        }
        let Some(target) = borrowed.target(false) else {
            continue;
        };
        if !target.same_as(end) && !target.same_as(topmost_end) {
            continue;
        }
        falses.insert(body[index + 1].key());
    }
    falses.insert(end.key());
    falses.insert(topmost_end.key());

    let (false_target, expression_end) = search_expression_end(&expression, &falses)?;

    let body = expression[expression_end..]
        .iter()
        .copied()
        .chain(body)
        .collect::<Vec<_>>();
    let expression = expression[..expression_end].to_vec();
    if expression.is_empty() {
        return Err(internal("an if without a condition"));
    }

    let true_end = *body
        .first()
        .ok_or_else(|| internal("an if without a body"))?;

    let expression =
        compile_expression(alloc, &expression, None, Some(true_end), Some(false_target))?;

    Ok((expression, body, false_target))
}

/// Finds where the condition ends: the first block that leaves through a way
/// out of the region.
fn search_expression_end<'a>(
    expression: &[ArenaNode<'a>],
    falses: &HashSet<usize>,
) -> Result<(ArenaNode<'a>, usize)> {
    let mut expression_end = None;
    let mut false_target: Option<ArenaNode<'a>> = None;

    for (index, block) in expression.iter().enumerate() {
        let Some(warp) = block.block_warp() else {
            continue;
        };
        let Some(target) = warp.target(true) else {
            continue;
        };
        if !falses.contains(&target.key()) {
            continue;
        }

        match false_target {
            None => {
                false_target = Some(target);
                expression_end = Some(index + 1);
            }
            Some(current) if current.same_as(target) => {
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
fn remove_processed_blocks<'a>(
    blocks: &[ArenaNode<'a>],
    boundaries: &[(usize, usize)],
) -> Vec<ArenaNode<'a>> {
    let mut remains: Vec<ArenaNode<'a>> = Vec::new();
    let mut last_end_index: i64 = -1;

    for (start, end) in boundaries {
        let up_to_index = if start == end { *start } else { start + 1 };
        let from = (last_end_index + 1) as usize;
        if from <= up_to_index && up_to_index <= blocks.len() {
            remains.extend(blocks[from..up_to_index].iter().copied());
        }
        last_end_index = *end as i64;
    }

    let from = (last_end_index + 1) as usize;
    if from <= blocks.len() {
        remains.extend(blocks[from..].iter().copied());
    }

    remains
}
