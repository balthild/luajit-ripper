//! Turning short circuit branches back into `and`/`or` expressions.
//!
//! This is a port of the expression half of ljd's `ast/unwarper.py`.
//!
//! A logical expression is compiled into branches that skip the rest of it:
//! `a and b` evaluates `a` and jumps past `b` when `a` is false, while `a or b`
//! jumps past `b` when `a` is true. What is left in the graph is a small region
//! with a true terminator, a false terminator and an end, which this pass
//! recognises and turns back into an expression.
//!
//! The terminator of a region is the block it leaves through when the
//! expression succeeds or fails. The last block of a region is special: both of
//! its branches point at a terminator, so it has no subexpression of its own
//! and only contributes its condition. That is where the recursion stops.
//!
//! The tricky part is deciding where a subexpression ends. ljd's answer is to
//! be greedy: a subexpression takes as many blocks as it can, stopping only
//! where the operator to its right changes. [`unwarp_expression`] is what does
//! that.

use std::collections::HashSet;

use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use super::super::helpers::is_equal;
use super::super::nodes::*;
use super::super::{slotworks, traverse};
use super::*;
use crate::error::{Error, Result};

// MARK: entry point

/// Replaces every short circuit region of a block list with an expression.
pub fn unwarp_expressions<'a>(
    alloc: &'a Allocator,
    blocks: Vec<NodeRef<'a>>,
    recovery: Recovery,
) -> Result<Vec<NodeRef<'a>>> {
    let mut blocks = blocks;
    let mut pack: Vec<Expression<'a>> = Vec::new();
    let mut packed: HashSet<usize> = HashSet::new();

    let mut start_index = 0usize;
    let mut end_index = 0usize;

    while start_index < blocks.len() - 1 {
        let start = blocks[start_index];

        if let Some(warp) = traverse::block_warp(start) {
            let borrowed = warp.borrow();
            if is_flow(&borrowed) {
                start_index += 1;
                continue;
            }
            // A jump that already holds statements can end an expression that
            // started earlier, so it is only skipped when it cannot.
            if is_jump(&borrowed)
                && start_index > 0
                && !traverse::block_contents(start).is_empty()
                && (start_index != end_index || !contains_primitive_condition(start))
            {
                start_index += 1;
                end_index += 1;
                continue;
            }
        }

        let Some((body, end, next_index)) = extract_if_body(start_index, &blocks, None) else {
            return Err(Error::Unsupported(
                "goto statements are not supported".to_string(),
            ));
        };
        end_index = next_index;

        if start_index > 0 && body.len() == 1 && warpins(body[0]) == 0 {
            // An unreachable branch only tests a constant, so nothing that
            // could be an expression was skipped.
            let contents = traverse::block_contents(body[0]);
            let is_constant = contents.last().is_some_and(|last| {
                matches!(&*last.borrow(), Node::Assignment(inner)
                    if traverse::list_contents(inner.expressions)
                        .last()
                        .is_some_and(|value| matches!(&*value.borrow(), Node::Primitive(_))))
            });
            if is_constant && start_index + 1 < blocks.len() {
                start_index += 1;
                continue;
            }
        }

        let mut known_blocks: HashSet<usize> = HashSet::new();
        let found = find_expressions(alloc, start, &body, end, 0, &mut known_blocks);

        // A region the matcher cannot read is left as it is and stepped over:
        // whatever it holds is written as plain statements rather than as an
        // expression of the enclosing one.
        let (expressions, unused) = match found {
            Ok(found) => found,
            Err(error) => {
                if recovery == Recovery::Off {
                    return Err(error);
                }
                mark_error(start);
                (Vec::new(), Vec::new())
            }
        };

        if expressions.is_empty() {
            start_index += 1;
            continue;
        }

        // A block belongs to exactly one expression.
        for expression in &expressions {
            if !packed.insert(node_key(expression.block)) {
                return Err(internal("a block was packed into two expressions"));
            }
        }

        let endest_end = find_endest_end(&expressions);
        if !traverse::same_node(endest_end, end) {
            end_index = traverse::position(&blocks, endest_end)
                .ok_or_else(|| internal("an expression ends outside its region"))?;
        }

        if !unused.is_empty() {
            let expression_start_index = traverse::position(&blocks, expressions[0].start)
                .ok_or_else(|| internal("an expression starts outside its region"))?;
            if expression_start_index > start_index + 1 {
                // The gap may hold expressions that were skipped over, so they
                // are packed too; otherwise they would be lost.
                let mut missed = Vec::new();
                for entry in &unused {
                    for index in start_index + 1..end_index.saturating_sub(1) {
                        if index < blocks.len() && traverse::same_node(entry.block, blocks[index]) {
                            missed.push(entry.clone());
                            break;
                        }
                    }
                }
                pack.extend(missed.into_iter().rev());
            }
        }

        pack.extend(expressions.into_iter().rev());
        start_index = end_index;
    }

    unwarp_expressions_pack(alloc, &mut blocks, &pack, recovery)?;
    Ok(blocks)
}

// MARK: expressions

/// One region that turned out to be an expression.
///
/// `slot` is the register the expression's value ends up in, which is what
/// ties the branches of an expression together.
#[derive(Clone)]
struct Expression<'a> {
    /// The block the value is computed in.
    block: NodeRef<'a>,
    /// The block the region starts at.
    start: NodeRef<'a>,
    /// The block the region ends at.
    end: NodeRef<'a>,
    /// The register the value is kept in, `-1` while unknown.
    slot: i64,
    /// What that register is, once it is known.
    slot_type: Option<IdentifierKind>,
    /// The identifier the value is written into.
    slot_ref: Option<NodeRef<'a>>,
    /// Whether the result has to be checked before it is inlined.
    needs_validation: bool,
}

impl<'a> Expression<'a> {
    fn same_as(&self, other: &Expression<'a>) -> bool {
        traverse::same_node(self.block, other.block)
            && traverse::same_node(self.start, other.start)
            && traverse::same_node(self.end, other.end)
            && self.slot == other.slot
            && self.needs_validation == other.needs_validation
    }
}

/// The end of the expression that reaches furthest.
fn find_endest_end<'a>(expressions: &[Expression<'a>]) -> NodeRef<'a> {
    let mut endest = expressions[0].end;
    for expression in &expressions[1..] {
        if block_index(expression.end) > block_index(endest) {
            endest = expression.end;
        }
    }
    endest
}

