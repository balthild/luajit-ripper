//! Rebuilds loops out of the back edges of a block graph.
//!
//! This is a port of the loop half of ljd's `ast/unwarper.py`.
//!
//! A loop is a jump back to a block that comes earlier in the list. The blocks
//! between that target and the jump are the body; a marker block left over from
//! the compiler says where the body starts, and everything between the loop head
//! and that marker is the condition.

use std::collections::HashSet;

use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use super::super::nodes::*;
use super::super::traverse;
use super::expressions::compile_expression;
use super::*;
use crate::error::{Error, Result};

// MARK: entry point

/// Which kind of `break` a block holds, for the pass that retargets the warps
/// that leave a loop.
const BREAK_INFINITE: u8 = 0;
const BREAK_ONE_USE: u8 = 1;

/// Ends the loops of one block list.
pub fn fix_loops<'a>(
    alloc: &'a Allocator,
    blocks: Vec<NodeRef<'a>>,
    repeat_until: bool,
) -> Result<Vec<NodeRef<'a>>> {
    let mut blocks = blocks;
    let loops = find_all_loops(&blocks, repeat_until)?;
    if loops.is_empty() {
        return Ok(blocks);
    }

    let fixed = cleanup_breaks_and_if_ends(alloc, &loops, &mut blocks)?;
    let mut replacements: Vec<(NodeRef<'a>, NodeRef<'a>)> = Vec::new();
    let lookup = |replacements: &[(NodeRef<'a>, NodeRef<'a>)], node: NodeRef<'a>| -> NodeRef<'a> {
        for (from, to) in replacements {
            if traverse::same_node(from, node) {
                return *to;
            }
        }
        node
    };

    for (start, end) in fixed {
        let end = lookup(&replacements, end);
        let mut start_index = position(&blocks, start)?;
        let end_index = position(&blocks, end)?;
        // The outermost block that marks the start of the body.
        let mut loop_block = None;
        for block in &blocks[start_index..end_index] {
            if block_is_loop(block) {
                loop_block = Some(*block);
                break;
            }
        }

        // A `for ... in` loop has no marker of its own, and using the one the
        // iterator warp sits on would hide the call that produces the values.
        if matches!(&*start.borrow(), Node::Block(inner)
            if inner.warp.is_some_and(|warp| matches!(&*warp.borrow(), Node::IteratorWarp(_))))
        {
            loop_block = None;
        }

        let Some(loop_block) = loop_block else {
            blocks = handle_single_loop(alloc, start, end, blocks, repeat_until)?;
            continue;
        };

        let mut body_start_index = position(&blocks, loop_block)?;

        // Skip the marker instruction; a `repeat` loop keeps its first block,
        // because that is where the user's code starts.
        if body_start_index == start_index && !repeat_until {
            body_start_index += 1;
        }

        let expr_body = blocks[start_index..body_start_index].to_vec();
        let body = blocks[body_start_index..end_index].to_vec();
        let mut built = unwarp_loop(alloc, start, end, body, Some(expr_body))?;
        let block = loop_build_block(
            alloc,
            start,
            &mut built.body,
            end,
            &blocks,
            built.node,
            body_start_index as u32,
        )?;
        set_statements(alloc, built.node, &built.body);

        if body_start_index == start_index {
            let mut new_blocks = blocks[..start_index + 1].to_vec();
            new_blocks.push(block);
            new_blocks.extend_from_slice(&blocks[end_index..]);
            blocks = new_blocks;
        } else {
            let is_while = matches!(&*built.node.borrow(), Node::While(_));
            let before = if is_while && start_index < body_start_index - 1 {
                // A condition that holds statements of its own has to be
                // unpacked before the loop can be built.
                if blocks[start_index..body_start_index]
                    .iter()
                    .any(|node| !traverse::block_contents(node).is_empty())
                {
                    start_index = body_start_index;
                }

                let before = blocks[..start_index].to_vec();
                if !before.is_empty() {
                    let old_start = blocks[start_index];
                    replace_targets(alloc, &before, old_start, block, false);
                    replacements.push((old_start, block));
                }
                before
            } else {
                blocks[..body_start_index].to_vec()
            };

            let mut new_blocks = before;
            new_blocks.push(block);
            new_blocks.extend_from_slice(&blocks[end_index..]);
            blocks = new_blocks;
        }

        validate_loop_body(&built.body)?;
    }

    validate_block_list(&blocks)?;
    Ok(blocks)
}

// MARK: finding loops

