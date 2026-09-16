//! Cleanups that have to happen before the control flow can be rebuilt.
//!
//! This is a port of the `pre_pass` half of ljd's `ast/mutator.py`. The graph
//! the builder produces does not look quite like the source did:
//!
//! * A loop is opened by a jump at its top and counted down at its bottom. ljd
//!   expects the loop marker on the block the loop starts at, and a jump back
//!   from the block that closes it, so the two warps are exchanged.
//! * A `return` that only closes an upvalue is how LuaJIT compiles leaving a
//!   loop; it is turned back into the `break` or the inlined statement it was.
//! * A condition that was materialised into a register and then tested against
//!   itself gets a block of its own, so that the unwarper sees an ordinary
//!   `if`-shaped branch.
//! * A branch whose two targets turned out to be the same block is unreachable
//!   and becomes a branch on the constant `false`.

use std::rc::Rc;

use crate::bytecode::SLOT_FALSE;

use super::helpers::{has_same_table, insert_table_record, is_equal};
use super::nodes::*;
use super::traverse::{self, Visitor};

/// Rewrites the graph into the shape the later passes expect.
pub fn pre_pass(root: &NodeRef) {
    traverse::traverse(&mut SimpleLoopWarpSwapper::new(), root);
}

/// Cleans up the tree once the control flow is structured.
///
/// This is a port of ljd's `primary_pass`: `if` statements nested in an `else`
/// become `elseif` chains, and assignments that only fill in fields of a table
/// are folded back into its constructor.
pub fn primary_pass(root: &NodeRef) {
    traverse::traverse(&mut MutatorVisitor, root);
}

struct MutatorVisitor;

impl Visitor for MutatorVisitor {
    fn visit(&mut self, node: &NodeRef) -> bool {
        if matches!(&*node.borrow(), Node::Statements(_)) {
            fill_constructors(node);
        }
        true
    }

    fn leave(&mut self, node: &NodeRef) {
        if matches!(&*node.borrow(), Node::If(_)) {
            merge_elseif(node);
        }
    }
}

/// Turns an `if` that is the only statement of an `else` into an `elseif`.
fn merge_elseif(if_node: &NodeRef) {
    let else_contents = match &*if_node.borrow() {
        Node::If(inner) => traverse::list_contents(&inner.else_block),
        _ => return,
    };
    if else_contents.len() != 1 {
        return;
    }

    let Node::If(sub) = &*else_contents[0].borrow() else {
        return;
    };
    let (expression, then_block, sub_elseifs, sub_else_block) = (
        sub.expression.clone(),
        sub.then_block.clone(),
        sub.elseifs.clone(),
        sub.else_block.clone(),
    );

    let entry = node(Node::ElseIf(Box::new(ElseIf {
        expression,
        then_block,
        meta: Meta::default(),
    })));

    if let Node::If(inner) = &mut *if_node.borrow_mut() {
        inner.elseifs.push(entry);
        inner.elseifs.extend(sub_elseifs);
        inner.else_block = sub_else_block;
    }
}

/// Folds `t.k = v` statements that follow `t = {}` into the constructor.
fn fill_constructors(statements: &NodeRef) {
    let contents = traverse::list_contents(statements);
    let mut patched: Vec<NodeRef> = Vec::new();

    let mut index = 0usize;
    while index < contents.len() {
        let statement = contents[index].clone();
        patched.push(statement.clone());
        index += 1;

        let Node::Assignment(assignment) = &*statement.borrow() else {
            continue;
        };
        let expressions = traverse::list_contents(&assignment.expressions);
        let Some(source) = expressions.first().cloned() else {
            continue;
        };
        if !matches!(&*source.borrow(), Node::TableConstructor(_)) {
            continue;
        }

        let destinations = traverse::list_contents(&assignment.destinations);
        let Some(destination) = destinations.first().cloned() else {
            continue;
        };
        if destinations.len() != 1 {
            continue;
        }

        index += fill_constructor(&destination, &source, &contents[index..]);
    }

    traverse::set_list_contents(statements, patched);
}

