//! Rebuilds structured control flow out of the block graph.
//!
//! This is a port of ljd's `ast/unwarper.py`. It runs as a sequence of steps,
//! each of which rewrites every statement list in the tree. The final step
//! glues the remaining blocks together into a flat statement list.
//!
//! A graph the passes cannot structure is reported as an [`Error::Internal`],
//! the same way the original asserts on it.

mod expressions;
mod ifs;
mod loops;

pub use expressions::unwarp_expressions;
pub use ifs::unwarp_ifs;
pub use loops::{fix_loops, unwarp_loops};
use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use super::ext::*;
use super::nodes::*;
use super::slotworks;
use crate::error::{Error, Result};

// MARK: unwarper

/// How the passes react to a graph they cannot structure.
///
/// Some control flow graphs cannot be turned back into statements: a jump does
/// not record which construct produced it, so a branch whose two arms only meet
/// again through a chain of empty jumps has no unambiguous shape. The original
/// decompiler is asked to carry on in that case, and this is what selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Recovery {
    /// Stop at the first region that cannot be structured and report it.
    #[default]
    Off,
    /// Point the region out and keep going.
    ///
    /// The region is written as straight line code and the node it starts at is
    /// marked, so that the writer can say so. What is recovered this way is
    /// usually right but not always: a branch that could not be told apart
    /// loses the arm that was not taken.
    On,
}

/// Rebuilds structured control flow for one function definition.
///
/// Only the statement lists that belong to this function are rewritten; the
/// functions nested inside it are left for their own call.
pub fn unwarp<'a>(alloc: &'a Allocator, function: ArenaNode<'a>, recovery: Recovery) -> Result<()> {
    recoverable(recovery, || {
        run_step(alloc, function, |blocks| fix_loops(alloc, blocks, false))
    })?;
    recoverable(recovery, || {
        run_step(alloc, function, |blocks| fix_loops(alloc, blocks, true))
    })?;

    // Under some conditions unwarping expressions makes new assignments become
    // expressions themselves; running it twice saves the bookkeeping that would
    // otherwise be needed to notice.
    recoverable(recovery, || {
        run_step(alloc, function, |blocks| {
            unwarp_expressions(alloc, blocks, recovery)
        })
    })?;
    recoverable(recovery, || {
        run_step(alloc, function, |blocks| {
            unwarp_expressions(alloc, blocks, recovery)
        })
    })?;

    recoverable(recovery, || {
        run_step(alloc, function, |blocks| unwarp_loops(alloc, blocks, false))
    })?;
    recoverable(recovery, || {
        run_step(alloc, function, |blocks| unwarp_loops(alloc, blocks, true))
    })?;
    recoverable(recovery, || {
        run_step(alloc, function, |blocks| {
            unwarp_ifs(alloc, blocks, recovery)
        })
    })?;
    recoverable(recovery, || {
        run_step(alloc, function, |blocks| cleanup_ast(alloc, blocks))
    })?;

    // From here on the tree has to be sound for the output to be usable, so a
    // failure is reported even when recovering: leaving blocks behind would
    // write bytecode structure into the source.
    glue_flows(alloc, function)?;
    trim_redundant_returns(alloc, function)?;

    // With everything rebuilt, the calls that pass their receiver as the first
    // argument can be written as method calls.
    slotworks::simplify_ast(alloc, function, &mut |_| {});

    Ok(())
}

/// Runs a step, recording the failure instead of reporting it when recovering.
fn recoverable(recovery: Recovery, step: impl FnOnce() -> Result<()>) -> Result<()> {
    match step() {
        Ok(()) => Ok(()),
        Err(error) => match recovery {
            Recovery::Off => Err(error),
            Recovery::On => Ok(()),
        },
    }
}