/// Verifies that no back edge is left over.
///
/// This is a check rather than a step: the loops are all built by
/// [`fix_loops`], and anything left here means the graph was not understood.
pub fn unwarp_loops<'a>(
    alloc: &'a Allocator,
    blocks: Vec<NodeRef<'a>>,
    repeat_until: bool,
) -> Result<Vec<NodeRef<'a>>> {
    let _ = alloc;
    let loops = find_all_loops(&blocks, repeat_until)?;
    if !loops.is_empty() {
        return Err(internal("a loop was left over"));
    }
    Ok(blocks)
}

/// Finds every back edge, innermost first.
///
/// The result is a list of the block a loop jumps back to and the block the
/// flow leaves the loop through.
fn find_all_loops<'a>(
    blocks: &[NodeRef<'a>],
    repeat_until: bool,
) -> Result<Vec<(NodeRef<'a>, NodeRef<'a>)>> {
    let mut loops: Vec<(NodeRef<'a>, NodeRef<'a>)> = Vec::new();

    // Two jumps back to the same block are the same loop: all but the last one
    // are `continue` statements the compiler put in, so the loop they describe
    // is dropped again.
    let mut starts: Vec<(usize, (NodeRef<'a>, NodeRef<'a>))> = Vec::new();

    let mut i = 0usize;
    while i < blocks.len() {
        let block = blocks[i];
        let Some(warp) = traverse::block_warp(block) else {
            i += 1;
            continue;
        };

        let warp_kind = {
            let borrowed = warp.borrow();
            match &*borrowed {
                Node::UnconditionalWarp(inner) => Some(inner.kind),
                Node::ConditionalWarp(_) => None,
                _ => {
                    i += 1;
                    continue;
                }
            }
        };

        if let Some(kind) = warp_kind {
            if kind == UnconditionalWarpKind::Flow {
                i += 1;
                continue;
            }

            let Some(start) = get_target(&warp.borrow(), false) else {
                return Err(internal("a jump without a target"));
            };

            if block_index(start) <= block_index(block) {
                if repeat_until {
                    return Err(internal("a back edge in a repeat pass"));
                }
                if i + 1 >= blocks.len() {
                    return Err(internal("a jump back from the last block"));
                }

                let entry = (start, blocks[i + 1]);
                let previous = starts
                    .iter()
                    .find(|(key, _)| *key == node_key(start))
                    .map(|(_, entry)| *entry);
                if let Some(previous) = previous
                    && let Some(index) = loops.iter().position(|(s, e)| {
                        traverse::same_node(s, previous.0) && traverse::same_node(e, previous.1)
                    })
                {
                    loops.remove(index);
                }

                loops.push(entry);
                starts.retain(|(key, _)| *key != node_key(start));
                starts.push((node_key(start), entry));
            }
        } else if repeat_until {
            let false_target = match &*warp.borrow() {
                Node::ConditionalWarp(inner) => inner.false_target,
                _ => None,
            };
            let Some(false_target) = false_target else {
                i += 1;
                continue;
            };

            if block_index(false_target) > block_index(block) {
                i += 1;
                continue;
            }

            let mut start = false_target;
            let first = block;
            let mut end = block;
            let mut last_i = i;

            // The end of the expression is the last jump back to the loop.
            while i < blocks.len() {
                let block = blocks[i];
                let Some(warp) = traverse::block_warp(block) else {
                    break;
                };
                let borrowed = warp.borrow();

                if !traverse::same_node(block, first) && !traverse::block_contents(block).is_empty()
                {
                    break;
                }
                if matches!(&*borrowed, Node::EndWarp(_)) {
                    break;
                }

                let Some(target) = get_target(&borrowed, false) else {
                    break;
                };
                if block_index(target) < block_index(block) {
                    if traverse::same_node(target, start) {
                        start = target;
                        end = block;
                        last_i = i;
                    } else {
                        break;
                    }
                }

                i += 1;
            }

            i = last_i;

            let end_index = position(blocks, end)?;
            let end = *blocks
                .get(end_index + 1)
                .ok_or_else(|| internal("a loop without an end"))?;

            loops.push((start, end));
        }

        i += 1;
    }

    // Inner loops come first, so they are built before the ones around them.
    loops.sort_by_key(|(start, _)| block_index(start));
    loops.reverse();
    Ok(loops)
}

// MARK: cleaning up nested loops