/// Looks for the expression that starts at `start` and ends at `end`.
///
/// Returns the parts of the expression, innermost first, and the regions that
/// were looked at but turned out not to be expressions.
fn find_expressions<'a>(
    alloc: &'a Allocator,
    start: NodeRef<'a>,
    body: &[NodeRef<'a>],
    end: NodeRef<'a>,
    level: usize,
    known_blocks: &mut HashSet<usize>,
) -> Result<(Vec<Expression<'a>>, Vec<Expression<'a>>)> {
    known_blocks.insert(node_key(start));
    add_warps_to_known_blocks(start, known_blocks);

    // A local that is assigned a comparison is an expression as well, as in
    // `local a = x ~= "b"`.
    let (mut slot, mut slot_type, mut slot_ref) = simple_local_assignment_slot(body);

    let mut slot_assignments: Vec<NodeRef<'a>> = Vec::new();
    let mut expressions: Vec<Expression<'a>> = Vec::new();
    let mut unused: Vec<Expression<'a>> = Vec::new();

    let extbody: Vec<NodeRef<'a>> = std::iter::once(start).chain(body.iter().copied()).collect();

    let mut is_local = false;
    let mut sure_expression: Option<bool> = None;
    let mut needs_validation = false;
    let mut block = start;
    let body = body.to_vec();

    let mut i = 0usize;
    while i < extbody.len() {
        let current_i = i;
        i += 1;
        block = extbody[current_i];

        if known_blocks.contains(&node_key(block)) {
            add_warps_to_known_blocks(block, known_blocks);
        }

        // A conditional of its own is processed first and then skipped over.
        let mut subs: Vec<Expression<'a>> = Vec::new();
        let mut subs_unused: Vec<Expression<'a>> = Vec::new();

        if let Some(branch_end) = find_branching_end(&extbody[current_i..], None)
            && let Some(be_index) = traverse::position(&extbody, branch_end)
        {
            i = be_index;

            let sub_body = extbody[current_i + 1..be_index].to_vec();
            let (found, skipped) =
                find_expressions(alloc, block, &sub_body, branch_end, level + 1, known_blocks)?;
            subs = found;
            subs_unused = skipped;
        }

        if !subs.is_empty() {
            let endest_end = find_endest_end(&subs);
            let new_i = traverse::position(&extbody, endest_end)
                .ok_or_else(|| internal("a subexpression ends outside its region"))?;
            if new_i <= current_i {
                return Err(internal("a subexpression does not make progress"));
            }

            // A local, or anything that is not an assignment, means this was an
            // `if` after all and not an expression.
            for sub in &subs {
                if sub.slot_type == Some(IdentifierKind::Local) {
                    return Ok((expressions, unused));
                }

                let sub_start_i = traverse::position(&extbody, sub.start)
                    .ok_or_else(|| internal("a subexpression starts outside its region"))?;
                let sub_end_i = traverse::position(&extbody, sub.end)
                    .ok_or_else(|| internal("a subexpression ends outside its region"))?;
                for sub_block in &extbody[sub_start_i..sub_end_i] {
                    let has_other = traverse::block_contents(sub_block)
                        .iter()
                        .any(|item| !matches!(&*item.borrow(), Node::Assignment(_)));
                    if has_other {
                        return Ok((expressions, subs));
                    }
                }
            }

            subs.extend(expressions);
            expressions = subs;
            i = new_i;
            continue;
        } else if !subs_unused.is_empty() {
            subs_unused.extend(unused);
            unused = subs_unused;
        } else if i > current_i + 1 {
            // Some subexpressions may not have been checked yet.
            i = current_i + 1;
        }

        let Some(warp) = traverse::block_warp(block) else {
            break;
        };

        let conditional = {
            let borrowed = warp.borrow();
            if let Node::ConditionalWarp(inner) = &*borrowed {
                Some((
                    inner.condition,
                    inner
                        .false_target
                        .is_some_and(|target| traverse::same_node(target, end)),
                    inner.slot.map(i64::from),
                    inner.true_target,
                ))
            } else {
                if is_jump(&borrowed)
                    && traverse::same_node(block, start)
                    && traverse::block_contents(block).is_empty()
                {
                    return Ok((Vec::new(), expressions));
                }
                None
            }
        };

        if let Some((condition, is_end, block_slot, true_target)) = conditional {
            let is_binop = matches!(
                condition.map(|c| c.borrow().kind()).unwrap_or(""),
                "binary operator"
            );
            let block_slot_value = block_slot.unwrap_or(slot);

            if is_end {
                if is_binop {
                    return Ok((expressions, unused));
                }
                if slot < 0 && block_slot_value >= 0 {
                    slot = block_slot_value;
                    slot_type = Some(IdentifierKind::Slot);
                    slot_ref = Some(block);
                    if sure_expression.is_none() {
                        sure_expression = Some(true);
                    }
                } else if slot != block_slot_value {
                    sure_expression = Some(false);
                    continue;
                } else if sure_expression.is_none() {
                    sure_expression = Some(true);
                }
            } else if let Some(true_target) = true_target {
                // `x = y and z` leaves the condition in a register of its own
                // and tests it with a no-op branch.
                let contents = traverse::block_contents(true_target);
                let is_noop_case = contents.len() == 1
                    && matches!(&*contents[0].borrow(), Node::NoOp(_))
                    && matches!(&*true_target.borrow(), Node::Block(inner)
                        if inner.warp.is_some_and(|warp| matches!(&*warp.borrow(), Node::UnconditionalWarp(_))));
                if is_noop_case {
                    sure_expression = Some(true);
                    needs_validation = true;
                    slot_ref = Some(block);
                    i += 1;
                    continue;
                }
            }
        }

        // MARK: computed value

        let contents = traverse::block_contents(block);
        if contents.is_empty() {
            continue;
        }
        if !traverse::same_node(block, start) && contents.len() > 1 {
            return Ok((expressions, unused));
        }

        let assignment = *contents.last().expect("checked above");
        if !matches!(&*assignment.borrow(), Node::Assignment(_)) {
            if traverse::same_node(block, start) {
                continue;
            }
            if matches!(&*assignment.borrow(), Node::NoOp(_)) {
                let target =
                    traverse::block_warp(block).and_then(|warp| get_target(&warp.borrow(), true));
                if !target.is_some_and(|target| traverse::same_node(target, end)) {
                    continue;
                }
            }
            return Ok((expressions, unused));
        }

        let destinations = match &*assignment.borrow() {
            Node::Assignment(inner) => traverse::list_contents(inner.destinations),
            _ => unreachable!(),
        };
        if destinations.len() != 1 {
            if traverse::same_node(block, start) {
                continue;
            }
            return Ok((expressions, unused));
        }
        if warpins(block) == 0 && level > 0 {
            return Ok((expressions, unused));
        }

        let destination = destinations[0];
        let Some((kind, slot_number)) = identifier_of(destination) else {
            if traverse::same_node(block, start) {
                continue;
            }
            return Ok((expressions, unused));
        };

        if traverse::block_warp(block)
            .is_some_and(|warp| matches!(&*warp.borrow(), Node::ConditionalWarp(_)))
        {
            if traverse::same_node(block, start) {
                continue;
            }
            return Ok((expressions, unused));
        }
        if sure_expression == Some(false) {
            return Ok((expressions, unused));
        }

        if slot < 0 {
            // If every encounter is a local, the first one is a local too.
            if kind == IdentifierKind::Local {
                is_local = true;
            } else if kind == IdentifierKind::Upvalue {
                return Ok((Vec::new(), expressions));
            }

            slot_assignments.push(assignment);
            slot = slot_number;
            slot_type = Some(kind);
            slot_ref = Some(destination);
        } else if slot == slot_number {
            slot_assignments.push(assignment);
            slot_type = Some(kind);
            slot_ref = Some(destination);

            if kind == IdentifierKind::Upvalue {
                return Ok((Vec::new(), expressions));
            }
        } else {
            if traverse::same_node(block, start) {
                return Err(internal("the first block of an expression has no value"));
            }
            return Ok((Vec::new(), expressions));
        }
    }

    if slot < 0 {
        return Ok((Vec::new(), expressions));
    }

    let (true_terminator, _false, _body) = get_terminators(&body);

    if sure_expression.is_none() {
        if true_terminator.is_some() {
            sure_expression = Some(true);
        }

        if !expressions.is_empty() && known_blocks.contains(&node_key(block)) {
            let block_warp = traverse::block_warp(block);
            let matching_end_warp = expressions.iter().any(|expression| {
                match (traverse::block_warp(expression.end), block_warp) {
                    (Some(a), Some(b)) => traverse::same_node(a, b),
                    _ => false,
                }
            });
            if !matching_end_warp {
                // It may be better off as a plain `if`.
                needs_validation = true;
                sure_expression = Some(true);
            }
        }
    }

    if sure_expression != Some(true)
        && is_local
        && !local_can_be_expression(start, &slot_assignments, slot)
    {
        return Ok((expressions, unused));
    }

    // ljd also checks whether the end is an `EndWarp` here, but `end` is a
    // block, so that half of the condition never fires; only the position of
    // the block matters.
    let is_last = traverse::position(&extbody, block)
        .map(|index| index + 1 == extbody.len())
        .unwrap_or(true);
    if sure_expression != Some(true) && !known_blocks.contains(&node_key(block)) && !is_last {
        return Ok((expressions, unused));
    }

    expressions.push(Expression {
        block,
        start,
        end,
        slot,
        slot_type,
        slot_ref,
        needs_validation,
    });
    let _ = alloc;
    Ok((expressions, unused))
}

