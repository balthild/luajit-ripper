//! Walking and rewriting the AST.
//!
//! The control flow graph is stored alongside the statements: a warp may point
//! at a block that is *not* one of its children. Traversal therefore
//! deliberately does not follow warp targets, exactly like ljd does; passes
//! that need the graph use [`block_targets`](NodeBlockExt::block_targets)
//! instead.
//!
//! What reads and replaces the contents of a list or block node lives here too:
//! the five list nodes hold their contents directly rather than in a child, and
//! a block node does the same.

use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use super::accessors::NodeAccessorExt;
use super::nodes::*;

// MARK: children

impl<'a> Node<'a> {
    /// The direct children of a node, in the order ljd visits them.
    ///
    /// Warp targets are not included: they are part of the graph, not of the
    /// tree.
    ///
    /// The result is a plain vector: it is the list of children to walk, not
    /// part of the tree, and every entry is a cheap copy of an [`ArenaNode`].
    pub fn children(&self) -> Vec<ArenaNode<'a>> {
        match self {
            Node::Statements(items)
            | Node::Expressions(items)
            | Node::Variables(items)
            | Node::Identifiers(items)
            | Node::Records(items) => items.iter().copied().collect(),
            Node::Assignment(inner) => vec![inner.expressions, inner.destinations],
            Node::FunctionCall(inner) => vec![inner.arguments, inner.function],
            Node::Return(inner) => vec![inner.returns],
            Node::If(inner) => {
                let mut out = vec![inner.expression, inner.then_block];
                out.extend(inner.elseifs.iter().copied());
                out.push(inner.else_block);
                out
            }
            Node::ElseIf(inner) => vec![inner.expression, inner.then_block],
            Node::While(inner) => vec![inner.expression, inner.statements],
            Node::RepeatUntil(inner) => vec![inner.statements, inner.expression],
            Node::NumericFor(inner) => vec![inner.variable, inner.expressions, inner.statements],
            Node::IteratorFor(inner) => {
                vec![inner.expressions, inner.identifiers, inner.statements]
            }
            Node::FunctionDefinition(inner) => vec![inner.arguments, inner.statements],
            Node::TableElement(inner) => vec![inner.key, inner.table],
            Node::TableConstructor(inner) => vec![inner.array, inner.records],
            Node::BinaryOperator(inner) => vec![inner.left, inner.right],
            Node::UnaryOperator(inner) => vec![inner.operand],
            Node::ArrayRecord(inner) => vec![inner.value],
            Node::TableRecord(inner) => vec![inner.key, inner.value],
            Node::Block(inner) => {
                let mut out: Vec<ArenaNode<'a>> = inner.contents.iter().copied().collect();
                if let Some(warp) = inner.warp {
                    out.push(warp);
                }
                out
            }
            Node::ConditionalWarp(inner) => inner.condition.iter().copied().collect(),
            Node::IteratorWarp(inner) => vec![inner.variables, inner.controls],
            Node::NumericLoopWarp(inner) => vec![inner.index, inner.controls],
            Node::EndWarp(_)
            | Node::UnconditionalWarp(_)
            | Node::Break
            | Node::NoOp(_)
            | Node::Identifier(_)
            | Node::Constant(_)
            | Node::Primitive(_)
            | Node::Vararg
            | Node::MulTres => Vec::new(),
        }
    }
}

// MARK: iterating

impl<'a> ArenaNode<'a> {
    /// Every node in this subtree, this one included, in depth first order.
    ///
    /// Nodes that are reachable through more than one path are reported once.
    ///
    /// This is lazy: nothing past the entry the caller stops at is visited, so a
    /// question answered by the first few entries costs only those. Because the
    /// tree is read as the iterator advances, it must not be rewritten while the
    /// iterator is alive; a pass that rewrites during its walk uses
    /// [`traverse`] instead.
    pub fn iter_nodes(self) -> impl Iterator<Item = ArenaNode<'a>> {
        let mut stack = vec![self];
        let mut seen = HashSet::new();