/// Retargets the jumps back to the start of nested loops, so that each one ends
/// at the block that follows its body.
fn cleanup_breaks_and_if_ends<'a>(
    alloc: &'a Allocator,
    loops: &[(NodeRef<'a>, NodeRef<'a>)],
    blocks: &mut Vec<NodeRef<'a>>,
) -> Result<Vec<(NodeRef<'a>, NodeRef<'a>)>> {
    let mut outer_start_index: Option<u32> = None;
    let mut outer_end: Option<NodeRef<'a>> = None;
    let mut current_start_index: Option<u32> = None;
    let mut current_end: Option<NodeRef<'a>> = None;

    let mut fixed: Vec<(NodeRef<'a>, NodeRef<'a>)> = Vec::new();

    for (start, end) in loops {
        let start_index = block_index(start);
        let is_nested =
            Some(start_index) == outer_start_index || Some(start_index) == current_start_index;

        if is_nested {
            let end_index = position(blocks, end)?;
            let last_in_body = *blocks
                .get(end_index.wrapping_sub(1))
                .ok_or_else(|| internal("a loop body that is empty"))?;
            let warp = traverse::block_warp(last_in_body)
                .ok_or_else(|| internal("a loop body without a warp"))?;
            if !matches!(&*warp.borrow(), Node::UnconditionalWarp(_)) {
                return Err(internal("a loop does not end with a jump"));
            }
            if !same_optional(get_target(&warp.borrow(), false), Some(*start)) {
                return Err(internal("a loop back edge does not reach its start"));
            }

            if Some(start_index) == outer_start_index {
                let outer_end =
                    outer_end.ok_or_else(|| internal("a nested loop without an outer end"))?;
                let outer_end_index = position(blocks, outer_end)?;
                let replacement = *blocks
                    .get(outer_end_index.wrapping_sub(1))
                    .ok_or_else(|| internal("an outer loop without a body"))?;
                set_target(warp, Some(replacement));
            } else {
                let current_end =
                    current_end.ok_or_else(|| internal("a nested loop without an end"))?;
                let current_end_index = position(blocks, current_end)?;

                let mut last = *blocks
                    .get(current_end_index.wrapping_sub(1))
                    .ok_or_else(|| internal("a loop without a body"))?;

                if traverse::same_node(last, end) {
                    // The end of the loop is also the last block of the body,
                    // so a block has to be made for the jump to land on.
                    let new_block = create_next_block(alloc, last);
                    let last_warp = traverse::block_warp(last)
                        .ok_or_else(|| internal("a loop end without a warp"))?;
                    traverse::set_block_warp(new_block, last_warp);
                    set_flow_to(alloc, last, new_block);
                    blocks.insert(current_end_index, new_block);
                    last = new_block;
                }

                if !traverse::block_contents(last).is_empty() {
                    return Err(internal("a loop end holds statements"));
                }
                let last_warp = traverse::block_warp(last)
                    .ok_or_else(|| internal("a loop end without a warp"))?;
                if !matches!(&*last_warp.borrow(), Node::UnconditionalWarp(_)) {
                    return Err(internal("a loop end without a jump"));
                }
                if !same_optional(get_target(&last_warp.borrow(), false), Some(*start)) {
                    return Err(internal("a loop end does not jump back"));
                }

                set_target(warp, Some(last));
            }
        } else {
            fixed.push((*start, *end));

            let nested = match (current_end, current_start_index) {
                (Some(current_end), Some(current_start_index)) => {
                    current_start_index < start_index
                        && block_index(current_end) >= block_index(end)
                }
                _ => false,
            };

            if nested {
                outer_start_index = current_start_index;
                outer_end = current_end;
            } else {
                outer_start_index = None;
                outer_end = None;
            }

            current_start_index = Some(start_index);
            current_end = Some(*end);
        }
    }

    Ok(fixed)
}

/// Builds the loop of a region that has no marker block of its own.
fn handle_single_loop<'a>(
    alloc: &'a Allocator,
    start: NodeRef<'a>,
    end: NodeRef<'a>,
    blocks: Vec<NodeRef<'a>>,
    repeat_until: bool,
) -> Result<Vec<NodeRef<'a>>> {
    let start_index = position(&blocks, start)?;
    let end_index = position(&blocks, end)?;

    let body = if repeat_until {
        blocks[start_index..end_index].to_vec()
    } else {
        blocks[start_index + 1..end_index].to_vec()
    };

    let mut built = unwarp_loop(alloc, start, end, body, None)?;
    let block = loop_build_block(
        alloc,
        start,
        &mut built.body,
        end,
        &blocks,
        built.node,
        block_index(start) + 1,
    )?;
    set_statements(alloc, built.node, &built.body);

    let mut new_blocks = blocks[..start_index + 1].to_vec();
    new_blocks.push(block);
    new_blocks.extend_from_slice(&blocks[end_index..]);
    Ok(new_blocks)
}