/// Folds the following field assignments into `constructor`.
///
/// Returns how many statements were consumed.
fn fill_constructor(table: &NodeRef, constructor: &NodeRef, statements: &[NodeRef]) -> usize {
    let mut consumed = 0;

    for statement in statements {
        let Node::Assignment(assignment) = &*statement.borrow() else {
            break;
        };

        let destinations = traverse::list_contents(&assignment.destinations);
        if destinations.len() != 1 {
            break;
        }
        let Node::TableElement(element) = &*destinations[0].borrow() else {
            break;
        };
        if !is_equal(&element.table, table, false) {
            break;
        }

        let expressions = traverse::list_contents(&assignment.expressions);
        let Some(source) = expressions.first().cloned() else {
            break;
        };
        if expressions.len() != 1 {
            break;
        }
        if has_same_table(&source, table) {
            break;
        }

        if !insert_table_record(constructor, &element.key, &source, false) {
            break;
        }

        consumed += 1;
    }

    consumed
}

/// The position of a block in its list.
fn block_index(block: &NodeRef) -> u32 {
    match &*block.borrow() {
        Node::Block(inner) => inner.index,
        _ => u32::MAX,
    }
}

/// What a warp does, for the few places that only need the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarpKind {
    Unconditional,
    Conditional,
    Iterator,
    NumericLoop,
    End,
    Other,
}

fn warp_kind(warp: &Node) -> WarpKind {
    match warp {
        Node::UnconditionalWarp(_) => WarpKind::Unconditional,
        Node::ConditionalWarp(_) => WarpKind::Conditional,
        Node::IteratorWarp(_) => WarpKind::Iterator,
        Node::NumericLoopWarp(_) => WarpKind::NumericLoop,
        Node::EndWarp(_) => WarpKind::End,
        _ => WarpKind::Other,
    }
}

