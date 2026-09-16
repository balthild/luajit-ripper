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

use std::rc::Rc;

use crate::error::{Error, Result};

use super::nodes::*;
use super::slotworks;
use super::traverse;

pub use expressions::unwarp_expressions;
pub use ifs::unwarp_ifs;
pub use loops::{fix_loops, unwarp_loops};

/// Rebuilds structured control flow for one function definition.
///
/// Only the statement lists that belong to this function are rewritten; the
/// functions nested inside it are left for their own call.
pub fn unwarp(function: &NodeRef) -> Result<()> {
    run_step(function, |blocks| fix_loops(blocks, false))?;
    run_step(function, |blocks| fix_loops(blocks, true))?;
    run_step(function, unwarp_expressions)?;
    run_step(function, unwarp_expressions)?;
    run_step(function, |blocks| unwarp_loops(blocks, false))?;
    run_step(function, |blocks| unwarp_loops(blocks, true))?;
    run_step(function, unwarp_ifs)?;
    run_step(function, cleanup_ast)?;
    glue_flows(function)?;
    trim_redundant_returns(function)?;

    // With everything rebuilt, the calls that pass their receiver as the first
    // argument can be written as method calls.
    slotworks::simplify_ast(function, &mut |_| {});

    Ok(())
}

/// Rebuilds the control flow of every function of a chunk.
///
/// The nested functions are done first, so that a function is finished before
/// the one that contains it is looked at.
pub fn unwarp_chunk(root: &NodeRef) -> Result<()> {
    let mut functions = traverse::functions(root);
    functions.reverse();

    for function in functions {
        unwarp(&function)?;
    }

    Ok(())
}

