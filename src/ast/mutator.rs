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

use oxc_allocator::{Allocator, ArenaBox};

use super::helpers::{has_same_table, insert_table_record, is_equal};
use super::nodes::*;
use super::traverse::{self, Visitor};
use crate::bytecode::SLOT_FALSE;

/// Rewrites the graph into the shape the later passes expect.
pub fn pre_pass<'a>(alloc: &'a Allocator, root: NodeRef<'a>) {
    traverse::traverse(&mut SimpleLoopWarpSwapper::new(alloc), root);
}

/// Cleans up the tree once the control flow is structured.
///
/// This is a port of ljd's `primary_pass`: `if` statements nested in an `else`
/// become `elseif` chains, and assignments that only fill in fields of a table
/// are folded back into its constructor.
pub fn primary_pass<'a>(alloc: &'a Allocator, root: NodeRef<'a>) {
    traverse::traverse(&mut MutatorVisitor { alloc }, root);
}

struct MutatorVisitor<'a> {
    alloc: &'a Allocator,
}

impl<'a> Visitor<'a> for MutatorVisitor<'a> {
    fn visit(&mut self, node: NodeRef<'a>) -> bool {
        if matches!(&*node.borrow(), Node::Statements(_)) {
            fill_constructors(self.alloc, node);
        }
        true
    }

    fn leave(&mut self, node: NodeRef<'a>) {
        if matches!(&*node.borrow(), Node::If(_)) {
            merge_elseif(self.alloc, node);
        }
    }
}

/// Turns an `if` that is the only statement of an `else` into an `elseif`.
fn merge_elseif<'a>(alloc: &'a Allocator, if_node: NodeRef<'a>) {
    let else_contents = match &*if_node.borrow() {
        Node::If(inner) => traverse::list_contents(inner.else_block),
        _ => return,
    };
    if else_contents.len() != 1 {
        return;
    }

    let Node::If(sub) = &*else_contents[0].borrow() else {
        return;
    };
    let (expression, then_block, sub_elseifs, sub_else_block) = (
        sub.expression,
        sub.then_block,
        sub.elseifs.iter().copied().collect::<Vec<_>>(),
        sub.else_block,
    );

    let entry = node(
        alloc,
        Node::ElseIf(ArenaBox::new_in(
            ElseIf {
                expression,
                then_block,
                meta: Meta::default(),
            },
            &alloc,
        )),
    );

    if let Node::If(inner) = &mut *if_node.borrow_mut() {
        inner.elseifs.push(entry);
        inner.elseifs.extend(sub_elseifs);
        inner.else_block = sub_else_block;
    }
}

/// Folds `t.k = v` statements that follow `t = {}` into the constructor.
fn fill_constructors<'a>(alloc: &'a Allocator, statements: NodeRef<'a>) {
    let contents = traverse::list_contents(statements);
    let mut patched: Vec<NodeRef<'a>> = Vec::new();

    let mut index = 0usize;
    while index < contents.len() {
        let statement = contents[index];
        patched.push(statement);
        index += 1;

        let Node::Assignment(assignment) = &*statement.borrow() else {
            continue;
        };
        let expressions = traverse::list_contents(assignment.expressions);
        let Some(source) = expressions.first().copied() else {
            continue;
        };
        if !matches!(&*source.borrow(), Node::TableConstructor(_)) {
            continue;
        }

        let destinations = traverse::list_contents(assignment.destinations);
        let Some(destination) = destinations.first().copied() else {
            continue;
        };
        if destinations.len() != 1 {
            continue;
        }

        index += fill_constructor(alloc, destination, source, &contents[index..]);
    }

    set_list_contents(alloc, statements, patched);
}

/// Folds the following field assignments into `constructor`.
///
/// Returns how many statements were consumed.
fn fill_constructor<'a>(
    alloc: &'a Allocator,
    table: NodeRef<'a>,
    constructor: NodeRef<'a>,
    statements: &[NodeRef<'a>],
) -> usize {
    let mut consumed = 0;

    for statement in statements {
        let Node::Assignment(assignment) = &*statement.borrow() else {
            break;
        };

        let destinations = traverse::list_contents(assignment.destinations);
        if destinations.len() != 1 {
            break;
        }
        let Node::TableElement(element) = &*destinations[0].borrow() else {
            break;
        };
        let (element_table, element_key) = (element.table, element.key);
        if !is_equal(element_table, table, false) {
            break;
        }

        let expressions = traverse::list_contents(assignment.expressions);
        let Some(source) = expressions.first().copied() else {
            break;
        };
        if expressions.len() != 1 {
            break;
        }
        if has_same_table(source, table) {
            break;
        }

        if !insert_table_record(alloc, constructor, element_key, source, false) {
            break;
        }

        consumed += 1;
    }

    consumed
}