// MARK: expression slots

/// The kind and register of an identifier node.
fn identifier_of<'a>(node: NodeRef<'a>) -> Option<(IdentifierKind, i64)> {
    match &*node.borrow() {
        Node::Identifier(identifier) => Some((identifier.kind, i64::from(identifier.slot))),
        _ => None,
    }
}

/// Whether a local that is only assigned constants may still become an
/// expression.
fn local_can_be_expression<'a>(
    start: NodeRef<'a>,
    slot_assignments: &[NodeRef<'a>],
    slot: i64,
) -> bool {
    if slot_assignments.len() != 2 {
        return false;
    }
    let start_contents = traverse::block_contents(start);
    if block_index(start) != 0 && start_contents.is_empty() {
        return false;
    }

    let previous = slot_assignments[slot_assignments.len() - 2];
    let value = match &*previous.borrow() {
        Node::Assignment(inner) => traverse::list_contents(inner.expressions).first().copied(),
        _ => None,
    };
    let Some(value) = value else {
        return false;
    };

    let allowed = matches!(&*value.borrow(), Node::Constant(_))
        || matches!(&*value.borrow(), Node::Primitive(primitive)
            if primitive.kind == PrimitiveKind::True);
    if !allowed {
        return false;
    }

    // Assigning nil to the same register first means the value is read after it
    // was cleared, so the assignment cannot move into the expression.
    if let Some(last) = start_contents.last().copied()
        && let Node::Assignment(inner) = &*last.borrow()
        && traverse::list_contents(inner.expressions)
            .first()
            .is_some_and(|expression| {
                matches!(&*expression.borrow(), Node::Primitive(primitive)
                if primitive.kind == PrimitiveKind::Nil)
            })
    {
        for destination in traverse::list_contents(inner.destinations) {
            if let Node::Identifier(identifier) = &*destination.borrow()
                && i64::from(identifier.slot) == slot
            {
                return false;
            }
        }
    }

    true
}

/// Adds the blocks a warp can lead to to the known set.
fn add_warps_to_known_blocks<'a>(node: NodeRef<'a>, known: &mut HashSet<usize>) {
    let Some(warp) = traverse::block_warp(node) else {
        return;
    };
    for target in traverse::block_targets(&warp.borrow()) {
        known.insert(node_key(target));
    }
}

/// The register a two block expression writes into.
fn simple_local_assignment_slot<'a>(
    body: &[NodeRef<'a>],
) -> (i64, Option<IdentifierKind>, Option<NodeRef<'a>>) {
    if body.len() != 2 {
        return (-1, None, None);
    }

    let (Some(true_terminator), _false, _rest) = get_terminators(body) else {
        return (-1, None, None);
    };

    let contents = traverse::block_contents(true_terminator);
    let Some(assignment) = contents.first().copied() else {
        return (-1, None, None);
    };
    let destination = match &*assignment.borrow() {
        Node::Assignment(inner) => traverse::list_contents(inner.destinations).first().copied(),
        _ => None,
    };
    let Some(destination) = destination else {
        return (-1, None, None);
    };

    match &*destination.borrow() {
        Node::Identifier(identifier) => (
            i64::from(identifier.slot),
            Some(identifier.kind),
            Some(destination),
        ),
        Node::TableElement(element) => {
            let table = element.table;
            match &*table.borrow() {
                Node::Identifier(identifier) => (
                    i64::from(identifier.slot),
                    Some(identifier.kind),
                    Some(table),
                ),
                _ => (-1, None, None),
            }
        }
        _ => (-1, None, None),
    }
}

/// The two assignments that mark a region as a short circuit expression.
///
/// A region ends with `slot = true` followed by `slot = false`, which is how
/// the compiler writes the result of a logical expression.
fn get_terminators<'a>(
    body: &[NodeRef<'a>],
) -> (Option<NodeRef<'a>>, Option<NodeRef<'a>>, Vec<NodeRef<'a>>) {
    if body.len() < 2 {
        return (None, None, body.to_vec());
    }

    let last = body[body.len() - 1];
    let contents = traverse::block_contents(last);
    if contents.len() != 1 {
        return (None, None, body.to_vec());
    }
    let is_true = matches!(&*contents[0].borrow(), Node::Assignment(inner)
        if traverse::list_contents(inner.expressions)
            .first()
            .is_some_and(|value| matches!(&*value.borrow(), Node::Primitive(primitive)
                if primitive.kind == PrimitiveKind::True)));
    if !is_true {
        return (None, None, body.to_vec());
    }

    let previous = body[body.len() - 2];
    let contents = traverse::block_contents(previous);
    if contents.len() != 1 {
        return (None, None, body.to_vec());
    }

    let value = match &*contents[0].borrow() {
        Node::Assignment(inner) => traverse::list_contents(inner.expressions).first().copied(),
        _ => Some(contents[0]),
    };
    let is_false = value.is_some_and(|value| {
        matches!(&*value.borrow(), Node::Primitive(primitive)
        if primitive.kind == PrimitiveKind::False)
    });
    if !is_false {
        return (None, None, body.to_vec());
    }

    (Some(last), Some(previous), body[..body.len() - 2].to_vec())
}

