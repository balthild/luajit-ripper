//! Walking and rewriting the AST.
//!
//! The control flow graph is stored alongside the statements: a warp may point
//! at a block that is *not* one of its children. Traversal therefore
//! deliberately does not follow warp targets, exactly like ljd does; passes
//! that need the graph use [`block_targets`] instead.

use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use super::nodes::*;
pub use super::nodes::{list_contents, set_list_contents};

// MARK: children

/// The direct children of a node, in the order ljd visits them.
///
/// Warp targets are not included: they are part of the graph, not of the tree.
///
/// The result is a plain vector: it is the list of children to walk, not part
/// of the tree, and every entry is a cheap copy of a [`NodeRef`].
pub fn children<'a>(node: &Node<'a>) -> Vec<NodeRef<'a>> {
    match node {
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
        Node::IteratorFor(inner) => vec![inner.expressions, inner.identifiers, inner.statements],
        Node::FunctionDefinition(inner) => vec![inner.arguments, inner.statements],
        Node::TableElement(inner) => vec![inner.key, inner.table],
        Node::TableConstructor(inner) => vec![inner.array, inner.records],
        Node::BinaryOperator(inner) => vec![inner.left, inner.right],
        Node::UnaryOperator(inner) => vec![inner.operand],
        Node::ArrayRecord(inner) => vec![inner.value],
        Node::TableRecord(inner) => vec![inner.key, inner.value],
        Node::Block(inner) => {
            let mut out: Vec<NodeRef<'a>> = inner.contents.iter().copied().collect();
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

// MARK: identity

/// The address of a node, which is what identity means in this tree.
///
/// The arena never moves what it has handed out, so the address of a node is a
/// stable name for it, just like the pointer `Rc` used to expose.
pub fn node_key(node: NodeRef<'_>) -> usize {
    std::ptr::from_ref(node).cast::<()>() as usize
}

/// Whether two node references point at the same node.
pub fn same_node(a: NodeRef<'_>, b: NodeRef<'_>) -> bool {
    node_key(a) == node_key(b)
}

// MARK: walking with visitors

/// Every node reachable from `root`, in depth first order.
///
/// Nodes that are reachable through more than one path are reported once.
pub fn walk<'a>(root: NodeRef<'a>) -> Vec<NodeRef<'a>> {
    walk_from(root, &mut HashSet::new())
}

/// [`walk`] with a set of nodes that are already known to have been visited.
pub fn walk_from<'a>(root: NodeRef<'a>, seen: &mut HashSet<usize>) -> Vec<NodeRef<'a>> {
    let mut result = Vec::new();
    let mut stack = vec![root];

    while let Some(current) = stack.pop() {
        if !seen.insert(node_key(current)) {
            continue;
        }
        result.push(current);
        let kids = children(&current.borrow());
        stack.extend(kids.into_iter().rev());
    }

    result
}

/// A hook based tree walk, mirroring ljd's visitor protocol.
///
/// [`visit`](Visitor::visit) is called before a node's children,
/// [`leave`](Visitor::leave) after them. Passes that only care about a few node
/// kinds match on the node inside the hook.
///
/// The trait carries the arena lifetime, so a visitor can store the nodes it is
/// handed instead of copying them out. The hooks take the node by value: a
/// [`NodeRef`] is a copyable reference, so a pass that wants to keep it just
/// stores it.
pub trait Visitor<'a> {
    /// Called when `node` is entered.
    ///
    /// Returning `false` skips the node entirely: its children are not visited
    /// and [`leave`](Visitor::leave) is not called for it.
    fn visit(&mut self, node: NodeRef<'a>) -> bool {
        let _ = node;
        true
    }

    /// Called after all children of `node` have been visited.
    fn leave(&mut self, node: NodeRef<'a>) {
        let _ = node;
    }
}

/// Walks `root` depth first, notifying `visitor` on entry and exit.
///
/// A node that is reachable twice is only visited once, which mirrors the loop
/// protection ljd puts into table constructors; the rest of the tree is a tree
/// in practice.
pub fn traverse<'a>(visitor: &mut impl Visitor<'a>, root: NodeRef<'a>) {
    let mut seen = HashSet::new();
    walk_into(visitor, root, &mut seen);
}

