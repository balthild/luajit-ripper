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

use super::nodes::*;
use super::{slotworks, traverse};
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
pub fn unwarp<'a>(alloc: &'a Allocator, function: NodeRef<'a>, recovery: Recovery) -> Result<()> {
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
pub fn unwarp_chunk<'a>(alloc: &'a Allocator, root: NodeRef<'a>, recovery: Recovery) -> Result<()> {
    let mut functions = traverse::functions(root);
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
    root: NodeRef<'a>,
    step: impl Fn(Vec<NodeRef<'a>>) -> Result<Vec<NodeRef<'a>>>,
) -> Result<()> {
    for statements in traverse::own_statement_lists(root) {
        let contents = traverse::list_contents(statements);
        let result = step(contents)?;
        set_list_contents(alloc, statements, result);
    }

    for statements in traverse::own_statement_lists(root) {
        for (index, node) in traverse::list_contents(statements).iter().enumerate() {
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

/// Whether two optional nodes are the same node.
fn same_optional<'a>(a: Option<NodeRef<'a>>, b: Option<NodeRef<'a>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => traverse::same_node(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// How many warps lead to a block.
fn warpins<'a>(block: NodeRef<'a>) -> u32 {
    match &*block.borrow() {
        Node::Block(inner) => inner.warpins_count,
        _ => 0,
    }
}

// MARK: control flow helpers

/// Identity of a node, for the sets the passes keep.
fn node_key<'a>(node: NodeRef<'a>) -> usize {
    traverse::node_key(node)
}

/// The block a warp leads to.
///
/// A conditional branch is represented by the block its *false* branch takes,
/// because those are the ones that jump in the bytecode; the true branch falls
/// through. An `EndWarp` only knows where it would have gone when the pass that
/// closed the region recorded it.
fn get_target<'a>(warp: &Node<'a>, allow_end: bool) -> Option<NodeRef<'a>> {
    match warp {
        Node::ConditionalWarp(inner) => inner.false_target,
        Node::UnconditionalWarp(inner) => inner.target,
        // ljd asserts these two never appear here; returning their way out
        // keeps a malformed graph from taking the decompiler down.
        Node::IteratorWarp(inner) => inner.way_out,
        Node::NumericLoopWarp(inner) => inner.way_out,
        Node::EndWarp(inner) if allow_end => inner.target,
        _ => None,
    }
}

/// Points a warp at another block.
fn set_target<'a>(warp: NodeRef<'a>, target: Option<NodeRef<'a>>) {
    match &mut *warp.borrow_mut() {
        Node::ConditionalWarp(inner) => inner.false_target = target,
        Node::UnconditionalWarp(inner) => inner.target = target,
        Node::EndWarp(inner) => inner.target = target,
        Node::IteratorWarp(inner) => inner.way_out = target,
        Node::NumericLoopWarp(inner) => inner.way_out = target,
        _ => {}
    }
}

/// Whether a warp is an unconditional flow into the next block.
fn is_flow<'a>(warp: &Node<'a>) -> bool {
    matches!(warp, Node::UnconditionalWarp(inner) if inner.kind == UnconditionalWarpKind::Flow)
}

/// Whether a warp is an unconditional jump somewhere else.
fn is_jump<'a>(warp: &Node<'a>) -> bool {
    matches!(warp, Node::UnconditionalWarp(inner) if inner.kind == UnconditionalWarpKind::Jump)
}

/// Makes a block flow into `target`.
fn set_flow_to<'a>(alloc: &'a Allocator, block: NodeRef<'a>, target: NodeRef<'a>) {
    let warp = Node::emplace(
        alloc,
        Node::UnconditionalWarp(ArenaBox::new_in(
            UnconditionalWarp {
                kind: UnconditionalWarpKind::Flow,
                target: Some(target),
                is_uclo: false,
                meta: Meta::default(),
            },
            &alloc,
        )),
    );
    traverse::set_block_warp(block, warp);
}

/// Ends the region that starts at `block`.
///
/// `force_no_target` leaves the end without a target, which matters for loops:
/// an end that points outside the loop would confuse the checks that follow.
fn set_end<'a>(alloc: &'a Allocator, block: NodeRef<'a>, force_no_target: bool) {
    let target = if force_no_target {
        None
    } else {
        traverse::block_warp(block).and_then(|warp| get_target(&warp.borrow(), true))
    };

    let end = Node::emplace(
        alloc,
        Node::EndWarp(ArenaBox::new_in(
            EndWarp {
                target,
                meta: Meta::default(),
            },
            &alloc,
        )),
    );
    traverse::set_block_warp(block, end);
}