// MARK: packing

/// Writes the expressions the scan found back into the graph.
///
/// The parts are processed in reverse, because replacing an expression changes
/// the graph the outer ones refer to.
fn unwarp_expressions_pack<'a>(
    alloc: &'a Allocator,
    blocks: &mut Vec<NodeRef<'a>>,
    pack: &[Expression<'a>],
    recovery: Recovery,
) -> Result<()> {
    let mut replacements: Vec<(NodeRef<'a>, NodeRef<'a>)> = Vec::new();
    let lookup = |replacements: &[(NodeRef<'a>, NodeRef<'a>)], node: NodeRef<'a>| -> NodeRef<'a> {
        for (from, to) in replacements {
            if traverse::same_node(from, node) {
                return *to;
            }
        }
        node
    };

    for (index, expression) in pack.iter().rev().enumerate() {
        let end = lookup(&replacements, expression.end);
        let start = expression.start;
        let block = lookup(&replacements, expression.block);

        let Some(start_index) = traverse::position(blocks, start) else {
            continue;
        };
        let Some(end_index) = traverse::position(blocks, end) else {
            continue;
        };

        let mut is_special = false;
        let mut skip_expression = false;
        let before = blocks[..start_index].to_vec();
        let body = blocks[start_index + 1..end_index].to_vec();

        if traverse::same_node(block, start)
            && traverse::block_warp(block).is_some_and(|warp| is_jump(&warp.borrow()))
        {
            skip_expression = true;
        }

        if expression.needs_validation {
            // A long chain of assignments to the same register is an `if`.
            let mut num_assignments = 0;
            for b in &body {
                for statement in traverse::block_contents(b) {
                    let borrowed = statement.borrow();
                    let Node::Assignment(inner) = &*borrowed else {
                        continue;
                    };
                    let destinations = traverse::list_contents(inner.destinations);
                    let expressions = traverse::list_contents(inner.expressions);
                    if destinations.len() != 1 || expressions.len() != 1 {
                        continue;
                    }
                    if expression
                        .slot_ref
                        .is_some_and(|slot| is_equal(destinations[0], slot, true))
                    {
                        num_assignments += 1;
                    }
                }
            }
            skip_expression = num_assignments > 2;
        }

        if expression.needs_validation
            && !skip_expression
            && traverse::block_warp(start)
                .is_some_and(|warp| matches!(&*warp.borrow(), Node::ConditionalWarp(_)))
        {
            // The special case has no operation in its true branch; be
            // conservative and skip expressions nested inside another one.
            let mut candidates = vec![start];
            candidates.extend(body.iter().copied());
            for b in candidates {
                let Some(warp) = traverse::block_warp(b) else {
                    continue;
                };
                let true_target = match &*warp.borrow() {
                    Node::ConditionalWarp(inner) => inner.true_target,
                    _ => None,
                };
                let Some(true_target) = true_target else {
                    continue;
                };
                let contents = traverse::block_contents(true_target);
                let is_special_shape = contents.len() == 1
                    && matches!(&*contents[0].borrow(), Node::NoOp(_))
                    && matches!(&*true_target.borrow(), Node::Block(inner)
                        if inner.warp.is_some_and(|warp| matches!(&*warp.borrow(), Node::UnconditionalWarp(_))));
                if !is_special_shape {
                    continue;
                }

                is_special = true;
                let other_start = !traverse::same_node(b, start);
                let nested = index > 0
                    && find_warps_to(&before, b).iter().any(|warp| {
                        let previous = traverse::block_warp(blocks[index - 1]);
                        !previous.is_some_and(|previous| traverse::same_node(warp, previous))
                    });
                if other_start || nested {
                    skip_expression = true;
                    break;
                }
            }
        }

        if !skip_expression {
            // Anything else jumping into the body is taken care of later.
            skip_expression = body.iter().any(|b| !find_warps_to(&before, b).is_empty());
        }
        if skip_expression {
            continue;
        }

        if let Err(error) = unwarp_logical_expression(alloc, start, end, &body) {
            // The subexpression is left as it was found, so what it holds is
            // written as separate statements.
            if recovery == Recovery::Off {
                return Err(error);
            }
            mark_error(start);
        }

        if is_special {
            let contents = traverse::block_contents(start);
            let Some(last) = contents.last().copied() else {
                continue;
            };
            let keep = special_case_is_equivalent(last, expression);
            if !keep {
                let mut contents = contents;
                contents.pop();
                traverse::set_block_contents(alloc, start, contents);
                continue;
            }
        }

        // The subexpression is gone; the flow now goes straight to its end.
        set_flow_to(alloc, start, end);

        if end_index - start_index > 2 {
            // There may still be registers to eliminate before the body goes
            // away, which is why a temporary block stands in for it.
            let first_address = match &*blocks[start_index + 1].borrow() {
                Node::Block(inner) => inner.first_address,
                _ => 0,
            };
            let last_address = match &*blocks[end_index - 1].borrow() {
                Node::Block(inner) => inner.last_address,
                _ => 0,
            };
            let temporary = node(
                alloc,
                Node::Block(ArenaBox::new_in(
                    Block {
                        index: block_index(blocks[start_index + 1]),
                        first_address,
                        last_address,
                        last_body_address: 0,
                        warpins_count: warpins(blocks[start_index + 1]),
                        is_loop: false,
                        contents: ArenaVec::from_iter_in([], &alloc),
                        warp: traverse::block_warp(blocks[end_index - 1]),
                    },
                    &alloc,
                )),
            );

            let mut contents = Vec::new();
            for b in &blocks[start_index + 1..end_index] {
                contents.extend(traverse::block_contents(b));
            }
            traverse::set_block_contents(alloc, temporary, contents);

            slotworks::eliminate_temporary(
                alloc,
                temporary,
                slotworks::Options {
                    ignore_ambiguous: false,
                    ..Default::default()
                },
            )?;
        }

        blocks.drain(start_index + 1..end_index);

        let end_warps = find_warps_to(blocks, end);
        if !traverse::contains(&end_warps, start) {
            return Err(internal("an expression does not end where it claims to"));
        }

        if start_index > 0 {
            let preceding = blocks[start_index - 1];
            let jumps_into_body = traverse::block_warp(preceding)
                .filter(|warp| matches!(&*warp.borrow(), Node::UnconditionalWarp(_)))
                .and_then(|warp| get_target(&warp.borrow(), false))
                .and_then(|target| traverse::position(blocks, target))
                .is_some_and(|index| index > start_index && index + 1 < end_index);
            if jumps_into_body {
                continue;
            }
        }

        if end_warps.len() == 1 {
            // Nothing but the start reaches the end, so the two can be merged.
            let mut contents = traverse::block_contents(start);
            contents.extend(traverse::block_contents(end));
            traverse::set_block_contents(alloc, end, contents);
            traverse::set_block_contents(alloc, start, Vec::new());

            blocks.remove(start_index);
            replace_targets(alloc, blocks, start, end, false);
            replacements.push((start, end));

            slotworks::eliminate_temporary(
                alloc,
                end,
                slotworks::Options {
                    ignore_ambiguous: false,
                    ..Default::default()
                },
            )?;
            slotworks::simplify_ast(alloc, end, &mut |node| {
                let _ = slotworks::eliminate_temporary(alloc, node, slotworks::Options::default());
            });
        } else {
            slotworks::eliminate_temporary(
                alloc,
                start,
                slotworks::Options {
                    ignore_ambiguous: false,
                    ..Default::default()
                },
            )?;
            slotworks::simplify_ast(alloc, end, &mut |node| {
                let _ = slotworks::eliminate_temporary(alloc, node, slotworks::Options::default());
            });
        }
    }

    let _ = lookup;
    Ok(())
}