/// The position of a block in its list.
fn block_index(block: NodeRef<'_>) -> u32 {
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

fn warp_kind<'a>(warp: NodeRef<'a>) -> WarpKind {
    match &*warp.borrow() {
        Node::UnconditionalWarp(_) => WarpKind::Unconditional,
        Node::ConditionalWarp(_) => WarpKind::Conditional,
        Node::IteratorWarp(_) => WarpKind::Iterator,
        Node::NumericLoopWarp(_) => WarpKind::NumericLoop,
        Node::EndWarp(_) => WarpKind::End,
        _ => WarpKind::Other,
    }
}

/// Whether two optional nodes are the same node.
fn same<'a>(a: Option<NodeRef<'a>>, b: Option<NodeRef<'a>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => traverse::same_node(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// The loop body and way out of an iterator or numeric loop warp.
fn loop_warp_targets<'a>(warp: NodeRef<'a>) -> Option<(NodeRef<'a>, Option<NodeRef<'a>>)> {
    match &*warp.borrow() {
        Node::IteratorWarp(inner) => Some((inner.body?, inner.way_out)),
        Node::NumericLoopWarp(inner) => Some((inner.body?, inner.way_out)),
        _ => None,
    }
}

/// The target of an unconditional warp, and whether it jumps.
fn jump_of<'a>(warp: NodeRef<'a>) -> Option<(UnconditionalWarpKind, Option<NodeRef<'a>>)> {
    match &*warp.borrow() {
        Node::UnconditionalWarp(inner) => Some((inner.kind, inner.target)),
        _ => None,
    }
}

/// The targets of a conditional warp.
fn conditional_targets<'a>(
    warp: NodeRef<'a>,
) -> Option<(Option<NodeRef<'a>>, Option<NodeRef<'a>>, Option<u32>)> {
    match &*warp.borrow() {
        Node::ConditionalWarp(inner) => Some((inner.true_target, inner.false_target, inner.slot)),
        _ => None,
    }
}

struct LoopWarpSwapper<'a> {
    /// `(body, way_out)` of every loop seen so far in this function.
    loops: Vec<(NodeRef<'a>, Option<NodeRef<'a>>)>,
    /// Blocks that hold an `UCLO` return, remembered so that they can be
    /// turned into a `break` once all the loops are known.
    jumps: Vec<(Vec<NodeRef<'a>>, usize)>,
}

struct SimpleLoopWarpSwapper<'a> {
    alloc: &'a Allocator,
    states: Vec<LoopWarpSwapper<'a>>,
}

impl<'a> SimpleLoopWarpSwapper<'a> {
    fn new(alloc: &'a Allocator) -> Self {
        SimpleLoopWarpSwapper {
            alloc,
            states: Vec::new(),
        }
    }