/// Walks a subtree of a traversal that is already running.
///
/// Passes that need to reorder the walk — the slot collector visits the
/// expressions of an assignment before registering its destinations — use this
/// to run a nested walk with the same visitor state.
pub fn visit_subtree<'a>(visitor: &mut impl Visitor<'a>, node: NodeRef<'a>) {
    let mut seen = HashSet::new();
    walk_into(visitor, node, &mut seen);
}

fn walk_into<'a>(visitor: &mut impl Visitor<'a>, node: NodeRef<'a>, seen: &mut HashSet<usize>) {
    if !seen.insert(node_key(node)) {
        return;
    }
    if !visitor.visit(node) {
        return;
    }

    // The borrow has to end before the children are walked: a visitor is
    // allowed to change the list of the node it is looking at.
    let children = children(&node.borrow());
    for child in children {
        walk_into(visitor, child, seen);
    }
    visitor.leave(node);
}

// MARK: copying

/// Copies a node and everything below it into `alloc`.
///
/// ljd uses `copy.deepcopy` when it has to move a subtree into a second place;
/// sharing the node between the two places instead would make later changes
/// visible in both.
pub fn deep_clone<'a>(alloc: &'a Allocator, root: NodeRef<'a>) -> NodeRef<'a> {
    let mut copies = HashMap::new();
    deep_clone_into(alloc, root, &mut copies)
}

/// Copies a list of nodes, giving two occurrences of the same node one copy.
///
/// This is what keeps a copy of a graph with shared children a graph with
/// shared children.
pub fn deep_clone_list<'a>(
    alloc: &'a Allocator,
    nodes: impl IntoIterator<Item = NodeRef<'a>>,
    copies: &mut HashMap<usize, NodeRef<'a>>,
) -> Vec<NodeRef<'a>> {
    nodes
        .into_iter()
        .map(|node| deep_clone_into(alloc, node, copies))
        .collect()
}

fn deep_clone_into<'a>(
    alloc: &'a Allocator,
    source: NodeRef<'a>,
    copies: &mut HashMap<usize, NodeRef<'a>>,
) -> NodeRef<'a> {
    if let Some(copy) = copies.get(&node_key(source)) {
        return copy;
    }

    let shallow = clone_shallow(alloc, &source.borrow(), copies);
    let copy = node(alloc, shallow);
    copies.insert(node_key(source), copy);

    // Rewriting the children of the copy is what makes this a deep copy. Every
    // occurrence is replaced, so that a child used twice stays shared instead
    // of half of it pointing into the original graph.
    let kids = {
        let borrowed = copy.borrow();
        children(&borrowed)
    };
    for child in kids {
        let cloned = deep_clone_into(alloc, child, copies);
        if same_node(cloned, child) {
            continue;
        }
        while replace_child(copy, child, cloned) {}
    }

    copy
}

/// Copies a node one level deep: the children are still the originals, which
/// [`deep_clone_into`] replaces with copies afterwards.
fn clone_shallow<'a>(
    alloc: &'a Allocator,
    source: &Node<'a>,
    copies: &mut HashMap<usize, NodeRef<'a>>,
) -> Node<'a> {
    let mut list = |nodes: &ArenaVec<'a, NodeRef<'a>>| -> Vec<NodeRef<'a>> {
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

/// Every function definition in the tree, including the root.
pub fn functions<'a>(root: NodeRef<'a>) -> Vec<NodeRef<'a>> {
    walk(root)
        .into_iter()
        .filter(|node| matches!(&*node.borrow(), Node::FunctionDefinition(_)))
        .collect()
}

/// Every non empty statement list in the tree.
///
/// ljd ignores empty lists, because passes cannot do anything useful with them.
pub fn statement_lists<'a>(root: NodeRef<'a>) -> Vec<NodeRef<'a>> {
    walk(root)
        .into_iter()
        .filter(|node| matches!(&*node.borrow(), Node::Statements(items) if !items.is_empty()))
        .collect()
}

/// Every non empty statement list that belongs to `function` itself.
///
/// The lists inside a nested function definition belong to that function, so
/// they are left out. That is what makes it possible to run a pass over one
/// function at a time.
pub fn own_statement_lists<'a>(function: NodeRef<'a>) -> Vec<NodeRef<'a>> {
    let mut result = Vec::new();
    collect_own_statement_lists(function, true, &mut result);
    result
}