// MARK: breaking out

/// Puts the loop into a block of its own and turns the jumps back into it into
/// `break`s.
fn loop_build_block<'a>(
    alloc: &'a Allocator,
    start: NodeRef<'a>,
    body: &mut Vec<NodeRef<'a>>,
    end: NodeRef<'a>,
    blocks: &[NodeRef<'a>],
    loop_node: NodeRef<'a>,
    index: u32,
) -> Result<NodeRef<'a>> {
    let first = *body
        .first()
        .ok_or_else(|| internal("a loop without a body"))?;
    let last = *body
        .last()
        .ok_or_else(|| internal("a loop without a body"))?;
    let (first_address, _) = traverse::block_range(first).unwrap_or((0, 0));
    let (_, last_address) = traverse::block_range(last).unwrap_or((0, 0));

    let block = Node::emplace(
        alloc,
        Node::Block(ArenaBox::new_in(
            Block {
                index,
                first_address,
                last_address,
                last_body_address: 0,
                warpins_count: 0,
                is_loop: false,
                contents: ArenaVec::from_iter_in([loop_node], &alloc),
                warp: None,
            },
            &alloc,
        )),
    );
    set_flow_to(alloc, block, end);

    replace_targets(alloc, blocks, first, block, false);

    // The end block is left without a target: it may point outside the loop,
    // which would confuse the checks that follow.
    set_end(alloc, last, true);

    unwarp_breaks(alloc, start, body, end)?;
    Ok(block)
}

/// Rewrites the jumps that leave a loop into `break` statements.
fn unwarp_breaks<'a>(
    alloc: &'a Allocator,
    start: NodeRef<'a>,
    blocks: &mut Vec<NodeRef<'a>>,
    next_block: NodeRef<'a>,
) -> Result<()> {
    let mut blocks_set: HashSet<usize> = HashSet::new();
    blocks_set.insert(node_key(start));
    for block in blocks.iter() {
        blocks_set.insert(node_key(block));
    }

    let ends = gather_possible_ends(next_block)?;

    let mut breaks: HashSet<usize> = HashSet::new();
    let mut patched: Vec<NodeRef<'a>> = Vec::new();
    let length = blocks.len();

    for (i, block) in blocks.iter().enumerate() {
        let block = *block;
        let Some(warp) = traverse::block_warp(block) else {
            patched.push(block);
            continue;
        };
        if !matches!(&*warp.borrow(), Node::UnconditionalWarp(_)) {
            patched.push(block);
            continue;
        }

        let Some(target) = get_target(&warp.borrow(), false) else {
            return Err(internal("a jump without a target"));
        };
        if blocks_set.contains(&node_key(target)) {
            patched.push(block);
            continue;
        }
        if !ends.contains(&node_key(target)) {
            return Err(Error::Unsupported(
                "goto statements are not supported".to_string(),
            ));
        }

        let contents = traverse::block_contents(block);
        let is_placeholder = contents.len() == 1 && matches!(&*contents[0].borrow(), Node::NoOp(_));

        let block = if warpins(block) != 0 && !is_placeholder {
            // The block is jumped to as well, so the `break` gets a block of
            // its own and the original keeps flowing into it.
            let new_block = create_next_block(alloc, block);
            set_flow_to(alloc, block, new_block);
            patched.push(block);
            patched.push(new_block);
            new_block
        } else {
            patched.push(block);
            block
        };

        let contents = traverse::block_contents(block);
        let mut contents = if contents.len() == 1 && matches!(&*contents[0].borrow(), Node::NoOp(_))
        {
            Vec::new()
        } else {
            contents
        };
        contents.push(Node::emplace(alloc, Node::Break));
        traverse::set_block_contents(alloc, block, contents);

        if i + 1 == length {
            set_end(alloc, block, false);
        } else {
            let next = blocks[i + 1];
            set_flow_to(alloc, block, next);
        }

        breaks.insert(node_key(block));
    }

    *blocks = patched;

    if breaks.is_empty() {
        return Ok(());
    }

    let mut breaks_stack: Vec<(u8, NodeRef<'a>)> = Vec::new();
    let mut warps_out: Vec<NodeRef<'a>> = Vec::new();
    let mut pending_break: Option<NodeRef<'a>> = None;

    for block in blocks.iter().rev() {
        let block = *block;

        if breaks.contains(&node_key(block)) {
            pending_break = None;
            let kind = if warpins(block) == 0 {
                BREAK_ONE_USE
            } else {
                BREAK_INFINITE
            };
            breaks_stack.push((kind, block));
            continue;
        }

        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };

        let target = {
            let borrowed = warp.borrow();
            if !matches!(&*borrowed, Node::ConditionalWarp(_)) {
                if is_flow(&borrowed) {
                    pending_break = None;
                }
                continue;
            }

            let Some(target) = get_target(&borrowed, false) else {
                return Err(internal("a branch without a target"));
            };
            target
        };

        if blocks_set.contains(&node_key(target)) {
            continue;
        }
        if !ends.contains(&node_key(target)) {
            return Err(Error::Unsupported(
                "goto statements are not supported".to_string(),
            ));
        }

        if pending_break.is_none() {
            let top = breaks_stack
                .last()
                .copied()
                .ok_or_else(|| internal("a break without a block to leave to"))?;

            set_target(warp, Some(top.1));

            if top.0 == BREAK_ONE_USE {
                pending_break = breaks_stack.pop().map(|entry| entry.1);
                warps_out.clear();
            } else {
                warps_out.push(block);
            }
        } else {
            set_target(warp, pending_break);
            warps_out.push(block);
        }

        if !traverse::block_contents(block).is_empty() {
            pending_break = None;
        }
    }

    while breaks_stack
        .last()
        .is_some_and(|(kind, _)| *kind == BREAK_INFINITE)
    {
        breaks_stack.pop();
    }

    while !warps_out.is_empty() && !breaks_stack.is_empty() {
        let block = warps_out.pop().expect("checked above");
        let (_, target) = breaks_stack.pop().expect("checked above");
        if let Some(warp) = traverse::block_warp(block) {
            set_target(warp, Some(target));
        }
    }

    Ok(())
}

