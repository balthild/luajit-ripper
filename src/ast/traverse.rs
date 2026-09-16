//! Walking and rewriting the AST.
//!
//! The control flow graph is stored alongside the statements: a warp may point
//! at a block that is *not* one of its children. Traversal therefore
//! deliberately does not follow warp targets, exactly like ljd does; passes
//! that need the graph use [`block_targets`] instead.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::nodes::*;

pub use super::nodes::{list_contents, set_list_contents};

/// The direct children of a node, in the order ljd visits them.
///
/// Warp targets are not included: they are part of the graph, not of the tree.
pub fn children(node: &Node) -> Vec<NodeRef> {
    match node {
        Node::Statements(items)
        | Node::Expressions(items)
        | Node::Variables(items)
        | Node::Identifiers(items)
        | Node::Records(items) => items.clone(),
        Node::Assignment(inner) => vec![inner.expressions.clone(), inner.destinations.clone()],
        Node::FunctionCall(inner) => vec![inner.arguments.clone(), inner.function.clone()],
        Node::Return(inner) => vec![inner.returns.clone()],
        Node::If(inner) => {
            let mut out = vec![inner.expression.clone(), inner.then_block.clone()];
            out.extend(inner.elseifs.iter().cloned());
            out.push(inner.else_block.clone());
            out
        }
        Node::ElseIf(inner) => vec![inner.expression.clone(), inner.then_block.clone()],
        Node::While(inner) => vec![inner.expression.clone(), inner.statements.clone()],
        Node::RepeatUntil(inner) => vec![inner.statements.clone(), inner.expression.clone()],
        Node::NumericFor(inner) => vec![
            inner.variable.clone(),
            inner.expressions.clone(),
            inner.statements.clone(),
        ],
        Node::IteratorFor(inner) => vec![
            inner.expressions.clone(),
            inner.identifiers.clone(),
            inner.statements.clone(),
        ],
        Node::FunctionDefinition(inner) => {
            vec![inner.arguments.clone(), inner.statements.clone()]
        }
        Node::TableElement(inner) => vec![inner.key.clone(), inner.table.clone()],
        Node::TableConstructor(inner) => vec![inner.array.clone(), inner.records.clone()],
        Node::BinaryOperator(inner) => vec![inner.left.clone(), inner.right.clone()],
        Node::UnaryOperator(inner) => vec![inner.operand.clone()],
        Node::ArrayRecord(inner) => vec![inner.value.clone()],
        Node::TableRecord(inner) => vec![inner.key.clone(), inner.value.clone()],
        Node::Block(inner) => {
            let mut out = inner.contents.clone();
            if let Some(warp) = &inner.warp {
                out.push(warp.clone());
            }
            out
        }
        Node::ConditionalWarp(inner) => inner.condition.iter().cloned().collect(),
        Node::IteratorWarp(inner) => vec![inner.variables.clone(), inner.controls.clone()],
        Node::NumericLoopWarp(inner) => vec![inner.index.clone(), inner.controls.clone()],
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

/// A hook based tree walk, mirroring ljd's visitor protocol.
///
/// [`visit`](Visitor::visit) is called before a node's children,
/// [`leave`](Visitor::leave) after them. Passes that only care about a few node
/// kinds match on the node inside the hook.
pub trait Visitor {
    /// Called when `node` is entered.
    ///
    /// Returning `false` skips the node entirely: its children are not visited
    /// and [`leave`](Visitor::leave) is not called for it.
    fn visit(&mut self, node: &NodeRef) -> bool {
        let _ = node;
        true
    }

    /// Called after all children of `node` have been visited.
    fn leave(&mut self, node: &NodeRef) {
        let _ = node;
    }
}

/// Walks `root` depth first, notifying `visitor` on entry and exit.
///
/// A node that is reachable twice is only visited once, which mirrors the loop
/// protection ljd puts into table constructors; the rest of the tree is a tree
/// in practice.
pub fn traverse(visitor: &mut impl Visitor, root: &NodeRef) {
    let mut seen = HashSet::new();
    walk_into(visitor, root, &mut seen);
}

/// Walks a subtree of a traversal that is already running.
///
/// Passes that need to reorder the walk — the slot collector visits the
/// expressions of an assignment before registering its destinations — use this
/// to run a nested walk with the same visitor state.
pub fn visit_subtree(visitor: &mut impl Visitor, node: &NodeRef) {
    let mut seen = HashSet::new();
    walk_into(visitor, node, &mut seen);
}

fn walk_into(visitor: &mut impl Visitor, node: &NodeRef, seen: &mut HashSet<usize>) {
    if !seen.insert(Rc::as_ptr(node) as usize) {
        return;
    }
    if !visitor.visit(node) {
        return;
    }

    // The borrow has to end before the children are walked: a visitor is
    // allowed to change the list of the node it is looking at.
    let children = children(&node.borrow());
    for child in children {
        walk_into(visitor, &child, seen);
    }
    visitor.leave(node);
}

/// Copies a node and everything below it.
///
/// ljd uses `copy.deepcopy` when it has to move a subtree into a second place;
/// cloning a [`NodeRef`] would instead share the node between the two, which
/// would make later changes visible in both.
pub fn deep_clone(root: &NodeRef) -> NodeRef {
    let mut copies = HashMap::new();
    deep_clone_into(root, &mut copies)
}

fn deep_clone_into(source: &NodeRef, copies: &mut HashMap<usize, NodeRef>) -> NodeRef {
    if let Some(copy) = copies.get(&(Rc::as_ptr(source) as usize)) {
        return copy.clone();
    }

    let shallow = match &*source.borrow() {
        Node::Statements(items) => Node::Statements(items.clone()),
        Node::Expressions(items) => Node::Expressions(items.clone()),
        Node::Variables(items) => Node::Variables(items.clone()),
        Node::Identifiers(items) => Node::Identifiers(items.clone()),
        Node::Records(items) => Node::Records(items.clone()),
        Node::Assignment(inner) => Node::Assignment(Box::new(Assignment {
            expressions: inner.expressions.clone(),
            destinations: inner.destinations.clone(),
            kind: inner.kind,
            meta: inner.meta,
        })),
        Node::FunctionCall(inner) => Node::FunctionCall(Box::new(FunctionCall {
            function: inner.function.clone(),
            arguments: inner.arguments.clone(),
            is_method: inner.is_method,
            meta: inner.meta,
        })),
        Node::Return(inner) => Node::Return(Box::new(Return {
            returns: inner.returns.clone(),
            meta: inner.meta,
        })),
        Node::If(inner) => Node::If(Box::new(If {
            expression: inner.expression.clone(),
            then_block: inner.then_block.clone(),
            elseifs: inner.elseifs.clone(),
            else_block: inner.else_block.clone(),
            meta: inner.meta,
        })),
        Node::ElseIf(inner) => Node::ElseIf(Box::new(ElseIf {
            expression: inner.expression.clone(),
            then_block: inner.then_block.clone(),
            meta: inner.meta,
        })),
        Node::While(inner) => Node::While(Box::new(While {
            expression: inner.expression.clone(),
            statements: inner.statements.clone(),
            meta: inner.meta,
        })),
        Node::RepeatUntil(inner) => Node::RepeatUntil(Box::new(RepeatUntil {
            expression: inner.expression.clone(),
            statements: inner.statements.clone(),
            meta: inner.meta,
        })),
        Node::NumericFor(inner) => Node::NumericFor(Box::new(NumericFor {
            variable: inner.variable.clone(),
            expressions: inner.expressions.clone(),
            statements: inner.statements.clone(),
            meta: inner.meta,
        })),
        Node::IteratorFor(inner) => Node::IteratorFor(Box::new(IteratorFor {
            identifiers: inner.identifiers.clone(),
            expressions: inner.expressions.clone(),
            statements: inner.statements.clone(),
            meta: inner.meta,
        })),
        Node::FunctionDefinition(inner) => Node::FunctionDefinition(Box::new(FunctionDefinition {
            arguments: inner.arguments.clone(),
            statements: inner.statements.clone(),
            upvalues: inner.upvalues.clone(),
            debug: inner.debug.clone(),
            instruction_count: inner.instruction_count,
            meta: inner.meta,
        })),
        Node::Identifier(inner) => Node::Identifier(Box::new(Identifier {
            kind: inner.kind,
            name: inner.name.clone(),
            slot: inner.slot,
            id: inner.id,
            possible_ids: inner.possible_ids.clone(),
            local_end: inner.local_end,
            meta: inner.meta,
        })),
        Node::TableElement(inner) => Node::TableElement(Box::new(TableElement {
            table: inner.table.clone(),
            key: inner.key.clone(),
            meta: inner.meta,
        })),
        Node::Constant(inner) => Node::Constant(Box::new(Constant {
            value: inner.value.clone(),
            meta: inner.meta,
        })),
        Node::Primitive(inner) => Node::Primitive(*inner),
        Node::TableConstructor(inner) => Node::TableConstructor(Box::new(TableConstructor {
            array: inner.array.clone(),
            records: inner.records.clone(),
            meta: inner.meta,
        })),
        Node::BinaryOperator(inner) => Node::BinaryOperator(Box::new(BinaryOperator {
            kind: inner.kind,
            left: inner.left.clone(),
            right: inner.right.clone(),
            meta: inner.meta,
        })),
        Node::UnaryOperator(inner) => Node::UnaryOperator(Box::new(UnaryOperator {
            kind: inner.kind,
            operand: inner.operand.clone(),
            meta: inner.meta,
        })),
        Node::ArrayRecord(inner) => Node::ArrayRecord(Box::new(ArrayRecord {
            value: inner.value.clone(),
            meta: inner.meta,
        })),
        Node::TableRecord(inner) => Node::TableRecord(Box::new(TableRecord {
            key: inner.key.clone(),
            value: inner.value.clone(),
            meta: inner.meta,
        })),
        Node::Block(inner) => Node::Block(Box::new(Block {
            index: inner.index,
            first_address: inner.first_address,
            last_address: inner.last_address,
            last_body_address: inner.last_body_address,
            warpins_count: inner.warpins_count,
            is_loop: inner.is_loop,
            contents: inner.contents.clone(),
            warp: inner.warp.clone(),
        })),
        Node::UnconditionalWarp(inner) => Node::UnconditionalWarp(Box::new(UnconditionalWarp {
            kind: inner.kind,
            target: inner.target.clone(),
            is_uclo: inner.is_uclo,
            meta: inner.meta,
        })),
        Node::ConditionalWarp(inner) => Node::ConditionalWarp(Box::new(ConditionalWarp {
            condition: inner.condition.clone(),
            true_target: inner.true_target.clone(),
            false_target: inner.false_target.clone(),
            slot: inner.slot,
            meta: inner.meta,
        })),
        Node::IteratorWarp(inner) => Node::IteratorWarp(Box::new(IteratorWarp {
            variables: inner.variables.clone(),
            controls: inner.controls.clone(),
            body: inner.body.clone(),
            way_out: inner.way_out.clone(),
            meta: inner.meta,
        })),
        Node::NumericLoopWarp(inner) => Node::NumericLoopWarp(Box::new(NumericLoopWarp {
            index: inner.index.clone(),
            controls: inner.controls.clone(),
            body: inner.body.clone(),
            way_out: inner.way_out.clone(),
            meta: inner.meta,
        })),
        Node::NoOp(inner) => Node::NoOp(Box::new(NoOp { meta: inner.meta })),
        Node::EndWarp(inner) => Node::EndWarp(Box::new(EndWarp {
            target: inner.target.clone(),
            meta: inner.meta,
        })),
        Node::Break => Node::Break,
        Node::Vararg => Node::Vararg,
        Node::MulTres => Node::MulTres,
    };

    let copy = node(shallow);
    copies.insert(Rc::as_ptr(source) as usize, copy.clone());

    // Rewriting the children of the copy is what makes this a deep copy. Every
    // occurrence is replaced, so that a child used twice stays shared instead
    // of half of it pointing into the original graph.
    let kids = {
        let borrowed = copy.borrow();
        children(&borrowed)
    };
    for child in kids {
        let cloned = deep_clone_into(&child, copies);
        if Rc::ptr_eq(&cloned, &child) {
            continue;
        }
        while replace_child(&copy, &child, cloned.clone()) {}
    }

    copy
}

/// Every node reachable from `root`, in depth first order.
///
/// Nodes that are reachable through more than one path are reported once.
pub fn walk(root: &NodeRef) -> Vec<NodeRef> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut stack = vec![root.clone()];

    while let Some(current) = stack.pop() {
        if !seen.insert(Rc::as_ptr(&current) as usize) {
            continue;
        }
        result.push(current.clone());
        let kids = children(&current.borrow());
        stack.extend(kids.into_iter().rev());
    }

    result
}

/// Every function definition in the tree, including the root.
pub fn functions(root: &NodeRef) -> Vec<NodeRef> {
    walk(root)
        .into_iter()
        .filter(|node| matches!(&*node.borrow(), Node::FunctionDefinition(_)))
        .collect()
}

/// Every non empty statement list in the tree.
///
/// ljd ignores empty lists, because passes cannot do anything useful with them.
pub fn statement_lists(root: &NodeRef) -> Vec<NodeRef> {
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
pub fn own_statement_lists(function: &NodeRef) -> Vec<NodeRef> {
    let mut result = Vec::new();
    collect_own_statement_lists(function, true, &mut result);
    result
}

fn collect_own_statement_lists(node: &NodeRef, is_root: bool, result: &mut Vec<NodeRef>) {
    let is_function = matches!(&*node.borrow(), Node::FunctionDefinition(_));
    if !is_root && is_function {
        return;
    }

    if matches!(&*node.borrow(), Node::Statements(items) if !items.is_empty()) {
        result.push(node.clone());
    }

    for child in children(&node.borrow()) {
        collect_own_statement_lists(&child, false, result);
    }
}

/// Replaces the child `old` of `parent` with `new`.
///
/// Returns whether `old` was found.
pub fn replace_child(parent: &NodeRef, old: &NodeRef, new: NodeRef) -> bool {
    let mut borrowed = parent.borrow_mut();
    match &mut *borrowed {
        Node::Statements(items)
        | Node::Expressions(items)
        | Node::Variables(items)
        | Node::Identifiers(items)
        | Node::Records(items) => replace_in_list(items, old, new),
        Node::Assignment(inner) => {
            replace_slot(&mut inner.expressions, old, new.clone())
                || replace_slot(&mut inner.destinations, old, new)
        }
        Node::FunctionCall(inner) => {
            replace_slot(&mut inner.arguments, old, new.clone())
                || replace_slot(&mut inner.function, old, new)
        }
        Node::Return(inner) => replace_slot(&mut inner.returns, old, new),
        Node::If(inner) => {
            if replace_slot(&mut inner.expression, old, new.clone())
                || replace_slot(&mut inner.then_block, old, new.clone())
                || replace_slot(&mut inner.else_block, old, new.clone())
            {
                return true;
            }
            inner
                .elseifs
                .iter()
                .any(|branch| replace_child(branch, old, new.clone()))
        }
        Node::ElseIf(inner) => {
            replace_slot(&mut inner.expression, old, new.clone())
                || replace_slot(&mut inner.then_block, old, new)
        }
        Node::While(inner) => {
            replace_slot(&mut inner.expression, old, new.clone())
                || replace_slot(&mut inner.statements, old, new)
        }
        Node::RepeatUntil(inner) => {
            replace_slot(&mut inner.expression, old, new.clone())
                || replace_slot(&mut inner.statements, old, new)
        }
        Node::NumericFor(inner) => {
            replace_slot(&mut inner.variable, old, new.clone())
                || replace_slot(&mut inner.expressions, old, new.clone())
                || replace_slot(&mut inner.statements, old, new)
        }
        Node::IteratorFor(inner) => {
            replace_slot(&mut inner.expressions, old, new.clone())
                || replace_slot(&mut inner.identifiers, old, new.clone())
                || replace_slot(&mut inner.statements, old, new)
        }
        Node::FunctionDefinition(inner) => {
            replace_slot(&mut inner.arguments, old, new.clone())
                || replace_slot(&mut inner.statements, old, new)
        }
        Node::TableElement(inner) => {
            replace_slot(&mut inner.key, old, new.clone())
                || replace_slot(&mut inner.table, old, new)
        }
        Node::TableConstructor(inner) => {
            replace_slot(&mut inner.array, old, new.clone())
                || replace_slot(&mut inner.records, old, new)
        }
        Node::BinaryOperator(inner) => {
            replace_slot(&mut inner.left, old, new.clone())
                || replace_slot(&mut inner.right, old, new)
        }
        Node::UnaryOperator(inner) => replace_slot(&mut inner.operand, old, new),
        Node::ArrayRecord(inner) => replace_slot(&mut inner.value, old, new),
        Node::TableRecord(inner) => {
            replace_slot(&mut inner.key, old, new.clone())
                || replace_slot(&mut inner.value, old, new)
        }
        Node::Block(inner) => {
            if replace_in_list(&mut inner.contents, old, new.clone()) {
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
            replace_slot(&mut inner.variables, old, new.clone())
                || replace_slot(&mut inner.controls, old, new)
        }
        Node::NumericLoopWarp(inner) => {
            replace_slot(&mut inner.index, old, new.clone())
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

fn replace_slot(slot: &mut NodeRef, old: &NodeRef, new: NodeRef) -> bool {
    if Rc::ptr_eq(slot, old) {
        *slot = new;
        true
    } else {
        false
    }
}

fn replace_in_list(items: &mut [NodeRef], old: &NodeRef, new: NodeRef) -> bool {
    for item in items.iter_mut() {
        if Rc::ptr_eq(item, old) {
            *item = new;
            return true;
        }
    }
    false
}

/// The targets a warp can jump to.
pub fn block_targets(warp: &Node) -> Vec<NodeRef> {
    match warp {
        Node::UnconditionalWarp(inner) => inner.target.iter().cloned().collect(),
        Node::ConditionalWarp(inner) => {
            let mut out = Vec::new();
            if let Some(target) = &inner.false_target {
                out.push(target.clone());
            }
            if let Some(target) = &inner.true_target {
                out.push(target.clone());
            }
            out
        }
        Node::IteratorWarp(inner) => {
            let mut out = Vec::new();
            if let Some(target) = &inner.way_out {
                out.push(target.clone());
            }
            if let Some(target) = &inner.body {
                out.push(target.clone());
            }
            out
        }
        Node::NumericLoopWarp(inner) => {
            let mut out = Vec::new();
            if let Some(target) = &inner.way_out {
                out.push(target.clone());
            }
            if let Some(target) = &inner.body {
                out.push(target.clone());
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Whether a warp simply falls through into the next block.
pub fn is_flow(warp: &Node) -> bool {
    matches!(
        warp,
        Node::UnconditionalWarp(inner) if inner.kind == UnconditionalWarpKind::Flow
    )
}

/// Reads the contents of a block node.
pub fn block_contents(block: &NodeRef) -> Vec<NodeRef> {
    match &*block.borrow() {
        Node::Block(inner) => inner.contents.clone(),
        _ => Vec::new(),
    }
}

/// Replaces the contents of a block node.
pub fn set_block_contents(block: &NodeRef, contents: Vec<NodeRef>) {
    if let Node::Block(inner) = &mut *block.borrow_mut() {
        inner.contents = contents;
    }
}

/// Reads the warp of a block node.
pub fn block_warp(block: &NodeRef) -> Option<NodeRef> {
    match &*block.borrow() {
        Node::Block(inner) => inner.warp.clone(),
        _ => None,
    }
}

/// Replaces the warp of a block node.
pub fn set_block_warp(block: &NodeRef, warp: NodeRef) {
    if let Node::Block(inner) = &mut *block.borrow_mut() {
        inner.warp = Some(warp);
    }
}

/// Reads the address range of a block node.
pub fn block_range(block: &NodeRef) -> Option<(u32, u32)> {
    match &*block.borrow() {
        Node::Block(inner) => Some((inner.first_address, inner.last_address)),
        _ => None,
    }
}

/// The block that `warp` targets when it is an unconditional jump or flow.
pub fn jump_target(warp: &Node) -> Option<NodeRef> {
    match warp {
        Node::UnconditionalWarp(inner) => inner.target.clone(),
        _ => None,
    }
}

/// Whether `items` contains `node`, comparing identity.
pub fn contains(items: &[NodeRef], node: &NodeRef) -> bool {
    items.iter().any(|item| Rc::ptr_eq(item, node))
}

/// The position of `node` in `items`, comparing identity.
pub fn position(items: &[NodeRef], node: &NodeRef) -> Option<usize> {
    items.iter().position(|item| Rc::ptr_eq(item, node))
}