fn collect_own_statement_lists<'a>(
    node: NodeRef<'a>,
    is_root: bool,
    result: &mut Vec<NodeRef<'a>>,
) {
    let is_function = matches!(&*node.borrow(), Node::FunctionDefinition(_));
    if !is_root && is_function {
        return;
    }

    if matches!(&*node.borrow(), Node::Statements(items) if !items.is_empty()) {
        result.push(node);
    }

    for child in children(&node.borrow()) {
        collect_own_statement_lists(child, false, result);
    }
}

// MARK: replacing

/// Replaces the child `old` of `parent` with `new`.
///
/// Returns whether `old` was found.
pub fn replace_child<'a>(parent: NodeRef<'a>, old: NodeRef<'a>, new: NodeRef<'a>) -> bool {
    let mut borrowed = parent.borrow_mut();
    match &mut *borrowed {
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
                .any(|branch| replace_child(branch, old, new))
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
            replace_slot(&mut inner.array, old, new) || replace_slot(&mut inner.records, old, new)
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
            replace_slot(&mut inner.index, old, new) || replace_slot(&mut inner.controls, old, new)
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

fn replace_slot<'a>(slot: &mut NodeRef<'a>, old: NodeRef<'a>, new: NodeRef<'a>) -> bool {
    if same_node(slot, old) {
        *slot = new;
        true
    } else {
        false
    }
}

fn replace_in_list<'a>(
    items: &mut ArenaVec<'a, NodeRef<'a>>,
    old: NodeRef<'a>,
    new: NodeRef<'a>,
) -> bool {
    for item in items.iter_mut() {
        if same_node(item, old) {
            *item = new;
            return true;
        }
    }
    false
}

// MARK: blocks

/// The targets a warp can jump to.
pub fn block_targets<'a>(warp: &Node<'a>) -> Vec<NodeRef<'a>> {
    match warp {
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

/// Whether a warp simply falls through into the next block.
pub fn is_flow(warp: &Node<'_>) -> bool {
    matches!(
        warp,
        Node::UnconditionalWarp(inner) if inner.kind == UnconditionalWarpKind::Flow
    )
}

/// Reads the contents of a block node.
pub fn block_contents<'a>(block: NodeRef<'a>) -> Vec<NodeRef<'a>> {
    match &*block.borrow() {
        Node::Block(inner) => inner.contents.iter().copied().collect(),
        _ => Vec::new(),
    }
}

/// Replaces the contents of a block node.
pub fn set_block_contents<'a>(
    alloc: &'a Allocator,
    block: NodeRef<'a>,
    contents: impl IntoIterator<Item = NodeRef<'a>>,
) {
    if let Node::Block(inner) = &mut *block.borrow_mut() {
        inner.contents = ArenaVec::from_iter_in(contents, &alloc);
    }
}

/// Reads the warp of a block node.
pub fn block_warp<'a>(block: NodeRef<'a>) -> Option<NodeRef<'a>> {
    match &*block.borrow() {
        Node::Block(inner) => inner.warp,
        _ => None,
    }
}

/// Replaces the warp of a block node.
pub fn set_block_warp<'a>(block: NodeRef<'a>, warp: NodeRef<'a>) {
    if let Node::Block(inner) = &mut *block.borrow_mut() {
        inner.warp = Some(warp);
    }
}

/// Reads the address range of a block node.
pub fn block_range(block: NodeRef<'_>) -> Option<(u32, u32)> {
    match &*block.borrow() {
        Node::Block(inner) => Some((inner.first_address, inner.last_address)),
        _ => None,
    }
}

/// The block that `warp` targets when it is an unconditional jump or flow.
pub fn jump_target<'a>(warp: &Node<'a>) -> Option<NodeRef<'a>> {
    match warp {
        Node::UnconditionalWarp(inner) => inner.target,
        _ => None,
    }
}

// MARK: list lookups

/// Whether `items` contains `node`, comparing identity.
pub fn contains<'a>(items: &[NodeRef<'a>], node: NodeRef<'a>) -> bool {
    items.iter().any(|item| same_node(item, node))
}

/// The position of `node` in `items`, comparing identity.
pub fn position<'a>(items: &[NodeRef<'a>], node: NodeRef<'a>) -> Option<usize> {
    items.iter().position(|item| same_node(item, node))
}