/// Whether the statement a special case produced really is the expression.
fn special_case_is_equivalent<'a>(statement: NodeRef<'a>, _expression: &Expression<'a>) -> bool {
    let borrowed = statement.borrow();
    let Node::Assignment(inner) = &*borrowed else {
        return false;
    };

    let destinations = traverse::list_contents(inner.destinations);
    let expressions = traverse::list_contents(inner.expressions);
    if destinations.len() != 1 || expressions.len() != 1 {
        return false;
    }

    let destination = destinations[0];
    let value = expressions[0];
    let value = match &*value.borrow() {
        Node::BinaryOperator(inner) => inner.right,
        _ => value,
    };

    is_equal(value, destination, true)
        || matches!(&*value.borrow(), Node::Primitive(primitive)
            if primitive.kind != PrimitiveKind::False)
}

// MARK: logical expression matcher

/// A piece of an expression being assembled: a value, the operator that joins
/// it to the next piece, or a group of pieces that has to be assembled first.
#[derive(Clone)]
enum Part<'a> {
    Node(NodeRef<'a>),
    Operator(BinaryOperatorKind),
    Group(Vec<Part<'a>>),
}

/// How tightly the operators of a logical expression bind.
///
/// `or` binds looser than `and`, which is what makes the pieces of an
/// expression group the way they do.
fn operator_rank(kind: BinaryOperatorKind) -> i32 {
    match kind {
        BinaryOperatorKind::LogicalOr => 0,
        BinaryOperatorKind::LogicalAnd => 10,
        _ => 100,
    }
}

/// Builds the expression that assigns its value to the register the region
/// writes into.
fn unwarp_logical_expression<'a>(
    alloc: &'a Allocator,
    start: NodeRef<'a>,
    end: NodeRef<'a>,
    body: &[NodeRef<'a>],
) -> Result<()> {
    let slot = find_expression_slot(body)
        .ok_or_else(|| internal("an expression without a value to assign"))?;

    let (true_terminator, false_terminator, body) = get_terminators(body);

    let mut parts = vec![start];
    parts.extend(body);
    let expression =
        compile_expression(alloc, &parts, Some(end), true_terminator, false_terminator)?;

    let destination = traverse::deep_clone(alloc, slot);
    let assignment = node(
        alloc,
        Node::Assignment(ArenaBox::new_in(
            Assignment {
                expressions: expressions(alloc, vec![expression]),
                destinations: variables(alloc, vec![destination]),
                kind: AssignmentKind::Normal,
                meta: Meta::default(),
            },
            &alloc,
        )),
    );

    let mut contents = traverse::block_contents(start);
    contents.push(assignment);
    traverse::set_block_contents(alloc, start, contents);
    Ok(())
}

/// The register the expression's value is written into.
fn find_expression_slot<'a>(body: &[NodeRef<'a>]) -> Option<NodeRef<'a>> {
    for block in body.iter().rev() {
        let contents = traverse::block_contents(block);
        let Some(last) = contents.last().copied() else {
            continue;
        };
        return match &*last.borrow() {
            Node::Assignment(inner) => traverse::list_contents(inner.destinations).first().copied(),
            _ => None,
        };
    }
    None
}

/// Turns a region into an expression.
///
/// `true_end` and `false_end` are the terminators the expression can leave
/// through; `end` is the block the whole region ends at, when it is known.
pub(super) fn compile_expression<'a>(
    alloc: &'a Allocator,
    body: &[NodeRef<'a>],
    end: Option<NodeRef<'a>>,
    true_end: Option<NodeRef<'a>>,
    false_end: Option<NodeRef<'a>>,
) -> Result<NodeRef<'a>> {
    let parts = unwarp_expression(alloc, body, end, true_end, false_end)?;

    if parts.len() < 3 {
        if parts.len() != 1 {
            return Err(internal("a logical expression without a value"));
        }
        return match &parts[0] {
            Part::Node(node) => Ok(*node),
            _ => Err(internal("a logical expression without a value")),
        };
    }

    let explicit = make_explicit_subexpressions(&parts);
    let expression = assemble_expression(alloc, &explicit)?;
    Ok(optimise_expression(alloc, expression))
}

// MARK: folding expressions

/// Rearranges an expression so the writer does not need brackets.
///
/// `1 + (2 + 3)` and `(1 + 2) + 3` mean the same thing for a commutative
/// operator, and the second one is written without brackets.
fn optimise_expression<'a>(alloc: &'a Allocator, expression: NodeRef<'a>) -> NodeRef<'a> {
    optimise_expression_skipping(alloc, expression, None)
}

fn optimise_expression_skipping<'a>(
    alloc: &'a Allocator,
    expression: NodeRef<'a>,
    skip: Option<BinaryOperatorKind>,
) -> NodeRef<'a> {
    let (kind, left, right) = match &*expression.borrow() {
        Node::BinaryOperator(inner) => (inner.kind, inner.left, inner.right),
        _ => return expression,
    };

    let left = optimise_expression_skipping(alloc, left, Some(kind));
    let right = optimise_expression_skipping(alloc, right, Some(kind));
    set_operator_operands(expression, left, right);

    // A node that is already being reorganised keeps its shape.
    if skip == Some(kind) {
        return expression;
    }
    if !kind.is_commutative() || kind.is_right_associative() {
        return expression;
    }
    // `==` and `~=` are commutative but swapping them would change which value
    // is returned.
    if matches!(
        kind,
        BinaryOperatorKind::Equal | BinaryOperatorKind::NotEqual
    ) {
        return expression;
    }

    let mut children = find_binary_operator_children(expression, kind);
    let mut result = children.remove(0);
    for child in children {
        let next = node(
            alloc,
            Node::BinaryOperator(ArenaBox::new_in(
                BinaryOperator {
                    kind,
                    left: result,
                    right: child,
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );
        result = next;
    }
    result
}

/// The operands of a chain of the same operator.
fn find_binary_operator_children<'a>(
    expression: NodeRef<'a>,
    kind: BinaryOperatorKind,
) -> Vec<NodeRef<'a>> {
    let (this_kind, left, right) = match &*expression.borrow() {
        Node::BinaryOperator(inner) => (inner.kind, inner.left, inner.right),
        _ => return vec![expression],
    };

    if this_kind != kind {
        return vec![expression];
    }

    let mut children = find_binary_operator_children(left, kind);
    children.extend(find_binary_operator_children(right, kind));
    children
}