// MARK: jump ends

/// The blocks a jump chain can end at, starting at `block`.
fn gather_possible_ends<'a>(block: NodeRef<'a>) -> Result<HashSet<usize>> {
    let mut ends: HashSet<usize> = HashSet::new();
    ends.insert(node_key(block));

    let mut block = block;
    while let Some(warp) = traverse::block_warp(block) {
        let target = {
            let borrowed = warp.borrow();
            if !is_jump(&borrowed) {
                break;
            }
            get_target(&borrowed, false)
        };
        let Some(target) = target else {
            break;
        };

        // A cycle of jumps would never end; the set turns it into a stop.
        if !ends.insert(node_key(target)) {
            break;
        }
        block = target;
    }

    Ok(ends)
}

/// A loop that has been built, together with the body it ended up with.
struct BuiltLoop<'a> {
    node: NodeRef<'a>,
    body: Vec<NodeRef<'a>>,
}

// MARK: loop node construction

/// Builds the loop node of a region.
fn unwarp_loop<'a>(
    alloc: &'a Allocator,
    start: NodeRef<'a>,
    end: NodeRef<'a>,
    body: Vec<NodeRef<'a>>,
    expr_body: Option<Vec<NodeRef<'a>>>,
) -> Result<BuiltLoop<'a>> {
    let last = body.last().copied().unwrap_or(start);
    let expr_body = expr_body.unwrap_or_default();

    let warp = traverse::block_warp(start).ok_or_else(|| internal("a loop head without a warp"))?;
    let (is_iterator, is_numeric) = match &*warp.borrow() {
        Node::IteratorWarp(_) => (true, false),
        Node::NumericLoopWarp(_) => (false, true),
        _ => (false, false),
    };

    let last_warp = traverse::block_warp(last).unwrap_or(warp);
    let last_is_unconditional = matches!(&*last_warp.borrow(), Node::UnconditionalWarp(_));

    if is_iterator {
        if !last_is_unconditional
            || !same_optional(get_target(&last_warp.borrow(), false), Some(start))
        {
            return Err(internal("an iterator loop does not jump back to its start"));
        }

        let (first_address, _) = body
            .first()
            .and_then(|block| traverse::block_range(block))
            .ok_or_else(|| internal("an iterator loop without a body"))?;

        let (identifiers, controls) = match &*warp.borrow() {
            Node::IteratorWarp(inner) => (inner.variables, inner.controls),
            _ => unreachable!("checked above"),
        };

        let node_loop = Node::emplace(
            alloc,
            Node::IteratorFor(ArenaBox::new_in(
                IteratorFor {
                    identifiers,
                    expressions: controls,
                    statements: Node::emplace_statements(alloc, body.iter().copied()),
                    meta: Meta::new(first_address, 0),
                },
                &alloc,
            )),
        );

        let first = *body
            .first()
            .ok_or_else(|| internal("an iterator loop without a body"))?;
        set_flow_to(alloc, start, first);

        return Ok(BuiltLoop {
            node: node_loop,
            body,
        });
    }

    if is_numeric {
        if !last_is_unconditional
            || !same_optional(get_target(&last_warp.borrow(), false), Some(start))
        {
            return Err(internal("a numeric loop does not jump back to its start"));
        }

        let (first_address, _) = body
            .first()
            .and_then(|block| traverse::block_range(block))
            .ok_or_else(|| internal("a numeric loop without a body"))?;

        let (index, controls) = match &*warp.borrow() {
            Node::NumericLoopWarp(inner) => (inner.index, inner.controls),
            _ => unreachable!("checked above"),
        };

        let node_loop = Node::emplace(
            alloc,
            Node::NumericFor(ArenaBox::new_in(
                NumericFor {
                    variable: index,
                    expressions: controls,
                    statements: Node::emplace_statements(alloc, body.iter().copied()),
                    meta: Meta::new(first_address, 0),
                },
                &alloc,
            )),
        );

        let first = *body
            .first()
            .ok_or_else(|| internal("a numeric loop without a body"))?;
        set_flow_to(alloc, start, first);

        return Ok(BuiltLoop {
            node: node_loop,
            body,
        });
    }

    if last_is_unconditional {
        if !same_optional(get_target(&last_warp.borrow(), false), Some(start)) {
            return Err(internal("a loop does not jump back to its start"));
        }

        let mut body = body;
        let is_flow_start = is_flow(&warp.borrow());

        let node_loop = if is_flow_start {
            // `while true` and `repeat ... until false`: the condition is
            // constant, and the `repeat` form has a block of its own to drop.
            let previous = if body.len() > 1 {
                body.get(body.len() - 2).copied()
            } else {
                None
            };
            let is_repeat_false = previous.is_some_and(|previous| {
                let Some(warp) = traverse::block_warp(previous) else {
                    return false;
                };
                let borrowed = warp.borrow();
                is_jump(&borrowed)
                    && get_target(&borrowed, false)
                        .is_some_and(|target| traverse::same_node(target, last))
                    && (body.len() <= 2
                        || body
                            .get(body.len() - 3)
                            .and_then(|block| traverse::block_warp(block))
                            .is_some_and(|warp| is_flow(&warp.borrow())))
            });

            if is_repeat_false {
                body.pop();
                let expression = Node::emplace_primitive(alloc, PrimitiveKind::False);
                let inner_statements = Node::emplace_statements(alloc, body.iter().copied());
                Node::emplace(
                    alloc,
                    Node::RepeatUntil(ArenaBox::new_in(
                        RepeatUntil {
                            expression,
                            statements: inner_statements,
                            meta: Meta::default(),
                        },
                        &alloc,
                    )),
                )
            } else {
                let expression = Node::emplace_primitive(alloc, PrimitiveKind::True);
                let inner_statements = Node::emplace_statements(alloc, body.iter().copied());
                Node::emplace(
                    alloc,
                    Node::While(ArenaBox::new_in(
                        While {
                            expression,
                            statements: inner_statements,
                            meta: Meta::default(),
                        },
                        &alloc,
                    )),
                )
            }
        } else {
            // The condition is everything before the first block that flows
            // into the next one.
            let mut i = body.len().saturating_sub(1);
            for (index, block) in body.iter().enumerate() {
                if traverse::block_warp(block).is_some_and(|warp| is_flow(&warp.borrow())) {
                    i = index;
                    break;
                }
            }

            let mut expression = expr_body.clone();
            expression.extend_from_slice(&body[..i]);
            body = body[i..].to_vec();

            fix_expression(&expression, start, end);

            let true_block = *body
                .first()
                .ok_or_else(|| internal("a while loop without a body"))?;

            let condition =
                compile_expression(alloc, &expression, None, Some(true_block), Some(end))?;

            let inner_statements = Node::emplace_statements(alloc, body.iter().copied());
            Node::emplace(
                alloc,
                Node::While(ArenaBox::new_in(
                    While {
                        expression: condition,
                        statements: inner_statements,
                        meta: Meta::default(),
                    },
                    &alloc,
                )),
            )
        };

        fix_nested_ifs(alloc, &mut body, start)?;

        let condition_end = expr_body.last().copied().unwrap_or(start);
        let first = *body
            .first()
            .ok_or_else(|| internal("a loop without a body"))?;
        set_flow_to(alloc, condition_end, first);

        return Ok(BuiltLoop {
            node: node_loop,
            body,
        });
    }

    // `repeat ... until <condition>`: the condition ends with a branch back to
    // the start of the loop.
    if !matches!(&*last_warp.borrow(), Node::ConditionalWarp(_)) {
        return Err(internal("a loop does not end with a jump or a branch"));
    }
    if !same_optional(get_target(&last_warp.borrow(), false), Some(start)) {
        return Err(internal("a repeat loop does not branch back to its start"));
    }

    let mut i = body.len() as isize - 1;
    while i >= 0 {
        let block = body[i as usize];
        let warp = traverse::block_warp(block).ok_or_else(|| internal("a block without a warp"))?;
        let (flow, empty) = {
            let borrowed = warp.borrow();
            (
                is_flow(&borrowed),
                traverse::block_contents(block).is_empty(),
            )
        };

        if flow {
            i += 1;
            break;
        }
        if !empty {
            break;
        }

        i -= 1;
    }

    if i < 0 {
        return Err(internal("a repeat loop without a condition"));
    }

    let mut expression = body[i as usize..].to_vec();
    let mut body = body[..i as usize + 1].to_vec();
    if expression.is_empty() {
        return Err(internal("a repeat loop without a condition"));
    }

    let first = expression[0];
    let first_is_jump = traverse::block_warp(first).is_some_and(|warp| is_jump(&warp.borrow()));
    if first_is_jump {
        // The condition starts with the `break` the region was cut from.
        expression.remove(0);

        if let Some(last) = body.last().copied() {
            let contents = traverse::block_contents(last);
            let mut contents =
                if contents.len() == 1 && matches!(&*contents[0].borrow(), Node::NoOp(_)) {
                    Vec::new()
                } else {
                    contents
                };
            contents.push(Node::emplace(alloc, Node::Break));
            traverse::set_block_contents(alloc, last, contents);
        }
    }

    let false_block = *body
        .first()
        .ok_or_else(|| internal("a repeat loop without a body"))?;

    let expression_last = *expression
        .last()
        .ok_or_else(|| internal("a repeat loop without a condition"))?;
    let true_block = traverse::block_warp(expression_last)
        .and_then(|warp| {
            let borrowed = warp.borrow();
            match &*borrowed {
                Node::ConditionalWarp(inner) => inner.true_target,
                _ => None,
            }
        })
        .ok_or_else(|| internal("a repeat loop condition without a true branch"))?;

    let condition = compile_expression(
        alloc,
        &expression,
        None,
        Some(true_block),
        Some(false_block),
    )?;

    // The first block of the body is the condition of the loop, so a copy takes
    // its place at the top of the body and the original is emptied.
    let start_range = traverse::block_range(start).unwrap_or((0, 0));
    let copy_contents = traverse::block_contents(start);
    let copy = Node::emplace(
        alloc,
        Node::Block(ArenaBox::new_in(
            Block {
                index: block_index(start),
                first_address: start_range.0,
                last_address: start_range.1,
                last_body_address: 0,
                warpins_count: warpins(start),
                is_loop: false,
                contents: ArenaVec::from_iter_in(copy_contents, &alloc),
                warp: None,
            },
            &alloc,
        )),
    );
    traverse::set_block_contents(alloc, start, Vec::new());

    if body.len() > 1 {
        let second = body[1];
        set_flow_to(alloc, copy, second);
    } else {
        set_end(alloc, copy, false);
    }
    set_flow_to(alloc, start, copy);
    body[0] = copy;

    let inner_statements = Node::emplace_statements(alloc, body.iter().copied());
    Ok(BuiltLoop {
        node: Node::emplace(
            alloc,
            Node::RepeatUntil(ArenaBox::new_in(
                RepeatUntil {
                    expression: condition,
                    statements: inner_statements,
                    meta: Meta::default(),
                },
                &alloc,
            )),
        ),
        body,
    })
}

