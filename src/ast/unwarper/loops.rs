//! Rebuilds loops out of the back edges of a block graph.
//!
//! This is a port of the loop half of ljd's `ast/unwarper.py`.
//!
//! A loop is a jump back to a block that comes earlier in the list. The blocks
//! between that target and the jump are the body; a marker block left over from
//! the compiler says where the body starts, and everything between the loop head
//! and that marker is the condition.

use std::collections::HashSet;
use std::rc::Rc;

use crate::error::{Error, Result};

use super::super::nodes::*;
use super::super::traverse;
use super::expressions::compile_expression;
use super::*;

/// Which kind of `break` a block holds, for the pass that retargets the warps
/// that leave a loop.
const BREAK_INFINITE: u8 = 0;
const BREAK_ONE_USE: u8 = 1;

/// Ends the loops of one block list.
pub fn fix_loops(blocks: Vec<NodeRef>, repeat_until: bool) -> Result<Vec<NodeRef>> {
    let mut blocks = blocks;
    let loops = find_all_loops(&blocks, repeat_until)?;
    if loops.is_empty() {
        return Ok(blocks);
    }

    let fixed = cleanup_breaks_and_if_ends(&loops, &mut blocks)?;
    let mut replacements: Vec<(NodeRef, NodeRef)> = Vec::new();
    let lookup = |replacements: &[(NodeRef, NodeRef)], node: &NodeRef| -> NodeRef {
        for (from, to) in replacements {
            if Rc::ptr_eq(from, node) {
                return to.clone();
            }
        }
        node.clone()
    };

    for (start, end) in fixed {
        let end = lookup(&replacements, &end);
        let mut start_index = position(&blocks, &start)?;
        let end_index = position(&blocks, &end)?;
        // The outermost block that marks the start of the body.
        let mut loop_block = None;
        for block in &blocks[start_index..end_index] {
            if block_is_loop(block) {
                loop_block = Some(block.clone());
                break;
            }
        }

        // A `for ... in` loop has no marker of its own, and using the one the
        // iterator warp sits on would hide the call that produces the values.
        if matches!(&*start.borrow(), Node::Block(inner)
            if inner.warp.as_ref().is_some_and(|warp| matches!(&*warp.borrow(), Node::IteratorWarp(_))))
        {
            loop_block = None;
        }

        let Some(loop_block) = loop_block else {
            blocks = handle_single_loop(&start, &end, blocks, repeat_until)?;
            continue;
        };

        let mut body_start_index = position(&blocks, &loop_block)?;

        // Skip the marker instruction; a `repeat` loop keeps its first block,
        // because that is where the user's code starts.
        if body_start_index == start_index && !repeat_until {
            body_start_index += 1;
        }

        let expr_body = blocks[start_index..body_start_index].to_vec();
        let body = blocks[body_start_index..end_index].to_vec();
        let mut built = unwarp_loop(&start, &end, body, Some(expr_body))?;
        let block = loop_build_block(
            &start,
            &mut built.body,
            &end,
            &blocks,
            &built.node,
            body_start_index as u32,
        )?;
        set_statements(&built.node, &built.body);

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
                    let old_start = blocks[start_index].clone();
                    replace_targets(&before, &old_start, &block, false);
                    replacements.push((old_start, block.clone()));
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

/// Verifies that no back edge is left over.
///
/// This is a check rather than a step: the loops are all built by
/// [`fix_loops`], and anything left here means the graph was not understood.
pub fn unwarp_loops(blocks: Vec<NodeRef>, repeat_until: bool) -> Result<Vec<NodeRef>> {
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
fn find_all_loops(blocks: &[NodeRef], repeat_until: bool) -> Result<Vec<(NodeRef, NodeRef)>> {
    let mut loops: Vec<(NodeRef, NodeRef)> = Vec::new();

    // Two jumps back to the same block are the same loop: all but the last one
    // are `continue` statements the compiler put in, so the loop they describe
    // is dropped again.
    let mut starts: Vec<(usize, (NodeRef, NodeRef))> = Vec::new();

    let mut i = 0usize;
    while i < blocks.len() {
        let block = blocks[i].clone();
        let Some(warp) = traverse::block_warp(&block) else {
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

            if block_index(&start) <= block_index(&block) {
                if repeat_until {
                    return Err(internal("a back edge in a repeat pass"));
                }
                if i + 1 >= blocks.len() {
                    return Err(internal("a jump back from the last block"));
                }

                let entry = (start.clone(), blocks[i + 1].clone());
                let previous = starts
                    .iter()
                    .find(|(key, _)| *key == node_key(&start))
                    .map(|(_, entry)| entry.clone());
                if let Some(previous) = previous
                    && let Some(index) = loops
                        .iter()
                        .position(|(s, e)| Rc::ptr_eq(s, &previous.0) && Rc::ptr_eq(e, &previous.1))
                {
                    loops.remove(index);
                }

                loops.push(entry.clone());
                starts.retain(|(key, _)| *key != node_key(&start));
                starts.push((node_key(&start), entry));
            }
        } else if repeat_until {
            let false_target = match &*warp.borrow() {
                Node::ConditionalWarp(inner) => inner.false_target.clone(),
                _ => None,
            };
            let Some(false_target) = false_target else {
                i += 1;
                continue;
            };

            if block_index(&false_target) > block_index(&block) {
                i += 1;
                continue;
            }

            let mut start = false_target;
            let first = block.clone();
            let mut end = block.clone();
            let mut last_i = i;

            // The end of the expression is the last jump back to the loop.
            while i < blocks.len() {
                let block = blocks[i].clone();
                let Some(warp) = traverse::block_warp(&block) else {
                    break;
                };
                let borrowed = warp.borrow();

                if !Rc::ptr_eq(&block, &first) && !traverse::block_contents(&block).is_empty() {
                    break;
                }
                if matches!(&*borrowed, Node::EndWarp(_)) {
                    break;
                }

                let Some(target) = get_target(&borrowed, false) else {
                    break;
                };
                if block_index(&target) < block_index(&block) {
                    if Rc::ptr_eq(&target, &start) {
                        start = target;
                        end = block.clone();
                        last_i = i;
                    } else {
                        break;
                    }
                }

                i += 1;
            }

            i = last_i;

            let end_index = position(blocks, &end)?;
            let end = blocks
                .get(end_index + 1)
                .cloned()
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

/// Retargets the jumps back to the start of nested loops, so that each one ends
/// at the block that follows its body.
fn cleanup_breaks_and_if_ends(
    loops: &[(NodeRef, NodeRef)],
    blocks: &mut Vec<NodeRef>,
) -> Result<Vec<(NodeRef, NodeRef)>> {
    let mut outer_start_index: Option<u32> = None;
    let mut outer_end: Option<NodeRef> = None;
    let mut current_start_index: Option<u32> = None;
    let mut current_end: Option<NodeRef> = None;

    let mut fixed: Vec<(NodeRef, NodeRef)> = Vec::new();

    for (start, end) in loops {
        let start_index = block_index(start);
        let is_nested =
            Some(start_index) == outer_start_index || Some(start_index) == current_start_index;

        if is_nested {
            let end_index = position(blocks, end)?;
            let last_in_body = blocks
                .get(end_index.wrapping_sub(1))
                .cloned()
                .ok_or_else(|| internal("a loop body that is empty"))?;
            let warp = traverse::block_warp(&last_in_body)
                .ok_or_else(|| internal("a loop body without a warp"))?;
            if !matches!(&*warp.borrow(), Node::UnconditionalWarp(_)) {
                return Err(internal("a loop does not end with a jump"));
            }
            if !same_optional(get_target(&warp.borrow(), false).as_ref(), Some(start)) {
                return Err(internal("a loop back edge does not reach its start"));
            }

            if Some(start_index) == outer_start_index {
                let outer_end = outer_end
                    .clone()
                    .ok_or_else(|| internal("a nested loop without an outer end"))?;
                let outer_end_index = position(blocks, &outer_end)?;
                let replacement = blocks
                    .get(outer_end_index.wrapping_sub(1))
                    .cloned()
                    .ok_or_else(|| internal("an outer loop without a body"))?;
                set_target(&warp, Some(replacement));
            } else {
                let current_end = current_end
                    .clone()
                    .ok_or_else(|| internal("a nested loop without an end"))?;
                let current_end_index = position(blocks, &current_end)?;

                let mut last = blocks
                    .get(current_end_index.wrapping_sub(1))
                    .cloned()
                    .ok_or_else(|| internal("a loop without a body"))?;

                if Rc::ptr_eq(&last, end) {
                    // The end of the loop is also the last block of the body,
                    // so a block has to be made for the jump to land on.
                    let new_block = create_next_block(&last);
                    let last_warp = traverse::block_warp(&last)
                        .ok_or_else(|| internal("a loop end without a warp"))?;
                    traverse::set_block_warp(&new_block, last_warp);
                    set_flow_to(&last, &new_block);
                    blocks.insert(current_end_index, new_block.clone());
                    last = new_block;
                }

                if !traverse::block_contents(&last).is_empty() {
                    return Err(internal("a loop end holds statements"));
                }
                let last_warp = traverse::block_warp(&last)
                    .ok_or_else(|| internal("a loop end without a warp"))?;
                if !matches!(&*last_warp.borrow(), Node::UnconditionalWarp(_)) {
                    return Err(internal("a loop end without a jump"));
                }
                if !same_optional(get_target(&last_warp.borrow(), false).as_ref(), Some(start)) {
                    return Err(internal("a loop end does not jump back"));
                }

                set_target(&warp, Some(last));
            }
        } else {
            fixed.push((start.clone(), end.clone()));

            let nested = match (&current_end, current_start_index) {
                (Some(current_end), Some(current_start_index)) => {
                    current_start_index < start_index
                        && block_index(current_end) >= block_index(end)
                }
                _ => false,
            };

            if nested {
                outer_start_index = current_start_index;
                outer_end = current_end.clone();
            } else {
                outer_start_index = None;
                outer_end = None;
            }

            current_start_index = Some(start_index);
            current_end = Some(end.clone());
        }
    }

    Ok(fixed)
}

/// Builds the loop of a region that has no marker block of its own.
fn handle_single_loop(
    start: &NodeRef,
    end: &NodeRef,
    blocks: Vec<NodeRef>,
    repeat_until: bool,
) -> Result<Vec<NodeRef>> {
    let start_index = position(&blocks, start)?;
    let end_index = position(&blocks, end)?;

    let body = if repeat_until {
        blocks[start_index..end_index].to_vec()
    } else {
        blocks[start_index + 1..end_index].to_vec()
    };

    let mut built = unwarp_loop(start, end, body, None)?;
    let block = loop_build_block(
        start,
        &mut built.body,
        end,
        &blocks,
        &built.node,
        block_index(start) + 1,
    )?;
    set_statements(&built.node, &built.body);

    let mut new_blocks = blocks[..start_index + 1].to_vec();
    new_blocks.push(block);
    new_blocks.extend_from_slice(&blocks[end_index..]);
    Ok(new_blocks)
}

/// Puts the loop into a block of its own and turns the jumps back into it into
/// `break`s.
fn loop_build_block(
    start: &NodeRef,
    body: &mut Vec<NodeRef>,
    end: &NodeRef,
    blocks: &[NodeRef],
    loop_node: &NodeRef,
    index: u32,
) -> Result<NodeRef> {
    let first = body
        .first()
        .cloned()
        .ok_or_else(|| internal("a loop without a body"))?;
    let last = body
        .last()
        .cloned()
        .ok_or_else(|| internal("a loop without a body"))?;
    let (first_address, _) = traverse::block_range(&first).unwrap_or((0, 0));
    let (_, last_address) = traverse::block_range(&last).unwrap_or((0, 0));

    let block = node(Node::Block(Box::new(Block {
        index,
        first_address,
        last_address,
        last_body_address: 0,
        warpins_count: 0,
        is_loop: false,
        contents: vec![loop_node.clone()],
        warp: None,
    })));
    set_flow_to(&block, end);

    replace_targets(blocks, &first, &block, false);

    // The end block is left without a target: it may point outside the loop,
    // which would confuse the checks that follow.
    set_end(&last, true);

    unwarp_breaks(start, body, end)?;
    Ok(block)
}

/// Rewrites the jumps that leave a loop into `break` statements.
fn unwarp_breaks(start: &NodeRef, blocks: &mut Vec<NodeRef>, next_block: &NodeRef) -> Result<()> {
    let mut blocks_set: HashSet<usize> = HashSet::new();
    blocks_set.insert(node_key(start));
    for block in blocks.iter() {
        blocks_set.insert(node_key(block));
    }

    let ends = gather_possible_ends(next_block)?;

    let mut breaks: HashSet<usize> = HashSet::new();
    let mut patched: Vec<NodeRef> = Vec::new();
    let length = blocks.len();

    for (i, block) in blocks.iter().enumerate() {
        let block = block.clone();
        let Some(warp) = traverse::block_warp(&block) else {
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
        if blocks_set.contains(&node_key(&target)) {
            patched.push(block);
            continue;
        }
        if !ends.contains(&node_key(&target)) {
            return Err(Error::Unsupported(
                "goto statements are not supported".to_string(),
            ));
        }

        let contents = traverse::block_contents(&block);
        let is_placeholder = contents.len() == 1 && matches!(&*contents[0].borrow(), Node::NoOp(_));

        let block = if warpins(&block) != 0 && !is_placeholder {
            // The block is jumped to as well, so the `break` gets a block of
            // its own and the original keeps flowing into it.
            let new_block = create_next_block(&block);
            set_flow_to(&block, &new_block);
            patched.push(block);
            patched.push(new_block.clone());
            new_block
        } else {
            patched.push(block.clone());
            block
        };

        let contents = traverse::block_contents(&block);
        let mut contents = if contents.len() == 1 && matches!(&*contents[0].borrow(), Node::NoOp(_))
        {
            Vec::new()
        } else {
            contents
        };
        contents.push(node(Node::Break));
        traverse::set_block_contents(&block, contents);

        if i + 1 == length {
            set_end(&block, false);
        } else {
            let next = blocks[i + 1].clone();
            set_flow_to(&block, &next);
        }

        breaks.insert(node_key(&block));
    }

    *blocks = patched;

    if breaks.is_empty() {
        return Ok(());
    }

    let mut breaks_stack: Vec<(u8, NodeRef)> = Vec::new();
    let mut warps_out: Vec<NodeRef> = Vec::new();
    let mut pending_break: Option<NodeRef> = None;

    for block in blocks.iter().rev() {
        let block = block.clone();

        if breaks.contains(&node_key(&block)) {
            pending_break = None;
            let kind = if warpins(&block) == 0 {
                BREAK_ONE_USE
            } else {
                BREAK_INFINITE
            };
            breaks_stack.push((kind, block));
            continue;
        }

        let Some(warp) = traverse::block_warp(&block) else {
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

        if blocks_set.contains(&node_key(&target)) {
            continue;
        }
        if !ends.contains(&node_key(&target)) {
            return Err(Error::Unsupported(
                "goto statements are not supported".to_string(),
            ));
        }

        if pending_break.is_none() {
            let top = breaks_stack
                .last()
                .cloned()
                .ok_or_else(|| internal("a break without a block to leave to"))?;

            set_target(&warp, Some(top.1.clone()));

            if top.0 == BREAK_ONE_USE {
                pending_break = breaks_stack.pop().map(|entry| entry.1);
                warps_out.clear();
            } else {
                warps_out.push(block.clone());
            }
        } else {
            let target = pending_break.clone();
            set_target(&warp, target);
            warps_out.push(block.clone());
        }

        if !traverse::block_contents(&block).is_empty() {
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
        if let Some(warp) = traverse::block_warp(&block) {
            set_target(&warp, Some(target));
        }
    }

    Ok(())
}

/// The blocks a jump chain can end at, starting at `block`.
fn gather_possible_ends(block: &NodeRef) -> Result<HashSet<usize>> {
    let mut ends: HashSet<usize> = HashSet::new();
    ends.insert(node_key(block));

    let mut block = block.clone();
    while let Some(warp) = traverse::block_warp(&block) {
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
        if !ends.insert(node_key(&target)) {
            break;
        }
        block = target;
    }

    Ok(ends)
}

/// A loop that has been built, together with the body it ended up with.
struct BuiltLoop {
    node: NodeRef,
    body: Vec<NodeRef>,
}

/// Builds the loop node of a region.
fn unwarp_loop(
    start: &NodeRef,
    end: &NodeRef,
    body: Vec<NodeRef>,
    expr_body: Option<Vec<NodeRef>>,
) -> Result<BuiltLoop> {
    let last = body.last().cloned().unwrap_or_else(|| start.clone());
    let expr_body = expr_body.unwrap_or_default();

    let warp = traverse::block_warp(start).ok_or_else(|| internal("a loop head without a warp"))?;
    let (is_iterator, is_numeric) = match &*warp.borrow() {
        Node::IteratorWarp(_) => (true, false),
        Node::NumericLoopWarp(_) => (false, true),
        _ => (false, false),
    };

    let last_warp = traverse::block_warp(&last).unwrap_or_else(|| warp.clone());
    let last_is_unconditional = matches!(&*last_warp.borrow(), Node::UnconditionalWarp(_));

    if is_iterator {
        if !last_is_unconditional
            || !same_optional(get_target(&last_warp.borrow(), false).as_ref(), Some(start))
        {
            return Err(internal("an iterator loop does not jump back to its start"));
        }

        let (first_address, _) = body
            .first()
            .and_then(traverse::block_range)
            .ok_or_else(|| internal("an iterator loop without a body"))?;

        let (identifiers, controls) = match &*warp.borrow() {
            Node::IteratorWarp(inner) => (inner.variables.clone(), inner.controls.clone()),
            _ => unreachable!("checked above"),
        };

        let node_loop = node(Node::IteratorFor(Box::new(IteratorFor {
            identifiers,
            expressions: controls,
            statements: statements(body.clone()),
            meta: Meta::new(first_address, 0),
        })));

        let first = body
            .first()
            .cloned()
            .ok_or_else(|| internal("an iterator loop without a body"))?;
        set_flow_to(start, &first);

        return Ok(BuiltLoop {
            node: node_loop,
            body,
        });
    }

    if is_numeric {
        if !last_is_unconditional
            || !same_optional(get_target(&last_warp.borrow(), false).as_ref(), Some(start))
        {
            return Err(internal("a numeric loop does not jump back to its start"));
        }

        let (first_address, _) = body
            .first()
            .and_then(traverse::block_range)
            .ok_or_else(|| internal("a numeric loop without a body"))?;

        let (index, controls) = match &*warp.borrow() {
            Node::NumericLoopWarp(inner) => (inner.index.clone(), inner.controls.clone()),
            _ => unreachable!("checked above"),
        };

        let node_loop = node(Node::NumericFor(Box::new(NumericFor {
            variable: index,
            expressions: controls,
            statements: statements(body.clone()),
            meta: Meta::new(first_address, 0),
        })));

        let first = body
            .first()
            .cloned()
            .ok_or_else(|| internal("a numeric loop without a body"))?;
        set_flow_to(start, &first);

        return Ok(BuiltLoop {
            node: node_loop,
            body,
        });
    }

    if last_is_unconditional {
        if !same_optional(get_target(&last_warp.borrow(), false).as_ref(), Some(start)) {
            return Err(internal("a loop does not jump back to its start"));
        }

        let mut body = body;
        let is_flow_start = is_flow(&warp.borrow());

        let node_loop = if is_flow_start {
            // `while true` and `repeat ... until false`: the condition is
            // constant, and the `repeat` form has a block of its own to drop.
            let previous = if body.len() > 1 {
                body.get(body.len() - 2).cloned()
            } else {
                None
            };
            let is_repeat_false = previous.is_some_and(|previous| {
                let Some(warp) = traverse::block_warp(&previous) else {
                    return false;
                };
                let borrowed = warp.borrow();
                is_jump(&borrowed)
                    && get_target(&borrowed, false).is_some_and(|target| Rc::ptr_eq(&target, &last))
                    && (body.len() <= 2
                        || body
                            .get(body.len() - 3)
                            .and_then(traverse::block_warp)
                            .is_some_and(|warp| is_flow(&warp.borrow())))
            });

            if is_repeat_false {
                body.pop();
                node(Node::RepeatUntil(Box::new(RepeatUntil {
                    expression: primitive(PrimitiveKind::False),
                    statements: statements(body.clone()),
                    meta: Meta::default(),
                })))
            } else {
                node(Node::While(Box::new(While {
                    expression: primitive(PrimitiveKind::True),
                    statements: statements(body.clone()),
                    meta: Meta::default(),
                })))
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

            let true_block = body
                .first()
                .cloned()
                .ok_or_else(|| internal("a while loop without a body"))?;

            let condition = compile_expression(&expression, None, Some(&true_block), Some(end))?;

            node(Node::While(Box::new(While {
                expression: condition,
                statements: statements(body.clone()),
                meta: Meta::default(),
            })))
        };

        fix_nested_ifs(&mut body, start)?;

        let condition_end = expr_body.last().cloned().unwrap_or_else(|| start.clone());
        let first = body
            .first()
            .cloned()
            .ok_or_else(|| internal("a loop without a body"))?;
        set_flow_to(&condition_end, &first);

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
    if !same_optional(get_target(&last_warp.borrow(), false).as_ref(), Some(start)) {
        return Err(internal("a repeat loop does not branch back to its start"));
    }

    let mut i = body.len() as isize - 1;
    while i >= 0 {
        let block = body[i as usize].clone();
        let warp =
            traverse::block_warp(&block).ok_or_else(|| internal("a block without a warp"))?;
        let (flow, empty) = {
            let borrowed = warp.borrow();
            (
                is_flow(&borrowed),
                traverse::block_contents(&block).is_empty(),
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

    let first = expression[0].clone();
    let first_is_jump = traverse::block_warp(&first).is_some_and(|warp| is_jump(&warp.borrow()));
    if first_is_jump {
        // The condition starts with the `break` the region was cut from.
        expression.remove(0);

        if let Some(last) = body.last().cloned() {
            let contents = traverse::block_contents(&last);
            let mut contents =
                if contents.len() == 1 && matches!(&*contents[0].borrow(), Node::NoOp(_)) {
                    Vec::new()
                } else {
                    contents
                };
            contents.push(node(Node::Break));
            traverse::set_block_contents(&last, contents);
        }
    }

    let false_block = body
        .first()
        .cloned()
        .ok_or_else(|| internal("a repeat loop without a body"))?;

    let expression_last = expression
        .last()
        .cloned()
        .ok_or_else(|| internal("a repeat loop without a condition"))?;
    let true_block = traverse::block_warp(&expression_last)
        .and_then(|warp| {
            let borrowed = warp.borrow();
            match &*borrowed {
                Node::ConditionalWarp(inner) => inner.true_target.clone(),
                _ => None,
            }
        })
        .ok_or_else(|| internal("a repeat loop condition without a true branch"))?;

    let condition = compile_expression(&expression, None, Some(&true_block), Some(&false_block))?;

    // The first block of the body is the condition of the loop, so a copy takes
    // its place at the top of the body and the original is emptied.
    let copy = node(Node::Block(Box::new(Block {
        index: block_index(start),
        first_address: traverse::block_range(start).unwrap_or((0, 0)).0,
        last_address: traverse::block_range(start).unwrap_or((0, 0)).1,
        last_body_address: 0,
        warpins_count: warpins(start),
        is_loop: false,
        contents: traverse::block_contents(start),
        warp: None,
    })));
    traverse::set_block_contents(start, Vec::new());

    if body.len() > 1 {
        let second = body[1].clone();
        set_flow_to(&copy, &second);
    } else {
        set_end(&copy, false);
    }
    set_flow_to(start, &copy);
    body[0] = copy;

    Ok(BuiltLoop {
        node: node(Node::RepeatUntil(Box::new(RepeatUntil {
            expression: condition,
            statements: statements(body.clone()),
            meta: Meta::default(),
        }))),
        body,
    })
}

/// Gives the last block of a loop body a place to leave the loop from.
///
/// Both targets of a branch cannot point at the same block, so a block is added
/// for anything that jumps to the start of the loop instead of its end.
fn fix_nested_ifs(blocks: &mut Vec<NodeRef>, start: &NodeRef) -> Result<()> {
    let last_existing = blocks
        .last()
        .cloned()
        .ok_or_else(|| internal("a loop without a body"))?;
    let last = create_next_block(&last_existing);

    let warp = traverse::block_warp(&last_existing)
        .ok_or_else(|| internal("a loop body without a warp"))?;
    if matches!(&*warp.borrow(), Node::ConditionalWarp(_)) {
        if let Node::ConditionalWarp(inner) = &mut *warp.borrow_mut() {
            inner.false_target = Some(last.clone());
        }
    } else {
        set_flow_to(&last_existing, &last);
    }

    blocks.push(last.clone());
    set_end(&last, false);

    let existing = blocks[..blocks.len() - 1].to_vec();
    for block in existing {
        let Some(warp) = traverse::block_warp(&block) else {
            continue;
        };
        if get_target(&warp.borrow(), false).is_some_and(|target| Rc::ptr_eq(&target, start)) {
            set_target(&warp, Some(last.clone()));
        }
    }

    Ok(())
}

/// Points the blocks that jump out of a loop condition at the end of the loop.
fn fix_expression(blocks: &[NodeRef], start: &NodeRef, end: &NodeRef) {
    for block in blocks {
        if !traverse::block_contents(block).is_empty() {
            break;
        }

        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let target = get_target(&warp.borrow(), false);
        if target.is_some_and(|target| block_index(&target) < block_index(start)) {
            set_target(&warp, Some(end.clone()));
        }
    }
}

/// Checks that everything a loop body branches to is inside it.
fn validate_loop_body(body: &[NodeRef]) -> Result<()> {
    for (index, block) in body.iter().enumerate() {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let borrowed = warp.borrow();

        if is_flow(&borrowed) {
            let target =
                get_target(&borrowed, false).ok_or_else(|| internal("a flow without a target"))?;
            if !same_optional(Some(&target), body.get(index + 1)) {
                return Err(internal("a loop body does not flow on to the next block"));
            }
        }

        match &*borrowed {
            Node::ConditionalWarp(inner) => {
                for target in [&inner.true_target, &inner.false_target]
                    .into_iter()
                    .flatten()
                {
                    if !traverse::contains(body, target) {
                        return Err(internal("a branch leaves the loop body"));
                    }
                }
            }
            Node::UnconditionalWarp(inner) => {
                if let Some(target) = &inner.target
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
fn validate_block_list(blocks: &[NodeRef]) -> Result<()> {
    for (index, block) in blocks.iter().enumerate() {
        let Some(warp) = traverse::block_warp(block) else {
            continue;
        };
        let borrowed = warp.borrow();

        if is_flow(&borrowed) {
            let target =
                get_target(&borrowed, false).ok_or_else(|| internal("a flow without a target"))?;
            if !same_optional(Some(&target), blocks.get(index + 1)) {
                return Err(internal("a block does not flow on to the next block"));
            }
        }

        match &*borrowed {
            Node::ConditionalWarp(inner) => {
                for target in [&inner.true_target, &inner.false_target]
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
                    && !traverse::contains(blocks, &target)
                {
                    return Err(internal("a warp leaves the statement list"));
                }
            }
        }
    }

    Ok(())
}

/// The block list of a loop node.
fn set_statements(loop_node: &NodeRef, body: &[NodeRef]) {
    let statements = match &*loop_node.borrow() {
        Node::While(inner) => Some(inner.statements.clone()),
        Node::RepeatUntil(inner) => Some(inner.statements.clone()),
        Node::NumericFor(inner) => Some(inner.statements.clone()),
        Node::IteratorFor(inner) => Some(inner.statements.clone()),
        _ => None,
    };

    if let Some(statements) = statements {
        traverse::set_list_contents(&statements, body.to_vec());
    }
}

/// Whether a block is the marker the compiler left for a loop body.
fn block_is_loop(block: &NodeRef) -> bool {
    matches!(&*block.borrow(), Node::Block(inner) if inner.is_loop)
}

/// The position of a block in a list.
fn position(blocks: &[NodeRef], block: &NodeRef) -> Result<usize> {
    traverse::position(blocks, block).ok_or_else(|| internal("a block is not in the list"))
}