/// Whether two optional nodes are the same node.
fn same(a: Option<&NodeRef>, b: Option<&NodeRef>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// The loop body and way out of an iterator or numeric loop warp.
fn loop_warp_targets(warp: &Node) -> Option<(NodeRef, Option<NodeRef>)> {
    match warp {
        Node::IteratorWarp(inner) => Some((inner.body.clone()?, inner.way_out.clone())),
        Node::NumericLoopWarp(inner) => Some((inner.body.clone()?, inner.way_out.clone())),
        _ => None,
    }
}

/// The target of an unconditional warp, and whether it jumps.
fn jump_of(warp: &Node) -> Option<(UnconditionalWarpKind, Option<NodeRef>)> {
    match warp {
        Node::UnconditionalWarp(inner) => Some((inner.kind, inner.target.clone())),
        _ => None,
    }
}

/// The targets of a conditional warp.
fn conditional_targets(warp: &Node) -> Option<(Option<NodeRef>, Option<NodeRef>, Option<u32>)> {
    match warp {
        Node::ConditionalWarp(inner) => Some((
            inner.true_target.clone(),
            inner.false_target.clone(),
            inner.slot,
        )),
        _ => None,
    }
}

struct LoopWarpSwapper {
    /// `(body, way_out)` of every loop seen so far in this function.
    loops: Vec<(NodeRef, Option<NodeRef>)>,
    /// Blocks that hold an `UCLO` return, remembered so that they can be
    /// turned into a `break` once all the loops are known.
    jumps: Vec<(Vec<NodeRef>, usize)>,
}

struct SimpleLoopWarpSwapper {
    states: Vec<LoopWarpSwapper>,
}

impl SimpleLoopWarpSwapper {
    fn new() -> Self {
        SimpleLoopWarpSwapper { states: Vec::new() }
    }

    fn state(&mut self) -> &mut LoopWarpSwapper {
        self.states
            .last_mut()
            .expect("a function is always entered")
    }

    /// Exchanges the warp of the block that closes a loop with the warp of the
    /// block the loop body starts after.
    ///
    /// `end_index` is the position of the block holding the loop warp, which
    /// ljd calls the end of the loop: for a numeric loop that is the block with
    /// the `FORL` in it.
    fn swap_loop_warp(&mut self, blocks: &mut [NodeRef], end_index: usize, iterator: bool) {
        let Some(end) = blocks.get(end_index).cloned() else {
            return;
        };
        let Some(warp) = traverse::block_warp(&end) else {
            return;
        };

        let body = {
            let borrowed = warp.borrow();
            match loop_warp_targets(&borrowed) {
                Some((body, _)) if (warp_kind(&borrowed) == WarpKind::Iterator) == iterator => body,
                _ => return,
            }
        };

        let Some(body_index) = traverse::position(blocks, &body) else {
            return;
        };
        if body_index == 0 {
            return;
        }

        let Some(start) = blocks.get(body_index - 1).cloned() else {
            return;
        };
        let Some(start_warp) = traverse::block_warp(&start) else {
            return;
        };

        let expected = if iterator {
            UnconditionalWarpKind::Jump
        } else {
            UnconditionalWarpKind::Flow
        };
        // A generic loop opens with a jump to the test at its end, while a
        // numeric loop flows straight into its body.
        let expected_target = if iterator { &end } else { &body };
        let matches = {
            let borrowed = start_warp.borrow();
            match jump_of(&borrowed) {
                Some((kind, target)) => {
                    kind == expected
                        && target
                            .as_ref()
                            .is_some_and(|t| Rc::ptr_eq(t, expected_target))
                }
                None => false,
            }
        };
        if !matches {
            return;
        }

        let end_addr = warp.borrow().addr();
        let start_addr = start_warp.borrow().addr();

        // The warp of the loop start becomes the jump back that closes the
        // loop; the loop marker takes its place. The addresses travel with the
        // nodes, so the debug information stays readable.
        if let Node::UnconditionalWarp(inner) = &mut *start_warp.borrow_mut() {
            inner.meta = Meta::new(end_addr.unwrap_or(0), inner.meta.line);
            inner.kind = UnconditionalWarpKind::Jump;
            inner.target = Some(start.clone());
        }
        set_meta(&warp, Meta::new(start_addr.unwrap_or(0), 0));

        traverse::set_block_warp(&end, start_warp);
        traverse::set_block_warp(&start, warp);
    }

    /// Replaces a conditional warp with two identical targets by a branch on
    /// the constant `false`.
    ///
    /// The true target leads through a chain of blocks that only hold a no-op
    /// until it reaches the false target, which means neither outcome of the
    /// original comparison could have changed where execution goes.
    fn simplify_unreachable_conditional_warps(&mut self, blocks: &mut Vec<NodeRef>, index: usize) {
        let Some(block) = blocks.get(index).cloned() else {
            return;
        };
        let Some(warp) = traverse::block_warp(&block) else {
            return;
        };
        let Some((target, false_target, _)) = ({
            let borrowed = warp.borrow();
            conditional_targets(&borrowed)
        }) else {
            return;
        };
        let Some(target) = target else {
            return;
        };

        // Walk the chain of no-op blocks the true target jumps through.
        let mut current = target.clone();
        let end_of_chain = loop {
            let Some(node_warp) = traverse::block_warp(&current) else {
                return;
            };
            let is_jump = {
                let borrowed = node_warp.borrow();
                matches!(jump_of(&borrowed), Some((UnconditionalWarpKind::Jump, _)))
            };
            if !is_jump {
                return;
            }

            let contents = traverse::block_contents(&current);
            if !contents.is_empty()
                && (contents.len() > 1 || !matches!(&*contents[0].borrow(), Node::NoOp(_)))
            {
                return;
            }

            if !Rc::ptr_eq(&current, &target) {
                break current.clone();
            }

            let Some(next) = ({
                let borrowed = node_warp.borrow();
                jump_of(&borrowed).and_then(|(_, target)| target)
            }) else {
                return;
            };
            current = next;
        };

        let node_warp = match traverse::block_warp(&end_of_chain) {
            Some(warp) => warp,
            None => return,
        };
        let reaches_false = {
            let borrowed = node_warp.borrow();
            jump_of(&borrowed).and_then(|(_, target)| target)
        };
        if !same(reaches_false.as_ref(), false_target.as_ref()) {
            return;
        }

        let Some(old_index) = traverse::position(blocks, &end_of_chain) else {
            return;
        };
        let Some(next_block) = blocks.get(old_index + 1).cloned() else {
            return;
        };

        if let Node::Block(inner) = &mut *next_block.borrow_mut() {
            inner.warpins_count += 1;
        }
        if let Some(reaches_false) = &reaches_false
            && let Node::Block(inner) = &mut *reaches_false.borrow_mut()
        {
            inner.warpins_count -= 1;
        }

        blocks.remove(old_index);

        // The block needs a condition again: the true branch now skips the
        // block that used to hold the `false` constant.
        let condition = node(Node::Identifier(Box::new(Identifier::new(
            IdentifierKind::Slot,
            SLOT_FALSE,
            Meta::default(),
        ))));
        let new_warp = node(Node::ConditionalWarp(Box::new(ConditionalWarp {
            condition: Some(condition),
            true_target: Some(next_block),
            false_target,
            slot: Some(SLOT_FALSE),
            meta: Meta::new(warp.borrow().addr().unwrap_or(0), 0),
        })));
        traverse::set_block_warp(&target, new_warp);

        if let Node::Block(inner) = &mut *target.borrow_mut() {
            inner.last_address += 1;
        }
        let mut contents = traverse::block_contents(&target);
        contents.remove(0);
        traverse::set_block_contents(&target, contents);
    }

    /// Inserts the block that reads the value a condition was stored in.
    ///
    /// LuaJIT evaluates a condition into a register of its own when the source
    /// used the value as well, as in `x = a < b`; the extra block keeps the
    /// graph a tree so that the unwarper can handle it.
    fn create_dummy_block(&self, block: &NodeRef, slot: u32) -> NodeRef {
        let (address, index) = match &*block.borrow() {
            Node::Block(inner) => (inner.last_address, inner.index),
            _ => (0, 0),
        };

        let statement = node(Node::Assignment(Box::new(Assignment {
            expressions: expressions(vec![node(Node::Identifier(Box::new(Identifier::new(
                IdentifierKind::Slot,
                slot,
                Meta::default(),
            ))))]),
            destinations: variables(vec![node(Node::Identifier(Box::new(Identifier::new(
                IdentifierKind::Slot,
                slot,
                Meta::default(),
            ))))]),
            kind: AssignmentKind::Normal,
            meta: Meta::default(),
        })));

        let false_target = traverse::block_warp(block).and_then(|warp| {
            let borrowed = warp.borrow();
            conditional_targets(&borrowed).and_then(|(_, false_target, _)| false_target)
        });

        let new_block = node(Node::Block(Box::new(Block::new(
            index + 1,
            address,
            address,
        ))));
        let flow = node(Node::UnconditionalWarp(Box::new(UnconditionalWarp {
            kind: UnconditionalWarpKind::Flow,
            target: false_target,
            is_uclo: false,
            meta: Meta::default(),
        })));
        if let Node::Block(inner) = &mut *new_block.borrow_mut() {
            inner.warpins_count = 1;
            inner.contents.push(statement);
            inner.warp = Some(flow);
        }

        if let Some(warp) = traverse::block_warp(block)
            && let Node::ConditionalWarp(inner) = &mut *warp.borrow_mut()
        {
            inner.true_target = Some(new_block.clone());
        }

        new_block
    }

    /// Remembers a `return` that only closes an upvalue, so that it can be
    /// turned into a `break` once the surrounding loops are known.
    fn note_uclo_return(&mut self, blocks: &[NodeRef], index: usize) {
        let Some(block) = blocks.get(index) else {
            return;
        };
        let Some(warp) = traverse::block_warp(block) else {
            return;
        };
        let target = {
            let borrowed = warp.borrow();
            jump_of(&borrowed).and_then(|(_, target)| target)
        };
        let Some(target) = target else {
            return;
        };

        let contents = traverse::block_contents(&target);
        if contents.len() != 1 || !matches!(&*contents[0].borrow(), Node::Return(_)) {
            return;
        }

        let is_return = traverse::block_contents(block)
            .last()
            .is_some_and(|last| matches!(&*last.borrow(), Node::Return(_)));
        if is_return {
            return;
        }

        self.state().jumps.push((blocks.to_vec(), index));
    }

    /// Walks the blocks of one statement list, fixing up the warps between
    /// them.
    ///
    /// The list can be shortened while it is being walked, exactly like ljd
    /// does: the index is only advanced by hand, so a removed block shifts the
    /// rest of the walk.
    fn visit_blocks(&mut self, node: &NodeRef) {
        let mut blocks = traverse::list_contents(node);
        let mut fixed: Vec<NodeRef> = Vec::with_capacity(blocks.len());
        let mut index_shift = 0i64;

        let mut index = 0;
        while index < blocks.len() {
            let block = blocks[index].clone();
            let original_index = block_index(&block);
            let warp = traverse::block_warp(&block);
            fixed.push(block.clone());

            if let Node::Block(inner) = &mut *block.borrow_mut() {
                inner.index = (i64::from(inner.index) + index_shift) as u32;
            }

            let kind = warp
                .as_ref()
                .map(|warp| warp_kind(&warp.borrow()))
                .unwrap_or(WarpKind::Other);

            match kind {
                WarpKind::Iterator | WarpKind::NumericLoop => {
                    self.swap_loop_warp(&mut blocks, index, kind == WarpKind::Iterator);

                    if let Some(warp) = traverse::block_warp(&block)
                        && let Some((body, way_out)) = loop_warp_targets(&warp.borrow())
                    {
                        self.state().loops.push((body, way_out));
                    }
                    index += 1;
                    continue;
                }
                _ => {}
            }

            let Some(warp) = warp else {
                index += 1;
                continue;
            };

            let is_uclo = {
                let borrowed = warp.borrow();
                jump_of(&borrowed).is_some()
                    && matches!(&*borrowed, Node::UnconditionalWarp(inner) if inner.is_uclo)
            };
            if is_uclo && index + 1 < blocks.len() {
                self.note_uclo_return(&blocks, index);
            }

            let targets = {
                let borrowed = warp.borrow();
                conditional_targets(&borrowed)
            };
            if let Some((true_target, false_target, slot)) = targets {
                if !same(true_target.as_ref(), false_target.as_ref()) {
                    self.simplify_unreachable_conditional_warps(&mut blocks, index);
                } else if let Some(slot) = slot {
                    // The block that reads the condition has to be the one
                    // that follows, which is what ljd asserts. When it is not
                    // there is nothing sensible to add: the condition is not a
                    // value that was stored somewhere first.
                    let expected = original_index + 1;
                    let actual = false_target.as_ref().map(block_index);
                    if actual == Some(expected) {
                        let new_block = self.create_dummy_block(&block, slot);
                        fixed.push(new_block);
                        index_shift += 1;
                    }
                }
            }

            index += 1;
        }

        traverse::set_list_contents(node, fixed);
    }

    /// Finishes the `UCLO` returns of a function once all its loops are known.
    fn finish_function(&mut self) {
        let state = self.states.pop().expect("a function is always entered");

        for (blocks, index) in state.jumps {
            let Some(block) = blocks.get(index).cloned() else {
                continue;
            };
            let Some(warp) = traverse::block_warp(&block) else {
                continue;
            };
            let target = {
                let borrowed = warp.borrow();
                jump_of(&borrowed).and_then(|(_, target)| target)
            };
            let Some(target) = target else {
                continue;
            };

            // Prefer a `break` when this return leaves a loop that directly
            // encloses it.
            let mut use_break = false;
            for (start, end) in &state.loops {
                if same(end.as_ref(), Some(&target)) {
                    use_break = traverse::position(&blocks, start) < Some(index);
                    break;
                }
            }

            let statement = if use_break {
                node(Node::Break)
            } else {
                let contents = traverse::block_contents(&target);
                if contents.is_empty() {
                    continue;
                }
                let statement = contents[0].clone();
                traverse::set_block_contents(&target, Vec::new());
                statement
            };

            let address = traverse::block_range(&block).map(|(_, last)| last);
            let mut contents = traverse::block_contents(&block);
            contents.push(statement.clone());
            traverse::set_block_contents(&block, contents);
            set_meta(&statement, Meta::new(address.unwrap_or(0), 0));

            if let Node::UnconditionalWarp(inner) = &mut *warp.borrow_mut() {
                inner.kind = UnconditionalWarpKind::Flow;
                inner.target = blocks.get(index + 1).cloned();
            }
        }
    }
}

impl Visitor for SimpleLoopWarpSwapper {
    fn visit(&mut self, node: &NodeRef) -> bool {
        enum Action {
            None,
            Enter,
            Blocks,
        }

        let action = {
            let borrowed = node.borrow();
            match &*borrowed {
                Node::FunctionDefinition(_) => Action::Enter,
                Node::Statements(_) => Action::Blocks,
                _ => Action::None,
            }
        };

        match action {
            Action::None => {}
            Action::Enter => self.states.push(LoopWarpSwapper {
                loops: Vec::new(),
                jumps: Vec::new(),
            }),
            Action::Blocks => self.visit_blocks(node),
        }

        true
    }

    fn leave(&mut self, node: &NodeRef) {
        if matches!(&*node.borrow(), Node::FunctionDefinition(_)) {
            self.finish_function();
        }
    }
}