// MARK: nested ifs

/// Gives the last block of a loop body a place to leave the loop from.
///
/// Both targets of a branch cannot point at the same block, so a block is added
/// for anything that jumps to the start of the loop instead of its end.
fn fix_nested_ifs<'a>(
    alloc: &'a Allocator,
    blocks: &mut Vec<NodeRef<'a>>,
    start: NodeRef<'a>,
) -> Result<()> {
    let last_existing = *blocks
        .last()
        .ok_or_else(|| internal("a loop without a body"))?;
    let last = create_next_block(alloc, last_existing);

    let warp = traverse::block_warp(last_existing)
        .ok_or_else(|| internal("a loop body without a warp"))?;
    if matches!(&*warp.borrow(), Node::ConditionalWarp(_)) {
        if let Node::ConditionalWarp(inner) = &mut *warp.borrow_mut() {
            inner.false_target = Some(last);
        }
    } else {
        set_flow_to(alloc, last_existing, last);
    }

    blocks.push(last);
    set_end(alloc, last, false);

    let existing = blocks[..blocks.len() - 1].to_vec();
    for block in existing {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        if get_target(&warp.borrow(), false)
            .is_some_and(|target| traverse::same_node(target, start))
        {
            set_target(warp, Some(last));
        }
    }

    Ok(())
}