/// Applies a step to every statement list of a function and renumbers the
/// blocks afterwards, which may have moved.
fn run_step(root: &NodeRef, step: impl Fn(Vec<NodeRef>) -> Result<Vec<NodeRef>>) -> Result<()> {
    for statements in traverse::own_statement_lists(root) {
        let contents = traverse::list_contents(&statements);
        let result = step(contents)?;
        traverse::set_list_contents(&statements, result);
    }

    for statements in traverse::own_statement_lists(root) {
        for (index, node) in traverse::list_contents(&statements).iter().enumerate() {
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
fn same_optional(a: Option<&NodeRef>, b: Option<&NodeRef>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// How many warps lead to a block.
fn warpins(block: &NodeRef) -> u32 {
    match &*block.borrow() {
        Node::Block(inner) => inner.warpins_count,
        _ => 0,
    }
}

// -- control flow helpers --------------------------------------------------

/// Identity of a node, for the sets the passes keep.
fn node_key(node: &NodeRef) -> usize {
    Rc::as_ptr(node) as usize
}

/// The block a warp leads to.
///
/// A conditional branch is represented by the block its *false* branch takes,
/// because those are the ones that jump in the bytecode; the true branch falls
/// through. An `EndWarp` only knows where it would have gone when the pass that
/// closed the region recorded it.
fn get_target(warp: &Node, allow_end: bool) -> Option<NodeRef> {
    match warp {
        Node::ConditionalWarp(inner) => inner.false_target.clone(),
        Node::UnconditionalWarp(inner) => inner.target.clone(),
        // ljd asserts these two never appear here; returning their way out
        // keeps a malformed graph from taking the decompiler down.
        Node::IteratorWarp(inner) => inner.way_out.clone(),
        Node::NumericLoopWarp(inner) => inner.way_out.clone(),
        Node::EndWarp(inner) if allow_end => inner.target.clone(),
        _ => None,
    }
}

/// Points a warp at another block.
fn set_target(warp: &NodeRef, target: Option<NodeRef>) {
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
fn is_flow(warp: &Node) -> bool {
    matches!(warp, Node::UnconditionalWarp(inner) if inner.kind == UnconditionalWarpKind::Flow)
}

/// Whether a warp is an unconditional jump somewhere else.
fn is_jump(warp: &Node) -> bool {
    matches!(warp, Node::UnconditionalWarp(inner) if inner.kind == UnconditionalWarpKind::Jump)
}

/// Makes a block flow into `target`.
fn set_flow_to(block: &NodeRef, target: &NodeRef) {
    let warp = node(Node::UnconditionalWarp(Box::new(UnconditionalWarp {
        kind: UnconditionalWarpKind::Flow,
        target: Some(target.clone()),
        is_uclo: false,
        meta: Meta::default(),
    })));
    traverse::set_block_warp(block, warp);
}

/// Ends the region that starts at `block`.
///
/// `force_no_target` leaves the end without a target, which matters for loops:
/// an end that points outside the loop would confuse the checks that follow.
fn set_end(block: &NodeRef, force_no_target: bool) {
    let target = if force_no_target {
        None
    } else {
        traverse::block_warp(block).and_then(|warp| get_target(&warp.borrow(), true))
    };

    let end = node(Node::EndWarp(Box::new(EndWarp {
        target,
        meta: Meta::default(),
    })));
    traverse::set_block_warp(block, end);
}

/// Points every warp that led to `original` at `replacement` instead.
///
/// The false branch of a conditional is only rewritten when it jumps forward,
/// unless `add_jumpback` says otherwise: a backward false branch is a loop, and
/// retargeting it would change what the loop does.
fn replace_targets(
    blocks: &[NodeRef],
    original: &NodeRef,
    replacement: &NodeRef,
    add_jumpback: bool,
) {
    for block in blocks {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };

        match &mut *warp.borrow_mut() {
            Node::UnconditionalWarp(inner) => {
                if inner
                    .target
                    .as_ref()
                    .is_some_and(|t| Rc::ptr_eq(t, original))
                {
                    inner.target = Some(replacement.clone());
                }
            }
            Node::ConditionalWarp(inner) => {
                if inner
                    .true_target
                    .as_ref()
                    .is_some_and(|t| Rc::ptr_eq(t, original))
                {
                    inner.true_target = Some(replacement.clone());
                }
                let target_last = inner
                    .false_target
                    .as_ref()
                    .and_then(traverse::block_range)
                    .map(|(_, last)| last);
                let block_last = traverse::block_range(block).map(|(_, last)| last);
                let forward = match (target_last, block_last) {
                    (Some(target), Some(current)) => target > current,
                    _ => false,
                };
                if inner
                    .false_target
                    .as_ref()
                    .is_some_and(|t| Rc::ptr_eq(t, original))
                    && (forward || add_jumpback)
                {
                    inner.false_target = Some(replacement.clone());
                }
            }
            Node::IteratorWarp(inner) => {
                if inner
                    .way_out
                    .as_ref()
                    .is_some_and(|t| Rc::ptr_eq(t, original))
                {
                    inner.way_out = Some(replacement.clone());
                }
                if inner.body.as_ref().is_some_and(|t| Rc::ptr_eq(t, original)) {
                    inner.body = Some(replacement.clone());
                }
            }
            Node::NumericLoopWarp(inner) => {
                if inner
                    .way_out
                    .as_ref()
                    .is_some_and(|t| Rc::ptr_eq(t, original))
                {
                    inner.way_out = Some(replacement.clone());
                }
                if inner.body.as_ref().is_some_and(|t| Rc::ptr_eq(t, original)) {
                    inner.body = Some(replacement.clone());
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
fn find_branching_end(blocks: &[NodeRef], topmost_end: Option<&NodeRef>) -> Option<NodeRef> {
    let mut end = blocks.first()?.clone();

    for block in blocks {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let target = get_target(&warp.borrow(), true);

        let Some(target) = target else {
            return Some(block.clone());
        };

        if warp.borrow().kind() == "unconditional warp" && Rc::ptr_eq(&target, &end) {
            return Some(end);
        }

        if block_index(&target) > block_index(&end) {
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
fn contains_primitive_condition(block: &NodeRef) -> bool {
    let contents = traverse::block_contents(block);
    if contents.is_empty() {
        return false;
    }

    let last_index = contents.len() - 1;
    for i in 0..last_index {
        let content = contents[last_index - i].clone();
        let borrowed = content.borrow();
        let Node::Assignment(assignment) = &*borrowed else {
            continue;
        };

        let destinations = traverse::list_contents(&assignment.destinations);
        let expressions = traverse::list_contents(&assignment.expressions);
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

/// The region that starts at `start_index`: its body, its end, and the position
/// of that end.
fn extract_if_body(
    start_index: usize,
    blocks: &[NodeRef],
    topmost_end: Option<&NodeRef>,
) -> Option<(Vec<NodeRef>, NodeRef, usize)> {
    let search = if start_index > 0 {
        blocks[start_index..].to_vec()
    } else {
        blocks.to_vec()
    };

    let end = find_branching_end(&search, topmost_end)?;

    let end_index = match traverse::position(blocks, &end) {
        Some(index) => index,
        None => {
            if topmost_end.is_some_and(|topmost| Rc::ptr_eq(topmost, &end)) {
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
fn create_next_block(original: &NodeRef) -> NodeRef {
    let (last_address, index, warpins_count) = match &*original.borrow() {
        Node::Block(inner) => (inner.last_address, inner.index, inner.warpins_count),
        _ => (0, 0, 0),
    };

    node(Node::Block(Box::new(Block {
        index: index + 1,
        first_address: last_address + 1,
        last_address: last_address + 1,
        last_body_address: last_address + 1,
        warpins_count,
        is_loop: false,
        contents: Vec::new(),
        warp: None,
    })))
}

/// Merges blocks that are only reachable through the fallthrough edge of their
/// predecessor.
pub fn cleanup_ast(mut blocks: Vec<NodeRef>) -> Result<Vec<NodeRef>> {
    let mut next_index = 0;
    while next_index < blocks.len() {
        let index = next_index;
        next_index += 1;

        // The first block is left alone: it holds the function's entry code.
        if index == 0 {
            continue;
        }

        let block = blocks[index].clone();
        let sources = find_warps_to(&blocks, &block);
        if sources.len() != 1 {
            continue;
        }

        let source = sources[0].clone();
        let Some(warp) = traverse::block_warp(&source) else {
            continue;
        };
        if !traverse::is_flow(&warp.borrow()) {
            continue;
        }

        if traverse::position(&blocks, &source) != Some(index - 1) {
            return Err(internal(
                "fallthrough edge that does not lead to the next block",
            ));
        }

        let mut contents = traverse::block_contents(&source);
        contents.extend(traverse::block_contents(&block));
        traverse::set_block_contents(&source, contents);
        if let Some(warp) = traverse::block_warp(&block) {
            traverse::set_block_warp(&source, warp);
        }
        let last_address = traverse::block_range(&block).map(|(_, last)| last);
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
    if let Some(first) = blocks.first().cloned() {
        slotworks::eliminate_temporary(
            &first,
            slotworks::Options {
                ignore_ambiguous: false,
                ..Default::default()
            },
        )?;
    }

    Ok(blocks)
}

/// All blocks whose warp can reach `target`.
fn find_warps_to(blocks: &[NodeRef], target: &NodeRef) -> Vec<NodeRef> {
    let mut sources = Vec::new();
    for block in blocks {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let borrowed = warp.borrow();
        if traverse::block_targets(&borrowed)
            .iter()
            .any(|candidate| Rc::ptr_eq(candidate, target))
        {
            sources.push(block.clone());
        }
    }
    sources
}

/// Concatenates the contents of every block along the fallthrough chain.
pub fn glue_flows(root: &NodeRef) -> Result<()> {
    for statements in traverse::own_statement_lists(root) {
        let blocks = traverse::list_contents(&statements);
        let Some(last) = blocks.last() else {
            continue;
        };
        if !matches!(&*last.borrow(), Node::Block(_)) {
            // Expression unwarping can leave a plain statement behind.
            continue;
        }

        for index in 0..blocks.len() - 1 {
            let block = blocks[index].clone();
            if traverse::block_contents(&block).is_empty() {
                continue;
            }

            let warp =
                traverse::block_warp(&block).ok_or_else(|| internal("block without a warp"))?;
            if !traverse::is_flow(&warp.borrow()) {
                return Err(internal("control flow that could not be structured"));
            }
            let target = traverse::jump_target(&warp.borrow())
                .ok_or_else(|| internal("flow warp without a target"))?;
            if !Rc::ptr_eq(&target, &blocks[index + 1]) {
                return Err(internal("fallthrough edge that skips a block"));
            }

            let mut merged = traverse::block_contents(&block);
            merged.extend(traverse::block_contents(&target));
            traverse::set_block_contents(&target, merged);
            traverse::set_block_contents(&block, Vec::new());
        }

        let contents = traverse::block_contents(last);
        traverse::set_list_contents(&statements, contents);
    }

    Ok(())
}

/// Drops a trailing `return` that has no values.
///
/// Only the statements of a function are looked at: a `return` at the end of an
/// `if` body is what leaves the function early, and dropping it would change
/// what the code does.
pub fn trim_redundant_returns(root: &NodeRef) -> Result<()> {
    for function in traverse::functions(root) {
        let statements = match &*function.borrow() {
            Node::FunctionDefinition(inner) => inner.statements.clone(),
            _ => continue,
        };
        let contents = traverse::list_contents(&statements);
        if contents.len() < 2 {
            continue;
        }

        let last = contents.last().cloned().expect("checked above");
        let is_empty_return = match &*last.borrow() {
            Node::Return(inner) => traverse::list_contents(&inner.returns).is_empty(),
            _ => false,
        };
        if is_empty_return {
            let mut contents = contents;
            contents.pop();
            traverse::set_list_contents(&statements, contents);
        }
    }
    Ok(())
}

// -- helpers ---------------------------------------------------------------

fn block_index(block: &NodeRef) -> u32 {
    match &*block.borrow() {
        Node::Block(inner) => inner.index,
        _ => u32::MAX,
    }
}