/// Rebuilds the control flow of every function of a chunk.
///
/// The nested functions are done first, so that a function is finished before
/// the one that contains it is looked at.
pub fn unwarp_chunk<'a>(
    alloc: &'a Allocator,
    root: ArenaNode<'a>,
    recovery: Recovery,
) -> Result<()> {
    let mut functions = root.functions();
    functions.reverse();

    for function in functions {
        unwarp(alloc, function, recovery)?;
    }

    Ok(())
}

/// Applies a step to every statement list of a function and renumbers the
/// blocks afterwards, which may have moved.
fn run_step<'a>(
    alloc: &'a Allocator,
    root: ArenaNode<'a>,
    step: impl Fn(Vec<ArenaNode<'a>>) -> Result<Vec<ArenaNode<'a>>>,
) -> Result<()> {
    for statements in root.own_statement_lists() {
        let contents = statements.list_contents();
        let result = step(contents)?;
        statements.borrow_mut().list_set_contents(alloc, result);
    }

    for statements in root.own_statement_lists() {
        for (index, node) in statements.list_contents().iter().enumerate() {
            if let Node::Block(block) = &mut *node.borrow_mut() {
                block.index = index as u32;
            }
        }
    }

    Ok(())
}

/// A state the pass cannot make sense of, which means a limitation of the
/// decompiler rather than a problem with the input.
fn internal(message: &str) -> Error {
    Error::Internal(message.to_string())
}

/// Points every warp that led to `original` at `replacement` instead.
///
/// The false branch of a conditional is only rewritten when it jumps forward,
/// unless `add_jumpback` says otherwise: a backward false branch is a loop, and
/// retargeting it would change what the loop does.
fn replace_targets<'a>(
    alloc: &'a Allocator,
    blocks: &[ArenaNode<'a>],
    original: ArenaNode<'a>,
    replacement: ArenaNode<'a>,
    add_jumpback: bool,
) {
    let _ = alloc;
    for block in blocks {
        let Some(warp) = block.block_warp() else {
            continue;
        };

        match &mut *warp.borrow_mut() {
            Node::UnconditionalWarp(inner) => {
                if inner.target.same_as(original) {
                    inner.target = Some(replacement);
                }
            }
            Node::ConditionalWarp(inner) => {
                if inner.true_target.same_as(original) {
                    inner.true_target = Some(replacement);
                }
                let target_last = inner
                    .false_target
                    .and_then(|target| target.block_range())
                    .map(|(_, last)| last);
                let block_last = block.block_range().map(|(_, last)| last);
                let forward = match (target_last, block_last) {
                    (Some(target), Some(current)) => target > current,
                    _ => false,
                };
                if inner.false_target.same_as(original) && (forward || add_jumpback) {
                    inner.false_target = Some(replacement);
                }
            }
            Node::IteratorWarp(inner) => {
                if inner.way_out.same_as(original) {
                    inner.way_out = Some(replacement);
                }
                if inner.body.same_as(original) {
                    inner.body = Some(replacement);
                }
            }
            Node::NumericLoopWarp(inner) => {
                if inner.way_out.same_as(original) {
                    inner.way_out = Some(replacement);
                }
                if inner.body.same_as(original) {
                    inner.body = Some(replacement);
                }
            }
            _ => {}
        }
    }
}

/// Where the region that starts at `blocks[0]` branches out.
///
/// The answer is the furthest block any of them can reach before the chain of
/// warps stops.
fn find_branching_end<'a>(
    blocks: &[ArenaNode<'a>],
    topmost_end: Option<ArenaNode<'a>>,
) -> Option<ArenaNode<'a>> {
    let mut end = *blocks.first()?;

    for block in blocks {
        let Some(warp) = block.block_warp() else {
            continue;
        };
        let target = warp.target(true);

        let Some(target) = target else {
            return Some(*block);
        };

        if warp.is_unconditional_warp() && target.same_as(end) {
            return Some(end);
        }

        if target.block_index() > end.block_index() {
            end = target;
        }
    }

    let _ = topmost_end;
    Some(end)
}