        std::iter::from_fn(move || {
            while let Some(current) = stack.pop() {
                if !seen.insert(current) {
                    continue;
                }
                let kids = current.borrow().children();
                stack.extend(kids.into_iter().rev());
                return Some(current);
            }
            None
        })
    }
}

// MARK: visiting

/// A hook based tree walk, mirroring ljd's visitor protocol.
///
/// [`visit`](Visitor::visit) is called before a node's children,
/// [`leave`](Visitor::leave) after them. Passes that only care about a few node
/// kinds match on the node inside the hook.
///
/// The trait carries the arena lifetime, so a visitor can store the nodes it is
/// handed instead of copying them out. The hooks take the node by value: a
/// [`ArenaNode`] is a copyable reference, so a pass that wants to keep it just
/// stores it.
pub trait Visitor<'a> {
    /// Called when `node` is entered.
    ///
    /// Returning `false` skips the node entirely: its children are not visited
    /// and [`leave`](Visitor::leave) is not called for it.
    fn visit(&mut self, node: ArenaNode<'a>) -> bool {
        let _ = node;
        true
    }

    /// Called after all children of `node` have been visited.
    fn leave(&mut self, node: ArenaNode<'a>) {
        let _ = node;
    }
}

impl<'a> ArenaNode<'a> {
    /// Walks this node depth first, notifying `visitor` on entry and exit.
    ///
    /// A node that is reachable twice is only visited once, which mirrors the
    /// loop protection ljd puts into table constructors; the rest of the tree is
    /// a tree in practice.
    ///
    /// This is the rewriting counterpart of
    /// [`iter_nodes`](ArenaNode::iter_nodes): the visitor may change the tree as
    /// it goes, which the lazy iterator cannot offer. It is named `traverse`
    /// rather than `visit` so that it does not read like the opposite of
    /// [`Visitor::visit`], which takes the two arguments the other way round.
    pub fn traverse(self, visitor: &mut impl Visitor<'a>) {
        let mut seen = HashSet::new();
        self.traverse_into(visitor, &mut seen);
    }

    fn traverse_into(self, visitor: &mut impl Visitor<'a>, seen: &mut HashSet<ArenaNode<'a>>) {
        if !seen.insert(self) {
            return;
        }
        if !visitor.visit(self) {
            return;
        }

        // The borrow has to end before the children are walked: a visitor is
        // allowed to change the list of the node it is looking at.
        let children = self.borrow().children();
        for child in children {
            child.traverse_into(visitor, seen);
        }

        visitor.leave(self);
    }
}

// MARK: copying

impl<'a> ArenaNode<'a> {
    /// Copies this node and everything below it into `alloc`.
    ///
    /// ljd uses `copy.deepcopy` when it has to move a subtree into a second
    /// place; sharing the node between the two places instead would make later
    /// changes visible in both.
    pub fn deep_clone(self, alloc: &'a Allocator) -> ArenaNode<'a> {
        let mut copies = HashMap::new();
        deep_clone_into(alloc, self, &mut copies)
    }
}

/// Copies a list of nodes, giving two occurrences of the same node one copy.
///
/// This is what keeps a copy of a graph with shared children a graph with
/// shared children.
fn deep_clone_list<'a>(
    alloc: &'a Allocator,
    nodes: impl IntoIterator<Item = ArenaNode<'a>>,
    copies: &mut HashMap<ArenaNode<'a>, ArenaNode<'a>>,
) -> Vec<ArenaNode<'a>> {
    nodes
        .into_iter()
        .map(|node| deep_clone_into(alloc, node, copies))
        .collect()
}