fn set_operator_operands<'a>(expression: NodeRef<'a>, left: NodeRef<'a>, right: NodeRef<'a>) {
    if let Node::BinaryOperator(inner) = &mut *expression.borrow_mut() {
        inner.left = left;
        inner.right = right;
    }
}

// MARK: unwarping expressions

/// The greedy matcher that turns a region into a flat list of values and
/// operators.
fn unwarp_expression<'a>(
    alloc: &'a Allocator,
    body: &[NodeRef<'a>],
    end: Option<NodeRef<'a>>,
    true_end: Option<NodeRef<'a>>,
    false_end: Option<NodeRef<'a>>,
) -> Result<Vec<Part<'a>>> {
    let mut parts: Vec<Part<'a>> = Vec::new();

    let terminator_index = match true_end {
        Some(true_end) => {
            let false_end = false_end.ok_or_else(|| internal("a terminator without its pair"))?;
            let mut index = block_index(true_end).min(block_index(false_end));
            if let Some(end) = end {
                index = index.min(block_index(end));
            }
            index
        }
        None => block_index(end.ok_or_else(|| internal("an expression without a terminator"))?),
    };

    let mut subexpression_start = 0usize;
    let mut i = 0usize;

    while i + 1 < body.len() {
        let block = body[i];
        let warp = traverse::block_warp(block)
            .ok_or_else(|| internal("a block without a warp in an expression"))?;
        let target = get_target(&warp.borrow(), false)
            .ok_or_else(|| internal("a warp without a target in an expression"))?;

        let subexpression: Vec<NodeRef<'a>> = if block_index(target) < terminator_index {
            // A subexpression that starts before the end of this one, as in
            // `(foo and (bar and y or z)) or x`.
            if i != subexpression_start {
                i += 1;
                continue;
            }

            let target_index = traverse::position(body, target)
                .ok_or_else(|| internal("a subexpression target is not in the body"))?;
            if target_index == 0 {
                i += 1;
                continue;
            }
            let last_block = body[target_index - 1];
            let last_block_warp = traverse::block_warp(last_block)
                .ok_or_else(|| internal("a block without a warp in an expression"))?;
            let last_block_target = get_target(&last_block_warp.borrow(), false)
                .ok_or_else(|| internal("a warp without a target in an expression"))?;

            if block_index(last_block_target) < terminator_index {
                i += 1;
                continue;
            }

            body[i..target_index].to_vec()
        } else {
            // Take every following block that leaves through the same
            // terminator with the same inversion.
            let mut warp = warp;
            while i + 2 < body.len() {
                let next_block = body[i + 1];
                let next_warp = match traverse::block_warp(next_block) {
                    Some(next_warp) => next_warp,
                    None => break,
                };
                let next_target = get_target(&next_warp.borrow(), false);
                if !same_optional(next_target, Some(target)) {
                    break;
                }

                let next_inverted = is_inverted(&next_warp.borrow(), true_end, end);
                let this_inverted = if contains_primitive_condition(block) {
                    !is_inverted(&warp.borrow(), true_end, end)
                } else {
                    is_inverted(&warp.borrow(), true_end, end)
                };
                if next_inverted != this_inverted {
                    break;
                }

                warp = next_warp;
                i += 1;
            }

            body[subexpression_start..=i].to_vec()
        };

        let last_block = *subexpression
            .last()
            .ok_or_else(|| internal("an empty subexpression"))?;
        let last_block_index = traverse::position(body, last_block)
            .ok_or_else(|| internal("a subexpression ends outside its body"))?;
        let next_block = *body
            .get(last_block_index + 1)
            .ok_or_else(|| internal("a subexpression has no following block"))?;

        let operator = get_operator(
            alloc,
            &subexpression,
            subexpression.len() - 1,
            true_end,
            end,
        )?;
        let new_subexpression = compile_subexpression(
            alloc,
            &subexpression,
            operator,
            last_block,
            next_block,
            true_end,
            end,
        )?;

        // A no-op has no value of its own, so it contributes nothing.
        if !matches!(&*new_subexpression.borrow(), Node::NoOp(_)) {
            parts.push(Part::Node(new_subexpression));
            parts.push(Part::Operator(operator));
        }

        i = last_block_index + 1;
        subexpression_start = i;
    }

    let last = *body
        .last()
        .ok_or_else(|| internal("an empty expression body"))?;

    let last_warp = traverse::block_warp(last);
    let is_conditional =
        last_warp.is_some_and(|warp| matches!(&*warp.borrow(), Node::ConditionalWarp(_)));

    if is_conditional {
        let warp = last_warp.expect("checked above");
        let (condition, inverted) = {
            let borrowed = warp.borrow();
            let Node::ConditionalWarp(inner) = &*borrowed else {
                unreachable!()
            };
            (inner.condition, is_inverted(&borrowed, true_end, end))
        };
        let condition = condition.ok_or_else(|| internal("a branch without a condition"))?;
        let value = if inverted {
            invert(alloc, condition)?
        } else {
            condition
        };
        parts.push(Part::Node(value));
    } else {
        let source = last_assignment_source(last);
        let source = match source {
            Some(source) => source,
            None => {
                // `A = B and A` leaves a no-op branch behind; the destination of
                // the branch that cannot be reached is the value.
                let mut special = None;
                let contents = traverse::block_contents(last);
                if contents.len() == 1 && matches!(&*contents[0].borrow(), Node::NoOp(_)) {
                    let true_end = true_end
                        .ok_or_else(|| internal("an expression without a true terminator"))?;
                    let false_end = false_end
                        .ok_or_else(|| internal("an expression without a false terminator"))?;

                    if warpins(false_end) == 0 && traverse::block_contents(true_end).len() == 1 {
                        special = Some(false_end);
                    } else if warpins(true_end) == 0
                        && traverse::block_contents(false_end).len() == 1
                    {
                        special = Some(true_end);
                    }
                }

                match special.filter(|block| !traverse::block_contents(block).is_empty()) {
                    Some(block) => {
                        let contents = traverse::block_contents(block);
                        match &*contents[contents.len() - 1].borrow() {
                            Node::Assignment(inner) => traverse::list_contents(inner.destinations)
                                .first()
                                .copied()
                                .ok_or_else(|| internal("an assignment without a value"))?,
                            _ => return Err(internal("a branch without a value")),
                        }
                    }
                    None => {
                        // The value is whichever constant the branch tests for.
                        let warp = traverse::block_warp(last);
                        let goes_to_true = warp
                            .and_then(|warp| get_target(&warp.borrow(), false))
                            .is_some_and(|target| same_optional(Some(target), true_end));
                        primitive(
                            alloc,
                            if goes_to_true {
                                PrimitiveKind::True
                            } else {
                                PrimitiveKind::False
                            },
                        )
                    }
                }
            }
        };
        parts.push(Part::Node(source));
    }

    Ok(parts)
}

/// Whether two optional nodes are the same node.
fn same_optional_unused() {}

// MARK: operators