impl<'a> ArenaNode<'a> {
    /// Whether the last thing this block does is test a constant.
    ///
    /// `b = false and (x or y)` is one of those: the compiler materialises the
    /// constant into a register, and the branch afterwards tests that register.
    pub(crate) fn contains_primitive_condition(self) -> bool {
        let contents = self.block_contents();
        if contents.is_empty() {
            return false;
        }

        let last_index = contents.len() - 1;
        for i in 0..last_index {
            let content = contents[last_index - i];
            let borrowed = content.borrow();
            let Node::Assignment(assignment) = &*borrowed else {
                continue;
            };

            let (Some(destination), Some(expression)) = (
                assignment.destinations.list_last(),
                assignment.expressions.list_last(),
            ) else {
                continue;
            };

            if !expression.is_primitive() {
                continue;
            }

            if destination.is_identifier_and(|identifier| identifier.name.is_none()) {
                // A primitive condition inside the block's own expression, as in
                // `b = false and (x or y)`.
                return true;
            }

            // A primitive that follows an ordinary assignment, as in
            // `a = nil; b = (false and x) or y`.
            return !matches!(
                self.block_warp().map(|warp| warp.borrow().kind()),
                Some("conditional warp")
            );
        }

        false
    }
}

// MARK: body extraction

/// The region that starts at `start_index`: its body, its end, and the position
/// of that end.
fn extract_if_body<'a>(
    start_index: usize,
    blocks: &[ArenaNode<'a>],
    topmost_end: Option<ArenaNode<'a>>,
) -> Option<(Vec<ArenaNode<'a>>, ArenaNode<'a>, usize)> {
    let search = if start_index > 0 {
        blocks[start_index..].to_vec()
    } else {
        blocks.to_vec()
    };

    let end = find_branching_end(&search, topmost_end)?;

    let end_index = match blocks.iter().position(|item| *item == end) {
        Some(index) => index,
        None => {
            if topmost_end.same_as(end) {
                blocks.len()
            } else {
                return None;
            }
        }
    };

    let body = blocks[start_index + 1..end_index].to_vec();
    Some((body, end, end_index))
}

/// Creates an empty block to sit right after `original`.
fn create_next_block<'a>(alloc: &'a Allocator, original: ArenaNode<'a>) -> ArenaNode<'a> {
    let (last_address, index, warpins_count) = match &*original.borrow() {
        Node::Block(inner) => (inner.last_address, inner.index, inner.warpins_count),
        _ => (0, 0, 0),
    };

    ArenaNode::new(
        alloc,
        Node::Block(ArenaBox::new_in(
            Block {
                index: index + 1,
                first_address: last_address + 1,
                last_address: last_address + 1,
                last_body_address: last_address + 1,
                warpins_count,
                is_loop: false,
                contents: ArenaVec::from_iter_in([], &alloc),
                warp: None,
            },
            &alloc,
        )),
    )
}

// MARK: cleanups

/// Merges blocks that are only reachable through the fallthrough edge of their
/// predecessor.
pub fn cleanup_ast<'a>(
    alloc: &'a Allocator,
    mut blocks: Vec<ArenaNode<'a>>,
) -> Result<Vec<ArenaNode<'a>>> {
    let mut next_index = 0;
    while next_index < blocks.len() {
        let index = next_index;
        next_index += 1;

        // The first block is left alone: it holds the function's entry code.
        if index == 0 {
            continue;
        }

        let block = blocks[index];
        let sources = find_warps_to(&blocks, block);
        if sources.len() != 1 {
            continue;
        }

        let source = sources[0];
        let Some(warp) = source.block_warp() else {
            continue;
        };
        if !warp.is_flow() {
            continue;
        }

        if blocks.iter().position(|item| *item == source) != Some(index - 1) {
            return Err(internal(
                "fallthrough edge that does not lead to the next block",
            ));
        }

        let mut contents = source.block_contents();
        contents.extend(block.block_contents());
        source.borrow_mut().block_set_contents(alloc, contents);
        if let Some(warp) = block.block_warp() {
            source.borrow_mut().block_set_warp(warp);
        }
        let last_address = block.block_range().map(|(_, last)| last);
        if let (Some(last_address), Node::Block(source_block)) =
            (last_address, &mut *source.borrow_mut())
        {
            source_block.last_address = last_address;
        }

        blocks.remove(index);
        next_index = index;
    }

    // With everything packed together, the registers a generic loop reads can
    // finally be eliminated: the statements that feed them are all in one
    // block now.
    if let Some(first) = blocks.first().copied() {
        slotworks::eliminate_temporary(
            alloc,
            first,
            slotworks::Options {
                ignore_ambiguous: false,
                ..Default::default()
            },
        )?;
    }

    Ok(blocks)
}