/// Points the blocks that jump out of a loop condition at the end of the loop.
fn fix_expression<'a>(blocks: &[NodeRef<'a>], start: NodeRef<'a>, end: NodeRef<'a>) {
    for block in blocks {
        if !traverse::block_contents(block).is_empty() {
            break;
        }

        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let target = get_target(&warp.borrow(), false);
        if target.is_some_and(|target| block_index(target) < block_index(start)) {
            set_target(warp, Some(end));
        }
    }
}

// MARK: validation

/// Checks that everything a loop body branches to is inside it.
fn validate_loop_body<'a>(body: &[NodeRef<'a>]) -> Result<()> {
    for (index, block) in body.iter().enumerate() {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let borrowed = warp.borrow();

        if is_flow(&borrowed) {
            let target =
                get_target(&borrowed, false).ok_or_else(|| internal("a flow without a target"))?;
            if !same_optional(Some(target), body.get(index + 1).copied()) {
                return Err(internal("a loop body does not flow on to the next block"));
            }
        }

        match &*borrowed {
            Node::ConditionalWarp(inner) => {
                for target in [inner.true_target, inner.false_target]
                    .into_iter()
                    .flatten()
                {
                    if !traverse::contains(body, target) {
                        return Err(internal("a branch leaves the loop body"));
                    }
                }
            }
            Node::UnconditionalWarp(inner) => {
                if let Some(target) = inner.target
                    && !traverse::contains(body, target)
                {
                    return Err(internal("a jump leaves the loop body"));
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Checks that every warp of a finished list points inside it.
fn validate_block_list<'a>(blocks: &[NodeRef<'a>]) -> Result<()> {
    for (index, block) in blocks.iter().enumerate() {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let borrowed = warp.borrow();

        if is_flow(&borrowed) {
            let target =
                get_target(&borrowed, false).ok_or_else(|| internal("a flow without a target"))?;
            if !same_optional(Some(target), blocks.get(index + 1).copied()) {
                return Err(internal("a block does not flow on to the next block"));
            }
        }

        match &*borrowed {
            Node::ConditionalWarp(inner) => {
                for target in [inner.true_target, inner.false_target]
                    .into_iter()
                    .flatten()
                {
                    if !traverse::contains(blocks, target) {
                        return Err(internal("a branch leaves the statement list"));
                    }
                }
            }
            _ => {
                if let Some(target) = get_target(&borrowed, true)
                    && !traverse::contains(blocks, target)
                {
                    return Err(internal("a warp leaves the statement list"));
                }
            }
        }
    }

    Ok(())
}

// MARK: block list helpers

/// The block list of a loop node.
fn set_statements<'a>(alloc: &'a Allocator, loop_node: NodeRef<'a>, body: &[NodeRef<'a>]) {
    let statements = match &*loop_node.borrow() {
        Node::While(inner) => Some(inner.statements),
        Node::RepeatUntil(inner) => Some(inner.statements),
        Node::NumericFor(inner) => Some(inner.statements),
        Node::IteratorFor(inner) => Some(inner.statements),
        _ => None,
    };

    if let Some(statements) = statements {
        set_list_contents(alloc, statements, body.iter().copied());
    }
}

/// Whether a block is the marker the compiler left for a loop body.
fn block_is_loop<'a>(block: NodeRef<'a>) -> bool {
    matches!(&*block.borrow(), Node::Block(inner) if inner.is_loop)
}

/// The position of a block in a list.
fn position<'a>(blocks: &[NodeRef<'a>], block: NodeRef<'a>) -> Result<usize> {
    traverse::position(blocks, block).ok_or_else(|| internal("a block is not in the list"))
}