/// The operator that joins a subexpression to the one after it.
fn get_operator<'a>(
    alloc: &'a Allocator,
    blocks: &[NodeRef<'a>],
    index: usize,
    true_end: Option<NodeRef<'a>>,
    end: Option<NodeRef<'a>>,
) -> Result<BinaryOperatorKind> {
    let block = blocks[index];
    let warp = traverse::block_warp(block)
        .ok_or_else(|| internal("a block without a warp in an expression"))?;

    let is_flow = {
        let borrowed = warp.borrow();
        matches!(&*borrowed, Node::UnconditionalWarp(_))
    };

    if is_flow {
        let source = last_assignment_source(block);

        let is_true = match source {
            Some(source) => is_unconditional_operator_true(alloc, source),
            None => {
                // A chain of constant branches: the operator is the one the
                // constants before this block imply.
                let mut operator_source = None;
                let mut operator_index = index;
                while operator_index > 0 {
                    operator_index -= 1;
                    let preceding = blocks[operator_index];
                    let Some(preceding_warp) = traverse::block_warp(preceding) else {
                        break;
                    };
                    if !matches!(&*preceding_warp.borrow(), Node::UnconditionalWarp(_)) {
                        break;
                    }

                    match last_assignment_source(preceding) {
                        Some(preceding_source)
                            if matches!(
                                &*preceding_source.borrow(),
                                Node::Primitive(_) | Node::BinaryOperator(_)
                            ) =>
                        {
                            operator_source = Some(preceding_source);
                        }
                        _ => break,
                    }
                }

                match operator_source {
                    Some(source) => is_unconditional_operator_true(alloc, source),
                    None => {
                        let target = get_target(&warp.borrow(), false);
                        target.is_some_and(|target| same_optional(Some(target), true_end))
                    }
                }
            }
        };

        Ok(if is_true {
            BinaryOperatorKind::LogicalOr
        } else {
            BinaryOperatorKind::LogicalAnd
        })
    } else {
        Ok(if is_inverted(&warp.borrow(), true_end, end) {
            BinaryOperatorKind::LogicalOr
        } else {
            BinaryOperatorKind::LogicalAnd
        })
    }
}

/// Whether the value a block produces is true.
///
/// A constant or a computed value counts as true; only a constant `false`, or
/// a logical operator that can produce one, does not.
fn is_unconditional_operator_true<'a>(alloc: &'a Allocator, source: NodeRef<'a>) -> bool {
    match &*source.borrow() {
        Node::Constant(_) | Node::UnaryOperator(_) => true,
        Node::BinaryOperator(inner) => {
            let (kind, left, right) = (inner.kind, inner.left, inner.right);
            let left = simplified_operand(alloc, left);
            let right = simplified_operand(alloc, right);

            match (
                &*left.borrow(),
                &*right.borrow(),
                matches!(
                    kind,
                    BinaryOperatorKind::LogicalOr | BinaryOperatorKind::LogicalAnd
                ),
            ) {
                (Node::Primitive(left), Node::Primitive(right), true) => {
                    if left.kind == PrimitiveKind::False {
                        if right.kind == PrimitiveKind::False {
                            false
                        } else {
                            kind == BinaryOperatorKind::LogicalOr
                        }
                    } else {
                        right.kind == PrimitiveKind::False && kind == BinaryOperatorKind::LogicalOr
                    }
                }
                _ => true,
            }
        }
        Node::Primitive(primitive) => primitive.kind == PrimitiveKind::True,
        Node::Identifier(_)
        | Node::TableElement(_)
        | Node::FunctionCall(_)
        | Node::FunctionDefinition(_)
        | Node::NoOp(_) => true,
        _ => true,
    }
}

/// Replaces a nested logical operator by the constant it evaluates to.
fn simplified_operand<'a>(alloc: &'a Allocator, operand: NodeRef<'a>) -> NodeRef<'a> {
    if matches!(&*operand.borrow(), Node::BinaryOperator(_)) {
        let kind = if is_unconditional_operator_true(alloc, operand) {
            PrimitiveKind::True
        } else {
            PrimitiveKind::False
        };
        return primitive(alloc, kind);
    }
    operand
}

/// The value the last statement of a block computes, if it has one.
fn last_assignment_source<'a>(block: NodeRef<'a>) -> Option<NodeRef<'a>> {
    let contents = traverse::block_contents(block);
    let last = contents.last().copied()?;

    match &*last.borrow() {
        Node::Assignment(inner) => traverse::list_contents(inner.expressions).first().copied(),
        Node::Return(inner) => traverse::list_contents(inner.returns).first().copied(),
        Node::FunctionCall(_) | Node::NoOp(_) => None,
        _ => None,
    }
}

/// Takes the value out of a block that only holds it to pass it on.
fn take_last_assignment_source<'a>(
    alloc: &'a Allocator,
    block: NodeRef<'a>,
) -> Option<NodeRef<'a>> {
    let mut contents = traverse::block_contents(block);
    let last = contents.pop()?;
    traverse::set_block_contents(alloc, block, contents);

    match &*last.borrow() {
        Node::Assignment(inner) => traverse::list_contents(inner.expressions).first().copied(),
        _ => Some(last),
    }
}

// MARK: result assembly

/// Builds the expression of one subexpression.
fn compile_subexpression<'a>(
    alloc: &'a Allocator,
    subexpression: &[NodeRef<'a>],
    operator: BinaryOperatorKind,
    block: NodeRef<'a>,
    next_block: NodeRef<'a>,
    true_end: Option<NodeRef<'a>>,
    end: Option<NodeRef<'a>>,
) -> Result<NodeRef<'a>> {
    let warp = traverse::block_warp(block)
        .ok_or_else(|| internal("a block without a warp in an expression"))?;

    if subexpression.len() == 1 {
        let is_flow = matches!(&*warp.borrow(), Node::UnconditionalWarp(_));
        if is_flow {
            return take_last_assignment_source(alloc, block)
                .ok_or_else(|| internal("a block without a value in an expression"));
        }

        let (condition, inverted) = {
            let borrowed = warp.borrow();
            let Node::ConditionalWarp(inner) = &*borrowed else {
                return Err(internal("a block without a condition"));
            };
            (inner.condition, is_inverted(&borrowed, true_end, end))
        };
        let condition = condition.ok_or_else(|| internal("a branch without a condition"))?;
        return if inverted {
            invert(alloc, condition)
        } else {
            Ok(condition)
        };
    }

    let is_flow = matches!(&*warp.borrow(), Node::UnconditionalWarp(_));
    let (sub_true, sub_false) = if is_flow {
        let target = get_target(&warp.borrow(), false)
            .ok_or_else(|| internal("a warp without a target in an expression"))?;
        if operator == BinaryOperatorKind::LogicalOr {
            (target, next_block)
        } else {
            (next_block, target)
        }
    } else {
        let (true_target, false_target) = {
            let borrowed = warp.borrow();
            let Node::ConditionalWarp(inner) = &*borrowed else {
                return Err(internal("a block without a condition"));
            };
            (inner.true_target, inner.false_target)
        };
        let true_target = true_target.ok_or_else(|| internal("a branch without a target"))?;
        let false_target = false_target.ok_or_else(|| internal("a branch without a target"))?;

        if operator == BinaryOperatorKind::LogicalOr {
            (false_target, true_target)
        } else {
            (true_target, false_target)
        }
    };

    compile_expression(alloc, subexpression, None, Some(sub_true), Some(sub_false))
}