/// All blocks whose warp can reach `target`.
fn find_warps_to<'a>(blocks: &[ArenaNode<'a>], target: ArenaNode<'a>) -> Vec<ArenaNode<'a>> {
    let mut sources = Vec::new();
    for block in blocks {
        let Some(warp) = block.block_warp() else {
            continue;
        };
        let borrowed = warp.borrow();
        if borrowed
            .block_targets()
            .iter()
            .any(|candidate| (*candidate).same_as(target))
        {
            sources.push(*block);
        }
    }
    sources
}

/// Concatenates the contents of every block along the fallthrough chain.
pub fn glue_flows<'a>(alloc: &'a Allocator, root: ArenaNode<'a>) -> Result<()> {
    for statements in root.own_statement_lists() {
        let blocks = statements.list_contents();
        let Some(last) = blocks.last().copied() else {
            continue;
        };
        if !last.is_block() {
            // Expression unwarping can leave a plain statement behind.
            continue;
        }

        // A block that was given up on is about to be emptied into the one
        // after it, so the mark moves to the statement that ends up carrying it
        // forward; otherwise the loss would go unnoticed.
        let mut error_pending = false;

        for index in 0..blocks.len() - 1 {
            let block = blocks[index];
            if block.has_error() {
                error_pending = true;
            }
            if block.block_contents().is_empty() {
                continue;
            }
            if error_pending {
                if let Some(first) = block.block_first() {
                    first.mark_error();
                }
                error_pending = false;
            }

            let warp = block
                .block_warp()
                .ok_or_else(|| internal("block without a warp"))?;
            if !warp.is_flow() {
                return Err(internal("control flow that could not be structured"));
            }
            let target = warp
                .target(false)
                .ok_or_else(|| internal("flow warp without a target"))?;
            if !target.same_as(blocks[index + 1]) {
                return Err(internal("fallthrough edge that skips a block"));
            }

            let mut merged = block.block_contents();
            merged.extend(target.block_contents());
            target.borrow_mut().block_set_contents(alloc, merged);
            block.borrow_mut().block_set_contents(alloc, Vec::new());
        }

        let contents = last.block_contents();
        statements.borrow_mut().list_set_contents(alloc, contents);
    }

    Ok(())
}

/// Drops a trailing `return` that has no values.
///
/// Only the statements of a function are looked at: a `return` at the end of an
/// `if` body is what leaves the function early, and dropping it would change
/// what the code does.
pub fn trim_redundant_returns<'a>(alloc: &'a Allocator, root: ArenaNode<'a>) -> Result<()> {
    for function in root.functions() {
        let statements = match &*function.borrow() {
            Node::FunctionDefinition(inner) => inner.statements,
            _ => continue,
        };
        let contents = statements.list_contents();
        if contents.len() < 2 {
            continue;
        }

        let last = *contents.last().expect("checked above");
        let is_empty_return = match &*last.borrow() {
            Node::Return(inner) => inner.returns.list_contents().is_empty(),
            _ => false,
        };
        if is_empty_return {
            let mut contents = contents;
            contents.pop();
            statements.borrow_mut().list_set_contents(alloc, contents);
        }
    }
    Ok(())
}