    fn state(&mut self) -> &mut LoopWarpSwapper<'a> {
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
    fn swap_loop_warp(&mut self, blocks: &mut [NodeRef<'a>], end_index: usize, iterator: bool) {
        let Some(end) = blocks.get(end_index).copied() else {
            return;
        };
        let Some(warp) = traverse::block_warp(end) else {
            return;
        };

        let body = match loop_warp_targets(warp) {
            Some((body, _)) if (warp_kind(warp) == WarpKind::Iterator) == iterator => body,
            _ => return,
        };

        let Some(body_index) = traverse::position(blocks, body) else {
            return;
        };
        if body_index == 0 {
            return;
        }

        let Some(start) = blocks.get(body_index - 1).copied() else {
            return;
        };
        let Some(start_warp) = traverse::block_warp(start) else {
            return;
        };

        let expected = if iterator {
            UnconditionalWarpKind::Jump
        } else {
            UnconditionalWarpKind::Flow
        };
        // A generic loop opens with a jump to the test at its end, while a
        // numeric loop flows straight into its body.
        let expected_target = if iterator { end } else { body };
        let matches = match jump_of(start_warp) {
            Some((kind, target)) => {
                kind == expected && target.is_some_and(|t| traverse::same_node(t, expected_target))
            }
            None => false,
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
            inner.target = Some(start);
        }
        set_meta(warp, Meta::new(start_addr.unwrap_or(0), 0));

        traverse::set_block_warp(end, start_warp);
        traverse::set_block_warp(start, warp);
    }

    /// Replaces a conditional warp with two identical targets by a branch on
    /// the constant `false`.
    ///
    /// The true target leads through a chain of blocks that only hold a no-op
    /// until it reaches the false target, which means neither outcome of the
    /// original comparison could have changed where execution goes.
    fn simplify_unreachable_conditional_warps(
        &mut self,
        blocks: &mut Vec<NodeRef<'a>>,
        index: usize,
    ) {
        let Some(block) = blocks.get(index).copied() else {
            return;
        };
        let Some(warp) = traverse::block_warp(block) else {
            return;
        };
        let Some((target, false_target, _)) = conditional_targets(warp) else {
            return;
        };
        let Some(target) = target else {
            return;
        };

        // Walk the chain of no-op blocks the true target jumps through.
        let mut current = target;
        let end_of_chain = loop {
            let Some(node_warp) = traverse::block_warp(current) else {
                return;
            };
            let is_jump = matches!(jump_of(node_warp), Some((UnconditionalWarpKind::Jump, _)));
            if !is_jump {
                return;
            }

            let contents = traverse::block_contents(current);
            if !contents.is_empty()
                && (contents.len() > 1 || !matches!(&*contents[0].borrow(), Node::NoOp(_)))
            {
                return;
            }

            if !traverse::same_node(current, target) {
                break current;
            }

            let Some(next) = jump_of(node_warp).and_then(|(_, target)| target) else {
                return;
            };
            current = next;
        };

        let node_warp = match traverse::block_warp(end_of_chain) {
            Some(warp) => warp,
            None => return,
        };
        let reaches_false = jump_of(node_warp).and_then(|(_, target)| target);
        if !same(reaches_false, false_target) {
            return;
        }

        let Some(old_index) = traverse::position(blocks, end_of_chain) else {
            return;
        };
        let Some(next_block) = blocks.get(old_index + 1).copied() else {
            return;
        };

        if let Node::Block(inner) = &mut *next_block.borrow_mut() {
            inner.warpins_count += 1;
        }
        if let Some(reaches_false) = reaches_false
            && let Node::Block(inner) = &mut *reaches_false.borrow_mut()
        {
            inner.warpins_count -= 1;
        }

        blocks.remove(old_index);

        // The block needs a condition again: the true branch now skips the
        // block that used to hold the `false` constant.
        let condition = node(
            self.alloc,
            Node::Identifier(ArenaBox::new_in(
                Identifier::new(
                    self.alloc,
                    IdentifierKind::Slot,
                    SLOT_FALSE,
                    Meta::default(),
                ),
                &self.alloc,
            )),
        );
        let new_warp = node(
            self.alloc,
            Node::ConditionalWarp(ArenaBox::new_in(
                ConditionalWarp {
                    condition: Some(condition),
                    true_target: Some(next_block),
                    false_target,
                    slot: Some(SLOT_FALSE),
                    meta: Meta::new(warp.borrow().addr().unwrap_or(0), 0),
                },
                &self.alloc,
            )),
        );
        traverse::set_block_warp(target, new_warp);

        if let Node::Block(inner) = &mut *target.borrow_mut() {
            inner.last_address += 1;
        }
        let mut contents = traverse::block_contents(target);
        contents.remove(0);
        traverse::set_block_contents(self.alloc, target, contents);
    }

    /// Inserts the block that reads the value a condition was stored in.
    ///
    /// LuaJIT evaluates a condition into a register of its own when the source
    /// used the value as well, as in `x = a < b`; the extra block keeps the
    /// graph a tree so that the unwarper can handle it.
    fn create_dummy_block(&self, block: NodeRef<'a>, slot: u32) -> NodeRef<'a> {
        let (address, index) = match &*block.borrow() {
            Node::Block(inner) => (inner.last_address, inner.index),
            _ => (0, 0),
        };

        let operand = node(
            self.alloc,
            Node::Identifier(ArenaBox::new_in(
                Identifier::new(self.alloc, IdentifierKind::Slot, slot, Meta::default()),
                &self.alloc,
            )),
        );
        let destination = node(
            self.alloc,
            Node::Identifier(ArenaBox::new_in(
                Identifier::new(self.alloc, IdentifierKind::Slot, slot, Meta::default()),
                &self.alloc,
            )),
        );
        let statement = node(
            self.alloc,
            Node::Assignment(ArenaBox::new_in(
                Assignment {
                    expressions: expressions(self.alloc, vec![operand]),
                    destinations: variables(self.alloc, vec![destination]),
                    kind: AssignmentKind::Normal,
                    meta: Meta::default(),
                },
                &self.alloc,
            )),
        );

        let false_target = traverse::block_warp(block).and_then(|warp| {
            conditional_targets(warp).and_then(|(_, false_target, _)| false_target)
        });

        let new_block = node(
            self.alloc,
            Node::Block(ArenaBox::new_in(
                Block::new(self.alloc, index + 1, address, address),
                &self.alloc,
            )),
        );
        let flow = node(
            self.alloc,
            Node::UnconditionalWarp(ArenaBox::new_in(
                UnconditionalWarp {
                    kind: UnconditionalWarpKind::Flow,
                    target: false_target,
                    is_uclo: false,
                    meta: Meta::default(),
                },
                &self.alloc,
            )),
        );
        if let Node::Block(inner) = &mut *new_block.borrow_mut() {
            inner.warpins_count = 1;
            inner.contents.push(statement);
            inner.warp = Some(flow);
        }

        if let Some(warp) = traverse::block_warp(block)
            && let Node::ConditionalWarp(inner) = &mut *warp.borrow_mut()
        {
            inner.true_target = Some(new_block);
        }

        new_block
    }

    /// Remembers a `return` that only closes an upvalue, so that it can be
    /// turned into a `break` once the surrounding loops are known.
    fn note_uclo_return(&mut self, blocks: &[NodeRef<'a>], index: usize) {
        let Some(block) = blocks.get(index).copied() else {
            return;
        };
        let Some(warp) = traverse::block_warp(block) else {
            return;
        };
        let target = jump_of(warp).and_then(|(_, target)| target);
        let Some(target) = target else {
            return;
        };

        let contents = traverse::block_contents(target);
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
    fn visit_blocks(&mut self, node: NodeRef<'a>) {
        let mut blocks = traverse::list_contents(node);
        let mut fixed: Vec<NodeRef<'a>> = Vec::with_capacity(blocks.len());
        let mut index_shift = 0i64;

        let mut index = 0;
        while index < blocks.len() {
            let block = blocks[index];
            let original_index = block_index(block);
            let warp = traverse::block_warp(block);
            fixed.push(block);

            if let Node::Block(inner) = &mut *block.borrow_mut() {
                inner.index = (i64::from(inner.index) + index_shift) as u32;
            }

            let kind = warp.map(warp_kind).unwrap_or(WarpKind::Other);

            match kind {
                WarpKind::Iterator | WarpKind::NumericLoop => {
                    self.swap_loop_warp(&mut blocks, index, kind == WarpKind::Iterator);

                    if let Some(warp) = traverse::block_warp(block)
                        && let Some((body, way_out)) = loop_warp_targets(warp)
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

            let is_uclo = matches!(
                &*warp.borrow(),
                Node::UnconditionalWarp(inner) if inner.is_uclo
            );
            if is_uclo && index + 1 < blocks.len() {
                self.note_uclo_return(&blocks, index);
            }

            if let Some((true_target, false_target, slot)) = conditional_targets(warp) {
                if !same(true_target, false_target) {
                    self.simplify_unreachable_conditional_warps(&mut blocks, index);
                } else if let Some(slot) = slot {
                    // The block that reads the condition has to be the one
                    // that follows, which is what ljd asserts. When it is not
                    // there is nothing sensible to add: the condition is not a
                    // value that was stored somewhere first.
                    let expected = original_index + 1;
                    let actual = false_target.map(block_index);
                    if actual == Some(expected) {
                        let new_block = self.create_dummy_block(block, slot);
                        fixed.push(new_block);
                        index_shift += 1;
                    }
                }
            }

            index += 1;
        }

        set_list_contents(self.alloc, node, fixed);
    }

    /// Finishes the `UCLO` returns of a function once all its loops are known.
    fn finish_function(&mut self) {
        let state = self.states.pop().expect("a function is always entered");

        for (blocks, index) in state.jumps {
            let Some(block) = blocks.get(index).copied() else {
                continue;
            };
            let Some(warp) = traverse::block_warp(block) else {
                continue;
            };
            let target = jump_of(warp).and_then(|(_, target)| target);
            let Some(target) = target else {
                continue;
            };

            // Prefer a `break` when this return leaves a loop that directly
            // encloses it.
            let mut use_break = false;
            for (start, end) in &state.loops {
                if same(*end, Some(target)) {
                    use_break = traverse::position(&blocks, start) < Some(index);
                    break;
                }
            }

            let statement = if use_break {
                node(self.alloc, Node::Break)
            } else {
                let contents = traverse::block_contents(target);
                if contents.is_empty() {
                    continue;
                }
                let statement = contents[0];
                traverse::set_block_contents(self.alloc, target, Vec::new());
                statement
            };

            let address = traverse::block_range(block).map(|(_, last)| last);
            let mut contents = traverse::block_contents(block);
            contents.push(statement);
            traverse::set_block_contents(self.alloc, block, contents);
            set_meta(statement, Meta::new(address.unwrap_or(0), 0));

            if let Node::UnconditionalWarp(inner) = &mut *warp.borrow_mut() {
                inner.kind = UnconditionalWarpKind::Flow;
                inner.target = blocks.get(index + 1).copied();
            }
        }
    }
}

impl<'a> Visitor<'a> for SimpleLoopWarpSwapper<'a> {
    fn visit(&mut self, node: NodeRef<'a>) -> bool {
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

    fn leave(&mut self, node: NodeRef<'a>) {
        if matches!(&*node.borrow(), Node::FunctionDefinition(_)) {
            self.finish_function();
        }
    }
}