/// Whether a branch tests the opposite of what its condition says.
fn is_inverted<'a>(
    warp: &Node<'a>,
    true_end: Option<NodeRef<'a>>,
    end: Option<NodeRef<'a>>,
) -> bool {
    match warp {
        Node::UnconditionalWarp(inner) => match (inner.target, end) {
            (Some(target), Some(end)) => traverse::same_node(target, end),
            _ => false,
        },
        Node::ConditionalWarp(inner) => {
            if inner
                .false_target
                .zip(true_end)
                .is_some_and(|(a, b)| traverse::same_node(a, b))
            {
                return true;
            }

            let goes_to_end = match (inner.false_target, end) {
                (Some(target), Some(end)) => traverse::same_node(target, end),
                _ => false,
            };
            if !goes_to_end {
                return false;
            }

            // A branch that leaves at the end is inverted when its condition is
            // a negation.
            match &inner.condition {
                Some(condition) => match &*condition.borrow() {
                    Node::BinaryOperator(_) => false,
                    Node::UnaryOperator(unary) => unary.kind == UnaryOperatorKind::Not,
                    _ => false,
                },
                None => false,
            }
        }
        _ => false,
    }
}

/// The opposite of a condition.
pub(super) fn invert<'a>(alloc: &'a Allocator, expression: NodeRef<'a>) -> Result<NodeRef<'a>> {
    match &*expression.borrow() {
        Node::UnaryOperator(inner) if inner.kind == UnaryOperatorKind::Not => Ok(inner.operand),
        Node::BinaryOperator(inner) => {
            let kind = inner.kind;
            let negated = match kind {
                BinaryOperatorKind::LessThan => Some(BinaryOperatorKind::GreaterOrEqual),
                BinaryOperatorKind::GreaterThan => Some(BinaryOperatorKind::LessOrEqual),
                BinaryOperatorKind::LessOrEqual => Some(BinaryOperatorKind::GreaterThan),
                BinaryOperatorKind::GreaterOrEqual => Some(BinaryOperatorKind::LessThan),
                BinaryOperatorKind::NotEqual => Some(BinaryOperatorKind::Equal),
                BinaryOperatorKind::Equal => Some(BinaryOperatorKind::NotEqual),
                BinaryOperatorKind::LogicalOr => Some(BinaryOperatorKind::LogicalAnd),
                BinaryOperatorKind::LogicalAnd => Some(BinaryOperatorKind::LogicalOr),
                _ => None,
            };

            let Some(negated) = negated else {
                return Err(Error::Unsupported(format!(
                    "cannot invert a `{}` expression",
                    kind.as_str()
                )));
            };

            // The original has to stay where it is, so the negation is built
            // from copies.
            let copy = traverse::deep_clone(alloc, expression);
            if let Node::BinaryOperator(inner) = &mut *copy.borrow_mut() {
                inner.kind = negated;
                if matches!(
                    negated,
                    BinaryOperatorKind::LogicalOr | BinaryOperatorKind::LogicalAnd
                ) {
                    let (left, right) = (inner.left, inner.right);
                    inner.left = invert(alloc, left)?;
                    inner.right = invert(alloc, right)?;
                }
            }
            Ok(copy)
        }
        _ => Ok(node(
            alloc,
            Node::UnaryOperator(ArenaBox::new_in(
                UnaryOperator {
                    kind: UnaryOperatorKind::Not,
                    operand: expression,
                    meta: Meta::default(),
                },
                &alloc,
            )),
        )),
    }
}

/// Joins the parts of an expression into a tree.
fn assemble_expression<'a>(alloc: &'a Allocator, parts: &[Part<'a>]) -> Result<NodeRef<'a>> {
    if parts.len() == 1 {
        return assemble_part(alloc, parts[0].clone());
    }
    if parts.len() < 3 {
        return Err(internal("a logical expression with a missing operand"));
    }

    let mut result = node(
        alloc,
        Node::BinaryOperator(ArenaBox::new_in(
            BinaryOperator {
                kind: operator_of(&parts[parts.len() - 2])?,
                left: assemble_part(alloc, parts[parts.len() - 3].clone())?,
                right: assemble_part(alloc, parts[parts.len() - 1].clone())?,
                meta: Meta::default(),
            },
            &alloc,
        )),
    );

    let mut i = parts.len() as i64 - 4;
    while i > 0 {
        let operator = operator_of(&parts[i as usize])?;
        let component = assemble_part(alloc, parts[i as usize - 1].clone())?;

        result = node(
            alloc,
            Node::BinaryOperator(ArenaBox::new_in(
                BinaryOperator {
                    kind: operator,
                    left: component,
                    right: result,
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );

        i -= 2;
    }

    Ok(result)
}

fn assemble_part<'a>(alloc: &'a Allocator, part: Part<'a>) -> Result<NodeRef<'a>> {
    match part {
        Part::Node(node) => Ok(node),
        Part::Group(items) => assemble_expression(alloc, &items),
        Part::Operator(_) => Err(internal("an operator where a value was expected")),
    }
}

fn operator_of(part: &Part<'_>) -> Result<BinaryOperatorKind> {
    match part {
        Part::Operator(kind) => Ok(*kind),
        _ => Err(internal("a value where an operator was expected")),
    }
}

/// Splits the topmost expression at every change of operator.
///
/// The assembly phase needs to know where a subexpression starts and ends,
/// which is what the grouping makes explicit.
fn make_explicit_subexpressions<'a>(parts: &[Part<'a>]) -> Vec<Part<'a>> {
    let mut patched: Vec<Part<'a>> = Vec::new();
    let mut i = 0usize;

    let mut last_operator = operator_of(&parts[1]).unwrap_or(BinaryOperatorKind::LogicalOr);
    let mut subexpression_start: i64 = -1;

    while i + 1 < parts.len() {
        let component = parts[i].clone();
        let operator = match operator_of(&parts[i + 1]) {
            Ok(operator) => operator,
            Err(_) => break,
        };

        if operator_rank(operator) < operator_rank(last_operator) {
            subexpression_start = i as i64;
            last_operator = operator;
        } else if subexpression_start > 0 {
            if operator_rank(operator) > operator_rank(last_operator)
                && (i as i64 - subexpression_start) % 2 != 0
            {
                patched.push(Part::Group(parts[subexpression_start as usize..i].to_vec()));
                subexpression_start = -1;
            }
        } else {
            patched.push(component);
            patched.push(Part::Operator(operator));
        }

        i += 2;
    }

    if subexpression_start >= 0 {
        patched.push(Part::Group(parts[subexpression_start as usize..].to_vec()));
    } else if let Some(last) = parts.last() {
        patched.push(last.clone());
    }

    patched
}