fn deep_clone_into<'a>(
    alloc: &'a Allocator,
    source: ArenaNode<'a>,
    copies: &mut HashMap<ArenaNode<'a>, ArenaNode<'a>>,
) -> ArenaNode<'a> {
    if let Some(copy) = copies.get(&source) {
        return *copy;
    }

    let shallow = clone_shallow(alloc, &source.borrow(), copies);
    let copy = ArenaNode::new(alloc, shallow);
    copies.insert(source, copy);

    // Rewriting the children of the copy is what makes this a deep copy. Every
    // occurrence is replaced, so that a child used twice stays shared instead
    // of half of it pointing into the original graph.
    let kids = copy.borrow().children();
    for child in kids {
        let cloned = deep_clone_into(alloc, child, copies);
        if cloned.same_as(child) {
            continue;
        }
        while copy.borrow_mut().replace_child(child, cloned) {}
    }

    copy
}

/// Copies a node one level deep: the children are still the originals, which
/// [`deep_clone_into`] replaces with copies afterwards.
fn clone_shallow<'a>(
    alloc: &'a Allocator,
    source: &Node<'a>,
    copies: &mut HashMap<ArenaNode<'a>, ArenaNode<'a>>,
) -> Node<'a> {
    let mut list = |nodes: &ArenaVec<'a, ArenaNode<'a>>| -> Vec<ArenaNode<'a>> {
        deep_clone_list(alloc, nodes.iter().copied(), copies)
    };

    match source {
        Node::Statements(items) => Node::Statements(ArenaVec::from_iter_in(list(items), &alloc)),
        Node::Expressions(items) => Node::Expressions(ArenaVec::from_iter_in(list(items), &alloc)),
        Node::Variables(items) => Node::Variables(ArenaVec::from_iter_in(list(items), &alloc)),
        Node::Identifiers(items) => Node::Identifiers(ArenaVec::from_iter_in(list(items), &alloc)),
        Node::Records(items) => Node::Records(ArenaVec::from_iter_in(list(items), &alloc)),
        Node::Assignment(inner) => Node::Assignment(ArenaBox::new_in(
            Assignment {
                expressions: inner.expressions,
                destinations: inner.destinations,
                kind: inner.kind,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::FunctionCall(inner) => Node::FunctionCall(ArenaBox::new_in(
            FunctionCall {
                function: inner.function,
                arguments: inner.arguments,
                is_method: inner.is_method,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::Return(inner) => Node::Return(ArenaBox::new_in(
            Return {
                returns: inner.returns,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::If(inner) => Node::If(ArenaBox::new_in(
            If {
                expression: inner.expression,
                then_block: inner.then_block,
                elseifs: ArenaVec::from_iter_in(list(&inner.elseifs), &alloc),
                else_block: inner.else_block,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::ElseIf(inner) => Node::ElseIf(ArenaBox::new_in(
            ElseIf {
                expression: inner.expression,
                then_block: inner.then_block,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::While(inner) => Node::While(ArenaBox::new_in(
            While {
                expression: inner.expression,
                statements: inner.statements,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::RepeatUntil(inner) => Node::RepeatUntil(ArenaBox::new_in(
            RepeatUntil {
                expression: inner.expression,
                statements: inner.statements,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::NumericFor(inner) => Node::NumericFor(ArenaBox::new_in(
            NumericFor {
                variable: inner.variable,
                expressions: inner.expressions,
                statements: inner.statements,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::IteratorFor(inner) => Node::IteratorFor(ArenaBox::new_in(
            IteratorFor {
                identifiers: inner.identifiers,
                expressions: inner.expressions,
                statements: inner.statements,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::FunctionDefinition(inner) => Node::FunctionDefinition(ArenaBox::new_in(
            FunctionDefinition {
                arguments: inner.arguments,
                statements: inner.statements,
                upvalues: ArenaVec::from_iter_in(inner.upvalues.iter().copied(), &alloc),
                // Debug information belongs to the prototype, not to the node,
                // so a copy shares it instead of duplicating it.
                debug: inner.debug,
                instruction_count: inner.instruction_count,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::Identifier(inner) => Node::Identifier(ArenaBox::new_in(
            Identifier {
                kind: inner.kind,
                name: inner.name,
                slot: inner.slot,
                id: inner.id,
                possible_ids: ArenaVec::from_iter_in(inner.possible_ids.iter().copied(), &alloc),
                local_end: inner.local_end,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::TableElement(inner) => Node::TableElement(ArenaBox::new_in(
            TableElement {
                table: inner.table,
                key: inner.key,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::Constant(inner) => Node::Constant(ArenaBox::new_in(
            Constant {
                value: inner.value.clone(),
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::Primitive(inner) => Node::Primitive(*inner),
        Node::TableConstructor(inner) => Node::TableConstructor(ArenaBox::new_in(
            TableConstructor {
                array: inner.array,
                records: inner.records,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::BinaryOperator(inner) => Node::BinaryOperator(ArenaBox::new_in(
            BinaryOperator {
                kind: inner.kind,
                left: inner.left,
                right: inner.right,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::UnaryOperator(inner) => Node::UnaryOperator(ArenaBox::new_in(
            UnaryOperator {
                kind: inner.kind,
                operand: inner.operand,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::ArrayRecord(inner) => Node::ArrayRecord(ArenaBox::new_in(
            ArrayRecord {
                value: inner.value,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::TableRecord(inner) => Node::TableRecord(ArenaBox::new_in(
            TableRecord {
                key: inner.key,
                value: inner.value,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::Block(inner) => Node::Block(ArenaBox::new_in(
            Block {
                index: inner.index,
                first_address: inner.first_address,
                last_address: inner.last_address,
                last_body_address: inner.last_body_address,
                warpins_count: inner.warpins_count,
                is_loop: inner.is_loop,
                contents: ArenaVec::from_iter_in(list(&inner.contents), &alloc),
                warp: inner.warp,
            },
            &alloc,
        )),
        Node::UnconditionalWarp(inner) => Node::UnconditionalWarp(ArenaBox::new_in(
            UnconditionalWarp {
                kind: inner.kind,
                target: inner.target,
                is_uclo: inner.is_uclo,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::ConditionalWarp(inner) => Node::ConditionalWarp(ArenaBox::new_in(
            ConditionalWarp {
                condition: inner.condition,
                true_target: inner.true_target,
                false_target: inner.false_target,
                slot: inner.slot,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::IteratorWarp(inner) => Node::IteratorWarp(ArenaBox::new_in(
            IteratorWarp {
                variables: inner.variables,
                controls: inner.controls,
                body: inner.body,
                way_out: inner.way_out,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::NumericLoopWarp(inner) => Node::NumericLoopWarp(ArenaBox::new_in(
            NumericLoopWarp {
                index: inner.index,
                controls: inner.controls,
                body: inner.body,
                way_out: inner.way_out,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::NoOp(inner) => Node::NoOp(ArenaBox::new_in(NoOp { meta: inner.meta }, &alloc)),
        Node::EndWarp(inner) => Node::EndWarp(ArenaBox::new_in(
            EndWarp {
                target: inner.target,
                meta: inner.meta,
            },
            &alloc,
        )),
        Node::Break => Node::Break,
        Node::Vararg => Node::Vararg,
        Node::MulTres => Node::MulTres,
    }
}

// MARK: lookups

impl<'a> ArenaNode<'a> {
    /// Every function definition in the tree, including this node if it is
    /// one.
    ///
    /// The result is a snapshot, collected up front: the passes rewrite the
    /// functions they are handed, which is only sound while the walk itself is
    /// over.
    pub fn functions(self) -> Vec<ArenaNode<'a>> {
        self.iter_nodes()
            .filter(|node| node.is_function_definition())
            .collect()
    }

    /// Every non empty statement list that belongs to this function itself.
    ///
    /// The lists inside a nested function definition belong to that function,
    /// so they are left out. That is what makes it possible to run a pass over
    /// one function at a time.
    pub fn own_statement_lists(self) -> Vec<ArenaNode<'a>> {
        let mut result = Vec::new();
        self.collect_own_statement_lists(&mut result);
        result
    }

    fn collect_own_statement_lists(&self, result: &mut Vec<ArenaNode<'a>>) {
        if self.is_statements_and(|items| !items.is_empty()) {
            result.push(*self);
        }

        for child in self.borrow().children() {
            if !child.is_function_definition() {
                child.collect_own_statement_lists(result);
            }
        }
    }
}

// MARK: replacing

impl<'a> Node<'a> {
    /// Replaces the child `old` of this node with `new`.
    ///
    /// Returns whether `old` was found.
    pub fn replace_child(&mut self, old: ArenaNode<'a>, new: ArenaNode<'a>) -> bool {
        match self {
            Node::Statements(items)
            | Node::Expressions(items)
            | Node::Variables(items)
            | Node::Identifiers(items)
            | Node::Records(items) => replace_in_list(items, old, new),
            Node::Assignment(inner) => {
                replace_slot(&mut inner.expressions, old, new)
                    || replace_slot(&mut inner.destinations, old, new)
            }
            Node::FunctionCall(inner) => {
                replace_slot(&mut inner.arguments, old, new)
                    || replace_slot(&mut inner.function, old, new)
            }
            Node::Return(inner) => replace_slot(&mut inner.returns, old, new),
            Node::If(inner) => {
                if replace_slot(&mut inner.expression, old, new)
                    || replace_slot(&mut inner.then_block, old, new)
                    || replace_slot(&mut inner.else_block, old, new)
                {
                    return true;
                }
                inner
                    .elseifs
                    .iter_mut()
                    .any(|branch| branch.borrow_mut().replace_child(old, new))
            }
            Node::ElseIf(inner) => {
                replace_slot(&mut inner.expression, old, new)
                    || replace_slot(&mut inner.then_block, old, new)
            }
            Node::While(inner) => {
                replace_slot(&mut inner.expression, old, new)
                    || replace_slot(&mut inner.statements, old, new)
            }
            Node::RepeatUntil(inner) => {
                replace_slot(&mut inner.expression, old, new)
                    || replace_slot(&mut inner.statements, old, new)
            }
            Node::NumericFor(inner) => {
                replace_slot(&mut inner.variable, old, new)
                    || replace_slot(&mut inner.expressions, old, new)
                    || replace_slot(&mut inner.statements, old, new)
            }
            Node::IteratorFor(inner) => {
                replace_slot(&mut inner.expressions, old, new)
                    || replace_slot(&mut inner.identifiers, old, new)
                    || replace_slot(&mut inner.statements, old, new)
            }
            Node::FunctionDefinition(inner) => {
                replace_slot(&mut inner.arguments, old, new)
                    || replace_slot(&mut inner.statements, old, new)
            }
            Node::TableElement(inner) => {
                replace_slot(&mut inner.key, old, new) || replace_slot(&mut inner.table, old, new)
            }
            Node::TableConstructor(inner) => {
                replace_slot(&mut inner.array, old, new)
                    || replace_slot(&mut inner.records, old, new)
            }
            Node::BinaryOperator(inner) => {
                replace_slot(&mut inner.left, old, new) || replace_slot(&mut inner.right, old, new)
            }
            Node::UnaryOperator(inner) => replace_slot(&mut inner.operand, old, new),
            Node::ArrayRecord(inner) => replace_slot(&mut inner.value, old, new),
            Node::TableRecord(inner) => {
                replace_slot(&mut inner.key, old, new) || replace_slot(&mut inner.value, old, new)
            }
            Node::Block(inner) => {
                if replace_in_list(&mut inner.contents, old, new) {
                    return true;
                }
                if let Some(warp) = &mut inner.warp {
                    return replace_slot(warp, old, new);
                }
                false
            }
            Node::ConditionalWarp(inner) => match &mut inner.condition {
                Some(condition) => replace_slot(condition, old, new),
                None => false,
            },
            Node::IteratorWarp(inner) => {
                replace_slot(&mut inner.variables, old, new)
                    || replace_slot(&mut inner.controls, old, new)
            }
            Node::NumericLoopWarp(inner) => {
                replace_slot(&mut inner.index, old, new)
                    || replace_slot(&mut inner.controls, old, new)
            }
            Node::EndWarp(_)
            | Node::UnconditionalWarp(_)
            | Node::Break
            | Node::NoOp(_)
            | Node::Identifier(_)
            | Node::Constant(_)
            | Node::Primitive(_)
            | Node::Vararg
            | Node::MulTres => false,
        }
    }
}

fn replace_slot<'a>(slot: &mut ArenaNode<'a>, old: ArenaNode<'a>, new: ArenaNode<'a>) -> bool {
    if (*slot).same_as(old) {
        *slot = new;
        true
    } else {
        false
    }
}

fn replace_in_list<'a>(
    items: &mut ArenaVec<'a, ArenaNode<'a>>,
    old: ArenaNode<'a>,
    new: ArenaNode<'a>,
) -> bool {
    for item in items.iter_mut() {
        if (*item).same_as(old) {
            *item = new;
            return true;
        }
    }
    false
}

// MARK: blocks

impl<'a> Node<'a> {
    /// The contents of a block node.
    ///
    /// This mirrors [`Node::list`](Node::list): a block holds its contents
    /// directly rather than in a child node.
    pub fn block(&self) -> Option<&ArenaVec<'a, ArenaNode<'a>>> {
        match self {
            Node::Block(inner) => Some(&inner.contents),
            _ => None,
        }
    }

    /// The contents of a block node, for mutation.
    pub fn block_mut(&mut self) -> Option<&mut ArenaVec<'a, ArenaNode<'a>>> {
        match self {
            Node::Block(inner) => Some(&mut inner.contents),
            _ => None,
        }
    }

    /// Points a block node at a warp.
    pub fn block_set_warp(&mut self, warp: ArenaNode<'a>) {
        if let Node::Block(inner) = self {
            inner.warp = Some(warp);
        }
    }

    /// Makes a block flow into `target`.
    ///
    /// The warp left behind is a fallthrough, not a jump: this is the
    /// unwarper's way of stitching two blocks together.
    pub fn block_set_flow_to(&mut self, alloc: &'a Allocator, target: ArenaNode<'a>) {
        let warp = ArenaNode::new(
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
        self.block_set_warp(warp);
    }

    /// Ends the region that starts at this block.
    ///
    /// `force_no_target` leaves the end without a target, which matters for
    /// loops: an end that points outside the loop would confuse the checks that
    /// follow.
    pub fn block_set_end(&mut self, alloc: &'a Allocator, force_no_target: bool) {
        let target = if force_no_target {
            None
        } else {
            self.block_warp().and_then(|warp| warp.target(true))
        };

        let end = ArenaNode::new(
            alloc,
            Node::EndWarp(ArenaBox::new_in(
                EndWarp {
                    target,
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );
        self.block_set_warp(end);
    }

    /// Replaces the contents of a block node.
    ///
    /// This is an inherent method rather than an extension method because it
    /// writes: a value that merely holds a node hands it back behind a shared
    /// borrow, so a caller that wants to change one has to borrow it mutably
    /// first. A node that is not a block is left alone.
    pub fn block_set_contents(
        &mut self,
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = ArenaNode<'a>>,
    ) {
        if let Some(items) = self.block_mut() {
            *items = ArenaVec::from_iter_in(contents, &alloc);
        }
    }
}

/// The contents of a block node, and its first and last entry, asked of
/// anything that may hold a node.
///
/// The counterpart of [`Node::block`](Node::block),
/// [`block_first`](NodeBlockExt::block_first),
/// [`block_last`](NodeBlockExt::block_last),
/// [`block_warp`](NodeBlockExt::block_warp) and
/// [`block_range`](NodeBlockExt::block_range) for a
/// value that may not hold a node at all, so that `maybe_node.block_contents()`
/// reads the same whether `maybe_node` is a [`Node`], a [`ArenaNode`] or an
/// `Option` of either.
pub trait NodeBlockExt<'a>: AsNode<'a> {
    /// The targets a warp can jump to.
    fn block_targets(&self) -> Vec<ArenaNode<'a>> {
        let node = self.as_node();
        let Some(node) = node.as_deref() else {
            return Vec::new();
        };

        match node {
            Node::UnconditionalWarp(inner) => inner.target.iter().copied().collect(),
            Node::ConditionalWarp(inner) => {
                let mut out = Vec::new();
                if let Some(target) = inner.false_target {
                    out.push(target);
                }
                if let Some(target) = inner.true_target {
                    out.push(target);
                }
                out
            }
            Node::IteratorWarp(inner) => {
                let mut out = Vec::new();
                if let Some(target) = inner.way_out {
                    out.push(target);
                }
                if let Some(target) = inner.body {
                    out.push(target);
                }
                out
            }
            Node::NumericLoopWarp(inner) => {
                let mut out = Vec::new();
                if let Some(target) = inner.way_out {
                    out.push(target);
                }
                if let Some(target) = inner.body {
                    out.push(target);
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// The contents of a block node.
    ///
    /// The copy is a plain `Vec`: it is a snapshot the caller works on, not part
    /// of the tree.
    fn block_contents(&self) -> Vec<ArenaNode<'a>> {
        let node = self.as_node();
        match node.as_deref().and_then(Node::block) {
            Some(items) => items.iter().copied().collect(),
            None => Vec::new(),
        }
    }

    /// The first entry of a block node.
    ///
    /// This is [`block_contents`](NodeBlockExt::block_contents) narrowed to its
    /// first entry: reaching for that entry through the copy would build the
    /// whole list first. A node that is not a block, or a block that is empty,
    /// has no first entry.
    fn block_first(&self) -> Option<ArenaNode<'a>> {
        let node = self.as_node();
        node.as_deref()?.block()?.first().copied()
    }

    /// The last entry of a block node.
    ///
    /// The counterpart of [`block_first`](NodeBlockExt::block_first), and the
    /// same reason to exist.
    fn block_last(&self) -> Option<ArenaNode<'a>> {
        let node = self.as_node();
        node.as_deref()?.block()?.last().copied()
    }

    /// The warp that leaves a block node.
    fn block_warp(&self) -> Option<ArenaNode<'a>> {
        let node = self.as_node();
        match node.as_deref() {
            Some(Node::Block(inner)) => inner.warp,
            _ => None,
        }
    }

    /// The address range of a block node.
    fn block_range(&self) -> Option<(u32, u32)> {
        let node = self.as_node();
        match node.as_deref() {
            Some(Node::Block(inner)) => Some((inner.first_address, inner.last_address)),
            _ => None,
        }
    }

    /// The position of a block in its list.
    ///
    /// A node that is not a block has no position, so the worst one is reported
    /// rather than the first: a stray comparison must not read as "the block at
    /// the front". Three copies of this used to disagree about the fallback,
    /// one answering `0` and two `u32::MAX`.
    fn block_index(&self) -> u32 {
        let node = self.as_node();
        match node.as_deref() {
            Some(Node::Block(inner)) => inner.index,
            _ => u32::MAX,
        }
    }

    /// How many warps lead to a block.
    fn block_warpins(&self) -> u32 {
        let node = self.as_node();
        match node.as_deref() {
            Some(Node::Block(inner)) => inner.warpins_count,
            _ => 0,
        }
    }

    /// Whether a block is the marker the compiler left for a loop body.
    fn block_is_loop(&self) -> bool {
        let node = self.as_node();
        matches!(node.as_deref(), Some(Node::Block(inner)) if inner.is_loop)
    }
}

impl<'a, T: AsNode<'a>> NodeBlockExt<'a> for T {}

// MARK: list

impl<'a> Node<'a> {
    /// The contents of a list node.
    pub fn list(&self) -> Option<&ArenaVec<'a, ArenaNode<'a>>> {
        match self {
            Node::Statements(items)
            | Node::Expressions(items)
            | Node::Variables(items)
            | Node::Identifiers(items)
            | Node::Records(items) => Some(items),
            _ => None,
        }
    }

    /// The contents of a list node, for mutation.
    pub fn list_mut(&mut self) -> Option<&mut ArenaVec<'a, ArenaNode<'a>>> {
        match self {
            Node::Statements(items)
            | Node::Expressions(items)
            | Node::Variables(items)
            | Node::Identifiers(items)
            | Node::Records(items) => Some(items),
            _ => None,
        }
    }

    /// Whether `items` contains `node`, comparing identity.
    pub fn list_contains(&self, node: ArenaNode<'a>) -> bool {
        self.list()
            .is_some_and(|items| items.iter().any(|item| (*item).same_as(node)))
    }

    /// The position of `node` in `items`, comparing identity.
    pub fn list_position(&self, node: ArenaNode<'a>) -> Option<usize> {
        self.list()
            .and_then(|items| items.iter().position(|item| (*item).same_as(node)))
    }

    /// Replaces the contents of a list node.
    ///
    /// This is an inherent method rather than an extension method because it
    /// writes: a value that merely holds a node hands it back behind a shared
    /// borrow, so a caller that wants to change one has to borrow it mutably
    /// first. A node that is not a list is left alone.
    pub fn list_set_contents(
        &mut self,
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = ArenaNode<'a>>,
    ) {
        if let Some(items) = self.list_mut() {
            *items = ArenaVec::from_iter_in(contents, &alloc);
        }
    }

    /// Appends to a list node.
    ///
    /// The list already knows the arena it was allocated in, so no allocator is
    /// needed here.
    pub fn list_push(&mut self, child: ArenaNode<'a>) {
        if let Some(items) = self.list_mut() {
            items.push(child);
        }
    }
}

/// The contents of a list node, and its first and last entry, asked of anything
/// that may hold a node.
pub trait NodeListExt<'a>: AsNode<'a> {
    /// The contents of a list node.
    ///
    /// The copy is a plain `Vec`: it is a snapshot the caller works on, not part
    /// of the tree.
    fn list_contents(&self) -> Vec<ArenaNode<'a>> {
        let node = self.as_node();
        match node.as_deref().and_then(Node::list) {
            Some(items) => items.iter().copied().collect(),
            None => Vec::new(),
        }
    }

    /// The first entry of a list node.
    ///
    /// This is [`list_contents`](NodeListExt::list_contents) narrowed to its
    /// first entry: reaching for that entry through the copy would build the
    /// whole list first, which is wasted work for a question about one node. A
    /// node that is not a list, or a list that is empty, has no first entry.
    fn list_first(&self) -> Option<ArenaNode<'a>> {
        let node = self.as_node();
        node.as_deref()?.list()?.first().copied()
    }

    /// The last entry of a list node.
    ///
    /// The counterpart of [`list_first`](NodeListExt::list_first), with the same
    /// reason for existing: the whole list has no business being built to reach
    /// its last entry. A node that is not a list, or a list that is empty, has
    /// no last entry.
    fn list_last(&self) -> Option<ArenaNode<'a>> {
        let node = self.as_node();
        node.as_deref()?.list()?.last().copied()
    }
}

impl<'a, T: AsNode<'a>> NodeListExt<'a> for T {}