/// Points every warp that led to `original` at `replacement` instead.
///
/// The false branch of a conditional is only rewritten when it jumps forward,
/// unless `add_jumpback` says otherwise: a backward false branch is a loop, and
/// retargeting it would change what the loop does.
fn replace_targets<'a>(
    alloc: &'a Allocator,
    blocks: &[NodeRef<'a>],
    original: NodeRef<'a>,
    replacement: NodeRef<'a>,
    add_jumpback: bool,
) {
    let _ = alloc;
    for block in blocks {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };

        match &mut *warp.borrow_mut() {
            Node::UnconditionalWarp(inner) => {
                if inner
                    .target
                    .is_some_and(|t| traverse::same_node(t, original))
                {
                    inner.target = Some(replacement);
                }
            }
            Node::ConditionalWarp(inner) => {
                if inner
                    .true_target
                    .is_some_and(|t| traverse::same_node(t, original))
                {
                    inner.true_target = Some(replacement);
                }
                let target_last = inner
                    .false_target
                    .and_then(traverse::block_range)
                    .map(|(_, last)| last);
                let block_last = traverse::block_range(block).map(|(_, last)| last);
                let forward = match (target_last, block_last) {
                    (Some(target), Some(current)) => target > current,
                    _ => false,
                };
                if inner
                    .false_target
                    .is_some_and(|t| traverse::same_node(t, original))
                    && (forward || add_jumpback)
                {
                    inner.false_target = Some(replacement);
                }
            }
            Node::IteratorWarp(inner) => {
                if inner
                    .way_out
                    .is_some_and(|t| traverse::same_node(t, original))
                {
                    inner.way_out = Some(replacement);
                }
                if inner.body.is_some_and(|t| traverse::same_node(t, original)) {
                    inner.body = Some(replacement);
                }
            }
            Node::NumericLoopWarp(inner) => {
                if inner
                    .way_out
                    .is_some_and(|t| traverse::same_node(t, original))
                {
                    inner.way_out = Some(replacement);
                }
                if inner.body.is_some_and(|t| traverse::same_node(t, original)) {
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
    blocks: &[NodeRef<'a>],
    topmost_end: Option<NodeRef<'a>>,
) -> Option<NodeRef<'a>> {
    let mut end = *blocks.first()?;

    for block in blocks {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let target = get_target(&warp.borrow(), true);

        let Some(target) = target else {
            return Some(*block);
        };

        if warp.borrow().kind() == "unconditional warp" && traverse::same_node(target, end) {
            return Some(end);
        }

        if block_index(target) > block_index(end) {
            end = target;
        }
    }

    let _ = topmost_end;
    Some(end)
}

/// Whether the last thing a block does is test a constant.
///
/// `b = false and (x or y)` is one of those: the compiler materialises the
/// constant into a register, and the branch afterwards tests that register.
fn contains_primitive_condition<'a>(block: NodeRef<'a>) -> bool {
    let contents = traverse::block_contents(block);
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

        let destinations = traverse::list_contents(assignment.destinations);
        let expressions = traverse::list_contents(assignment.expressions);
        let (Some(destination), Some(expression)) = (destinations.last(), expressions.last())
        else {
            continue;
        };

        let is_primitive = matches!(&*expression.borrow(), Node::Primitive(_));
        if !is_primitive {
            continue;
        }

        let unnamed = matches!(&*destination.borrow(), Node::Identifier(identifier)
            if identifier.name.is_none());
        if unnamed {
            // A primitive condition inside the block's own expression, as in
            // `b = false and (x or y)`.
            return true;
        }

        // A primitive that follows an ordinary assignment, as in
        // `a = nil; b = (false and x) or y`.
        return !matches!(
            traverse::block_warp(block).map(|warp| warp.borrow().kind()),
            Some("conditional warp")
        );
    }

    false
}

// MARK: body extraction

/// The region that starts at `start_index`: its body, its end, and the position
/// of that end.
fn extract_if_body<'a>(
    start_index: usize,
    blocks: &[NodeRef<'a>],
    topmost_end: Option<NodeRef<'a>>,
) -> Option<(Vec<NodeRef<'a>>, NodeRef<'a>, usize)> {
    let search = if start_index > 0 {
        blocks[start_index..].to_vec()
    } else {
        blocks.to_vec()
    };

    let end = find_branching_end(&search, topmost_end)?;

    let end_index = match traverse::position(blocks, end) {
        Some(index) => index,
        None => {
            if topmost_end.is_some_and(|topmost| traverse::same_node(topmost, end)) {
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
fn create_next_block<'a>(alloc: &'a Allocator, original: NodeRef<'a>) -> NodeRef<'a> {
    let (last_address, index, warpins_count) = match &*original.borrow() {
        Node::Block(inner) => (inner.last_address, inner.index, inner.warpins_count),
        _ => (0, 0, 0),
    };

    Node::emplace(
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
    mut blocks: Vec<NodeRef<'a>>,
) -> Result<Vec<NodeRef<'a>>> {
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
        let Some(warp) = traverse::block_warp(source) else {
            continue;
        };
        if !traverse::is_flow(&warp.borrow()) {
            continue;
        }

        if traverse::position(&blocks, source) != Some(index - 1) {
            return Err(internal(
                "fallthrough edge that does not lead to the next block",
            ));
        }

        let mut contents = traverse::block_contents(source);
        contents.extend(traverse::block_contents(block));
        traverse::set_block_contents(alloc, source, contents);
        if let Some(warp) = traverse::block_warp(block) {
            traverse::set_block_warp(source, warp);
        }
        let last_address = traverse::block_range(block).map(|(_, last)| last);
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
fn find_warps_to<'a>(blocks: &[NodeRef<'a>], target: NodeRef<'a>) -> Vec<NodeRef<'a>> {
    let mut sources = Vec::new();
    for block in blocks {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let borrowed = warp.borrow();
        if traverse::block_targets(&borrowed)
            .iter()
            .any(|candidate| traverse::same_node(candidate, target))
        {
            sources.push(*block);
        }
    }
    sources
}

/// Concatenates the contents of every block along the fallthrough chain.
pub fn glue_flows<'a>(alloc: &'a Allocator, root: NodeRef<'a>) -> Result<()> {
    for statements in traverse::own_statement_lists(root) {
        let blocks = traverse::list_contents(statements);
        let Some(last) = blocks.last().copied() else {
            continue;
        };
        if !matches!(&*last.borrow(), Node::Block(_)) {
            // Expression unwarping can leave a plain statement behind.
            continue;
        }

        // A block that was given up on is about to be emptied into the one
        // after it, so the mark moves to the statement that ends up carrying it
        // forward; otherwise the loss would go unnoticed.
        let mut error_pending = false;

        for index in 0..blocks.len() - 1 {
            let block = blocks[index];
            if has_error(block) {
                error_pending = true;
            }
            if traverse::block_contents(block).is_empty() {
                continue;
            }
            if error_pending {
                if let Some(first) = traverse::block_contents(block).first().copied() {
                    mark_error(first);
                }
                error_pending = false;
            }

            let warp =
                traverse::block_warp(block).ok_or_else(|| internal("block without a warp"))?;
            if !traverse::is_flow(&warp.borrow()) {
                return Err(internal("control flow that could not be structured"));
            }
            let target = traverse::jump_target(&warp.borrow())
                .ok_or_else(|| internal("flow warp without a target"))?;
            if !traverse::same_node(target, blocks[index + 1]) {
                return Err(internal("fallthrough edge that skips a block"));
            }

            let mut merged = traverse::block_contents(block);
            merged.extend(traverse::block_contents(target));
            traverse::set_block_contents(alloc, target, merged);
            traverse::set_block_contents(alloc, block, Vec::new());
        }

        let contents = traverse::block_contents(last);
        set_list_contents(alloc, statements, contents);
    }

    Ok(())
}

/// Drops a trailing `return` that has no values.
///
/// Only the statements of a function are looked at: a `return` at the end of an
/// `if` body is what leaves the function early, and dropping it would change
/// what the code does.
pub fn trim_redundant_returns<'a>(alloc: &'a Allocator, root: NodeRef<'a>) -> Result<()> {
    for function in traverse::functions(root) {
        let statements = match &*function.borrow() {
            Node::FunctionDefinition(inner) => inner.statements,
            _ => continue,
        };
        let contents = traverse::list_contents(statements);
        if contents.len() < 2 {
            continue;
        }

        let last = *contents.last().expect("checked above");
        let is_empty_return = match &*last.borrow() {
            Node::Return(inner) => traverse::list_contents(inner.returns).is_empty(),
            _ => false,
        };
        if is_empty_return {
            let mut contents = contents;
            contents.pop();
            set_list_contents(alloc, statements, contents);
        }
    }
    Ok(())
}

// MARK: block index

fn block_index<'a>(block: NodeRef<'a>) -> u32 {
    match &*block.borrow() {
        Node::Block(inner) => inner.index,
        _ => u32::MAX,
    }
}
