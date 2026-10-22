//! Reading a node from the outside.
//!
//! The kind of a node, its metadata, and [`NodeAccessorExt`], the trait that asks
//! what a node is. It is written over [`AsNode`], which a [`Node`], a [`ArenaNode`]
//! and an `Option` of either all answer, so that a caller can write
//! `node.is_block()` whichever of the four it happens to hold, without a
//! `map`/`is_some_and` chain around it.

use oxc_allocator::{Allocator, ArenaVec};

use crate::ast::ext::*;
use crate::ast::nodes::*;

// MARK: identifier

impl<'a> Node<'a> {
    /// The identifier payload, if this is an identifier.
    pub fn identifier(&self) -> Option<&Identifier<'a>> {
        match self {
            Node::Identifier(identifier) => Some(identifier),
            _ => None,
        }
    }

    /// Records the id of the assignment an identifier came from.
    ///
    /// A node that is not an identifier is left alone.
    pub fn identifier_set_id(&mut self, id: u32) {
        if let Node::Identifier(identifier) = self {
            identifier.id = Some(id);
        }
    }

    /// Replaces the ids an identifier may refer to.
    ///
    /// A node that is not an identifier is left alone.
    pub fn identifier_set_possible_ids(
        &mut self,
        alloc: &'a Allocator,
        ids: impl IntoIterator<Item = u32>,
    ) {
        if let Node::Identifier(identifier) = self {
            identifier.possible_ids = ArenaVec::from_iter_in(ids, &alloc);
        }
    }

    /// Drops one of the ids an identifier may refer to.
    ///
    /// A node that is not an identifier is left alone.
    pub fn identifier_forget_possible_id(&mut self, id: u32) {
        if let Node::Identifier(identifier) = self {
            identifier.possible_ids.retain(|candidate| *candidate != id);
        }
    }
}

// MARK: recovery markers

impl<'a> ArenaNode<'a> {
    /// Records that a pass gave up on this node.
    pub fn mark_error(self) {
        if let Some(meta) = self.borrow_mut().meta_mut() {
            meta.error_here = true;
        }
    }

    /// Records that this node's function could not be decompiled.
    pub fn mark_failure(self) {
        if let Some(meta) = self.borrow_mut().meta_mut() {
            meta.failed_here = true;
        }
    }
}

/// Whether `node` carries the mark [`ArenaNode::mark_error`] leaves.
fn node_has_error(node: &Node<'_>) -> bool {
    node.meta().is_some_and(|meta| meta.error_here)
}

/// Whether `node` carries the mark [`ArenaNode::mark_failure`] leaves.
fn node_has_failure(node: &Node<'_>) -> bool {
    node.meta().is_some_and(|meta| meta.failed_here)
}

/// Whether `node` carries either mark.
fn node_has_recovery(node: &Node<'_>) -> bool {
    node_has_error(node) || node_has_failure(node)
}

// MARK: warp targets

impl<'a> Node<'a> {
    /// Points a warp at another block.
    ///
    /// The kind of warp decides which of its targets is rewritten, so this is
    /// the one place that knows a conditional branch is stored by its false
    /// branch. A node that is not a warp is left alone.
    ///
    /// This is an inherent method rather than an extension method because it
    /// writes: a value that merely holds a node hands it back behind a shared
    /// borrow, so a caller that wants to change one has to borrow it mutably
    /// first.
    pub fn set_target(&mut self, target: Option<ArenaNode<'a>>) {
        match self {
            Node::ConditionalWarp(inner) => inner.false_target = target,
            Node::UnconditionalWarp(inner) => inner.target = target,
            Node::EndWarp(inner) => inner.target = target,
            Node::IteratorWarp(inner) => inner.way_out = target,
            Node::NumericLoopWarp(inner) => inner.way_out = target,
            _ => {}
        }
    }
}

/// The block a warp leads to.
///
/// A conditional branch is described by the block its *false* branch takes,
/// because that is the one that jumps in the bytecode; the true branch falls
/// through. An [`EndWarp`](Node::EndWarp) only knows where it would have gone,
/// and is reported only when `allow_end` asks for it. Anything that is not a
/// warp has no target.
fn target_of<'a>(node: &Node<'a>, allow_end: bool) -> Option<ArenaNode<'a>> {
    match node {
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

/// The kind and target of an unconditional warp, if that is what this is.
///
/// The kind tells a jump apart from a fallthrough, which is what decides
/// whether the block can be looked through or has to be kept.
fn jump_of_node<'a>(node: &Node<'a>) -> Option<(UnconditionalWarpKind, Option<ArenaNode<'a>>)> {
    match node {
        Node::UnconditionalWarp(inner) => Some((inner.kind, inner.target)),
        _ => None,
    }
}

/// The two targets of a conditional branch, and the register its condition was
/// computed into.
///
/// The register is `None` for a comparison, which leaves the condition in no
/// register at all; the passes use the distinction to tell a condition that was
/// materialised into a slot from one that was not.
fn conditional_targets_of<'a>(
    node: &Node<'a>,
) -> Option<(Option<ArenaNode<'a>>, Option<ArenaNode<'a>>, Option<u32>)> {
    match node {
        Node::ConditionalWarp(inner) => Some((inner.true_target, inner.false_target, inner.slot)),
        _ => None,
    }
}

/// The loop body and way out of an iterator or numeric loop warp.
///
/// A loop head without a body is not reported at all, so the caller gets the
/// pair it needs or nothing.
fn loop_targets_of<'a>(node: &Node<'a>) -> Option<(ArenaNode<'a>, Option<ArenaNode<'a>>)> {
    match node {
        Node::IteratorWarp(inner) => Some((inner.body?, inner.way_out)),
        Node::NumericLoopWarp(inner) => Some((inner.body?, inner.way_out)),
        _ => None,
    }
}

// MARK: predicates

/// Writes the variant predicates of [`NodeAccessorExt`].
///
/// Each entry names its method and the variant it asks about, and the macro
/// writes the method into the trait. Because the trait is filled in by a blanket
/// implementation, each method body is written once however many types answer
/// the trait.
///
/// A variant with a payload is listed under `payloads` and gets two methods: one
/// that only asks about the kind, and one that hands the payload to a predicate.
/// The second is what keeps a question about the *contents* of a node from
/// having to be written as a `matches!` with a guard of its own.
///
/// A variant with nothing in it is listed under `simple`, as are the variants
/// that are told apart by a payload *field* rather than by their variant, like
/// a string constant or a warp that flows rather than jumps; those get only the
/// kind-only method.
///
/// A predicate handed no node at all is `false`, so a caller never has to put an
/// `is_some()` in front of one.
macro_rules! node_predicates {
    (
        payloads: {
            $(
                $(#[$pdoc:meta])*
                $pname:ident, $pand:ident => $pvariant:ident : $pty:ty
            ),* $(,)?
        },
        simple: {
            $(
                $(#[$sdoc:meta])*
                $sname:ident => $spattern:pat $(if $sguard:expr)?
            ),* $(,)?
        } $(,)?
    ) => {
        /// Whether a node is of a particular kind.
        ///
        /// Each method answers one question about the variant a node holds, and
        /// every one of them is `false` for a node that is not there, so the
        /// `Option` forms can be asked without an `is_some()` first.
        ///
        /// The trait is written over [`AsNode`], so a [`Node`], a `&Node`, a
        /// [`ArenaNode`] and an `Option` of either all answer it. Asking a
        /// [`ArenaNode`] costs a borrow, which is taken and released inside the
        /// method.
        pub trait NodeAccessorExt<'a>: AsNode<'a> {
            $(
                $(#[$sdoc])*
                fn $sname(&self) -> bool {
                    matches!(self.as_node().as_deref(), Some($spattern) $(if $sguard)?)
                }
            )*

            $(
                $(#[$pdoc])*
                fn $pname(&self) -> bool {
                    matches!(self.as_node().as_deref(), Some(Node::$pvariant(_)))
                }

                $(#[$pdoc])*
                /// Whether `predicate` accepts the contents of such a node.
                ///
                /// The counterpart of the kind-only method above, for a question
                /// that has to look inside the node rather than only at which
                /// variant it holds.
                fn $pand(&self, predicate: impl FnOnce(&$pty) -> bool) -> bool {
                    match self.as_node().as_deref() {
                        Some(Node::$pvariant(inner)) => predicate(inner),
                        _ => false,
                    }
                }
            )*

            /// The block a warp leads to.
            ///
            /// A conditional branch is described by the block its *false*
            /// branch takes, because that is the one that jumps in the
            /// bytecode; the true branch falls through. An
            /// [`EndWarp`](Node::EndWarp) only knows where it would have gone,
            /// and is reported only when `allow_end` asks for it. Anything that
            /// is not a warp has no target.
            fn target(&self, allow_end: bool) -> Option<ArenaNode<'a>> {
                target_of(self.as_node().as_deref()?, allow_end)
            }

            /// The kind and target of an unconditional warp, if that is what
            /// this is.
            ///
            /// The kind tells a jump apart from a fallthrough, which is what
            /// decides whether the block can be looked through or has to be
            /// kept.
            fn jump_of(&self) -> Option<(UnconditionalWarpKind, Option<ArenaNode<'a>>)> {
                jump_of_node(self.as_node().as_deref()?)
            }

            /// The two targets of a conditional branch, and the register its
            /// condition was computed into.
            ///
            /// The register is `None` for a comparison, which leaves the
            /// condition in no register at all; the passes use the distinction
            /// to tell a condition that was materialised into a slot from one
            /// that was not.
            fn conditional_targets(
                &self,
            ) -> Option<(Option<ArenaNode<'a>>, Option<ArenaNode<'a>>, Option<u32>)> {
                conditional_targets_of(self.as_node().as_deref()?)
            }

            /// The loop body and way out of an iterator or numeric loop warp.
            ///
            /// A loop head without a body is not reported at all, so the caller
            /// gets the pair it needs or nothing.
            fn loop_targets(&self) -> Option<(ArenaNode<'a>, Option<ArenaNode<'a>>)> {
                loop_targets_of(self.as_node().as_deref()?)
            }

            /// Whether both sides hold a node and it is the same one.
            ///
            /// A side that holds no node is never the same as anything, not even
            /// as another empty side. Where two absent nodes have to count as
            /// equal, use [`same_optional_as`](NodeAccessorExt::same_optional_as).
            ///
            /// Identity is [`ArenaNode::key`](crate::ast::nodes::ArenaNode::key):
            /// the address of the node in the arena, which never moves what it
            /// has handed out.
            fn same_as(&self, other: impl AsNode<'a>) -> bool {
                match (self.as_key(), other.as_key()) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                }
            }

            /// Whether the two sides are the same node, with two absent sides
            /// counting as the same.
            ///
            /// This is the question a pass asks when it wants to know whether a
            /// target it looked up is still the one it expects: both lookups
            /// coming back empty is agreement, not a mismatch.
            fn same_optional_as(&self, other: impl AsNode<'a>) -> bool {
                match (self.as_key(), other.as_key()) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => true,
                    _ => false,
                }
            }

            /// Whether a pass gave up on the node.
            fn has_error(&self) -> bool {
                self.as_node().as_deref().is_some_and(node_has_error)
            }

            /// Whether the node's function could not be decompiled.
            fn has_failure(&self) -> bool {
                self.as_node().as_deref().is_some_and(node_has_failure)
            }

            /// Whether the decompiler had to give up on the node, wholly or in
            /// part.
            fn has_recovery(&self) -> bool {
                self.as_node().as_deref().is_some_and(node_has_recovery)
            }

            /// The register an identifier names, if this node is one.
            fn identifier_slot(&self) -> Option<u32> {
                self.as_node()
                    .as_deref()?
                    .identifier()
                    .map(|identifier| identifier.slot)
            }

            /// What an identifier refers to, if this node is one.
            fn identifier_kind(&self) -> Option<IdentifierKind> {
                self.as_node()
                    .as_deref()?
                    .identifier()
                    .map(|identifier| identifier.kind)
            }

            /// The kind and register of an identifier, if this node is one.
            fn identifier_kind_and_slot(&self) -> Option<(IdentifierKind, u32)> {
                self.as_node()
                    .as_deref()?
                    .identifier()
                    .map(|identifier| (identifier.kind, identifier.slot))
            }

            /// The register of an identifier that still stands for a register.
            ///
            /// This is `None` once the slot handling passes have given the
            /// reference a name, which is what the passes are waiting for: an
            /// unnamed register is one they still have to resolve.
            fn unnamed_slot(&self) -> Option<u32> {
                self.identifier_kind_and_slot()
                    .filter(|(kind, _)| *kind == IdentifierKind::Slot)
                    .map(|(_, slot)| slot)
            }

            /// The id of the assignment an identifier came from, if it has one.
            fn identifier_id(&self) -> Option<u32> {
                self.as_node()
                    .as_deref()?
                    .identifier()
                    .and_then(|identifier| identifier.id)
            }

            /// The ids an identifier may refer to, when one could not be pinned
            /// down.
            ///
            /// A node that is not an identifier has none, so the answer is an
            /// empty vector rather than `None`.
            fn identifier_possible_ids(&self) -> Vec<u32> {
                self.as_node()
                    .as_deref()
                    .and_then(Node::identifier)
                    .map(|identifier| identifier.possible_ids.iter().copied().collect())
                    .unwrap_or_default()
            }

            /// The name of an identifier, once one is known.
            fn identifier_name(&self) -> Option<&'a str> {
                self.as_node()
                    .as_deref()?
                    .identifier()
                    .and_then(|identifier| identifier.name)
            }

            /// The address of the statement a node belongs to, if one is known.
            ///
            /// A statement knows its own address, but an expression only knows
            /// the address of the statement it belongs to, so the few node kinds
            /// that can hold an expression are looked through to find it. ljd
            /// does the same.
            fn find_addr(&self) -> Option<u32> {
                let node = self.as_node();
                let node = node.as_deref()?;

                if let Some(addr) = node.addr() {
                    return Some(addr);
                }

                let (first, second) = match node {
                    Node::Assignment(inner) => (inner.destinations.list_first(), None),
                    Node::If(inner) => (Some(inner.expression), None),
                    Node::UnaryOperator(inner) => (Some(inner.operand), None),
                    Node::BinaryOperator(inner) => (Some(inner.left), Some(inner.right)),
                    _ => (None, None),
                };

                first
                    .and_then(|node| node.find_addr())
                    .or_else(|| second.and_then(|node| node.find_addr()))
            }

            /// Whether this is one of the lists the slot passes walk.
            ///
            /// This is [`Node::list`] without a table constructor's records: a
            /// record's key and value are reached through the record, so the
            /// records list is never the node those passes rewrite.
            fn is_slot_list(&self) -> bool {
                matches!(
                    self.as_node().as_deref(),
                    Some(
                        Node::Variables(_)
                            | Node::Identifiers(_)
                            | Node::Expressions(_)
                            | Node::Statements(_)
                    )
                )
            }
        }

        impl<'a, T: AsNode<'a>> NodeAccessorExt<'a> for T {}
    };
}

node_predicates! {
    payloads: {
        /// A list of statements.
        is_statements, is_statements_and => Statements: ArenaVec<'a, ArenaNode<'a>>,
        /// A list of expressions.
        is_expressions, is_expressions_and => Expressions: ArenaVec<'a, ArenaNode<'a>>,
        /// A list of assignable variables.
        is_variables, is_variables_and => Variables: ArenaVec<'a, ArenaNode<'a>>,
        /// A list of identifiers.
        is_identifiers, is_identifiers_and => Identifiers: ArenaVec<'a, ArenaNode<'a>>,
        /// A list of table constructor records.
        is_records, is_records_and => Records: ArenaVec<'a, ArenaNode<'a>>,
        /// An assignment.
        is_assignment, is_assignment_and => Assignment: Assignment<'a>,
        /// A function call used as a statement.
        is_function_call, is_function_call_and => FunctionCall: FunctionCall<'a>,
        /// A `return` statement.
        is_return, is_return_and => Return: Return<'a>,
        /// A no-op placeholder.
        is_no_op, is_no_op_and => NoOp: NoOp,
        /// An `if` statement.
        is_if, is_if_and => If: If<'a>,
        /// A `while` loop.
        is_while, is_while_and => While: While<'a>,
        /// A `repeat ... until` loop.
        is_repeat_until, is_repeat_until_and => RepeatUntil: RepeatUntil<'a>,
        /// A numeric `for` loop.
        is_numeric_for, is_numeric_for_and => NumericFor: NumericFor<'a>,
        /// A generic `for ... in` loop.
        is_iterator_for, is_iterator_for_and => IteratorFor: IteratorFor<'a>,
        /// A function definition.
        is_function_definition, is_function_definition_and => FunctionDefinition: FunctionDefinition<'a>,
        /// An `elseif` branch.
        is_else_if, is_else_if_and => ElseIf: ElseIf<'a>,
        /// A variable reference.
        is_identifier, is_identifier_and => Identifier: Identifier<'a>,
        /// A table indexing expression.
        is_table_element, is_table_element_and => TableElement: TableElement<'a>,
        /// A literal constant.
        is_constant, is_constant_and => Constant: Constant<'a>,
        /// `nil`, `true` or `false`.
        is_primitive, is_primitive_and => Primitive: Primitive,
        /// A table constructor.
        is_table_constructor, is_table_constructor_and => TableConstructor: TableConstructor<'a>,
        /// A binary operator.
        is_binary_operator, is_binary_operator_and => BinaryOperator: BinaryOperator<'a>,
        /// A unary operator.
        is_unary_operator, is_unary_operator_and => UnaryOperator: UnaryOperator<'a>,
        /// An array element inside a table constructor.
        is_array_record, is_array_record_and => ArrayRecord: ArrayRecord<'a>,
        /// A key/value pair inside a table constructor.
        is_table_record, is_table_record_and => TableRecord: TableRecord<'a>,
        /// A basic block.
        is_block, is_block_and => Block: Block<'a>,
        /// An unconditional jump or fallthrough.
        is_unconditional_warp, is_unconditional_warp_and => UnconditionalWarp: UnconditionalWarp<'a>,
        /// A conditional branch.
        is_conditional_warp, is_conditional_warp_and => ConditionalWarp: ConditionalWarp<'a>,
        /// A generic `for ... in` loop head.
        is_iterator_warp, is_iterator_warp_and => IteratorWarp: IteratorWarp<'a>,
        /// A numeric `for` loop head.
        is_numeric_loop_warp, is_numeric_loop_warp_and => NumericLoopWarp: NumericLoopWarp<'a>,
        /// The end of a function.
        is_end_warp, is_end_warp_and => EndWarp: EndWarp<'a>,
    },
    simple: {
        /// A `break` statement.
        is_break => Node::Break,
        /// `...`
        is_vararg => Node::Vararg,
        /// The result of a previous multi result call.
        is_mul_tres => Node::MulTres,
        /// Whether a constant holds a string.
        is_constant_string => Node::Constant(inner) if matches!(inner.value, ConstantValue::String(_)),
        /// Whether a node is the primitive `nil`.
        is_primitive_nil => Node::Primitive(inner) if inner.kind == PrimitiveKind::Nil,
        /// Whether a node is the primitive `true`.
        is_primitive_true => Node::Primitive(inner) if inner.kind == PrimitiveKind::True,
        /// Whether a node is the primitive `false`.
        is_primitive_false => Node::Primitive(inner) if inner.kind == PrimitiveKind::False,
        /// Whether a warp simply falls through into the next block.
        is_flow => Node::UnconditionalWarp(inner) if inner.kind == UnconditionalWarpKind::Flow,
        /// Whether a warp is an unconditional jump somewhere else.
        is_jump => Node::UnconditionalWarp(inner) if inner.kind == UnconditionalWarpKind::Jump,
    },
}

// MARK: tests

#[cfg(test)]
mod tests {
    use oxc_allocator::{Allocator, ArenaBox};

    use super::*;

    /// An unconditional warp of the given kind, with no target.
    fn warp<'a>(alloc: &'a Allocator, kind: UnconditionalWarpKind) -> ArenaNode<'a> {
        ArenaNode::new(
            alloc,
            Node::UnconditionalWarp(ArenaBox::new_in(
                UnconditionalWarp {
                    kind,
                    target: None,
                    is_uclo: false,
                    meta: Meta::default(),
                },
                &alloc,
            )),
        )
    }

    #[test]
    fn a_predicate_answers_the_same_for_a_node_and_for_a_reference() {
        let alloc = &Allocator::new();
        let node = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let owned = Node::Primitive(Primitive {
            kind: PrimitiveKind::Nil,
        });

        // The same question asked of each form `AsNode` covers: a node, a
        // `ArenaNode`, the `Ref` a reference borrows to, and a plain `&Node`.
        assert!(owned.is_primitive());
        assert!(node.is_primitive());
        assert!(node.borrow().is_primitive());

        assert!(!owned.is_block());
        assert!(!node.is_block());
        assert!(!node.borrow().is_block());
    }

    #[test]
    fn an_absent_node_answers_false() {
        let alloc = &Allocator::new();
        let node = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);

        let present: Option<ArenaNode<'_>> = Some(node);
        let absent: Option<ArenaNode<'_>> = None;
        assert!(present.is_primitive());
        assert!(!absent.is_primitive());
        assert!(!absent.is_block());

        let held = node.borrow();
        let borrowed: Option<&Node<'_>> = Some(&held);
        let absent_node: Option<&Node<'_>> = None;
        assert!(borrowed.is_primitive());
        assert!(!absent_node.is_primitive());

        let owned: Option<Node<'_>> = Some(Node::Primitive(Primitive {
            kind: PrimitiveKind::True,
        }));
        assert!(owned.is_primitive());
    }

    #[test]
    fn a_unit_variant_is_matched_without_a_payload() {
        let alloc = &Allocator::new();
        let vararg = ArenaNode::new(alloc, Node::Vararg);
        let brk = ArenaNode::new(alloc, Node::Break);
        assert!(vararg.is_vararg());
        assert!(!vararg.is_break());
        assert!(brk.is_break());
        assert!(!brk.is_vararg());
    }

    #[test]
    fn the_three_primitives_are_told_apart() {
        let alloc = &Allocator::new();
        let nil = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let tru = ArenaNode::new_primitive(alloc, PrimitiveKind::True);
        let fal = ArenaNode::new_primitive(alloc, PrimitiveKind::False);

        // Every one of them is a primitive, and each one is only itself.
        for node in [nil, tru, fal] {
            assert!(node.is_primitive());
        }
        assert!(nil.is_primitive_nil() && !nil.is_primitive_true() && !nil.is_primitive_false());
        assert!(tru.is_primitive_true() && !tru.is_primitive_nil() && !tru.is_primitive_false());
        assert!(fal.is_primitive_false() && !fal.is_primitive_nil() && !fal.is_primitive_true());

        // The reference form and an absent node answer the same way.
        assert!(nil.borrow().is_primitive_nil());
        assert!(tru.borrow().is_primitive_true());
        assert!(fal.borrow().is_primitive_false());
        let absent: Option<ArenaNode<'_>> = None;
        assert!(
            !absent.is_primitive_nil()
                && !absent.is_primitive_true()
                && !absent.is_primitive_false()
        );

        // A constant is not a primitive, whatever it holds.
        let zero = ArenaNode::new(
            alloc,
            Node::Constant(ArenaBox::new_in(
                Constant {
                    value: ConstantValue::Integer(0),
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );
        assert!(
            !zero.is_primitive_nil() && !zero.is_primitive_true() && !zero.is_primitive_false()
        );
    }

    #[test]
    fn a_string_constant_is_told_from_another_constant() {
        let alloc = &Allocator::new();
        let text = ArenaNode::new(
            alloc,
            Node::Constant(ArenaBox::new_in(
                Constant {
                    value: ConstantValue::String(b"name"),
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );
        let number = ArenaNode::new(
            alloc,
            Node::Constant(ArenaBox::new_in(
                Constant {
                    value: ConstantValue::Integer(1),
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );

        // Both are constants, and only one of them holds a string.
        assert!(text.is_constant() && number.is_constant());
        assert!(text.is_constant_string() && !number.is_constant_string());
        assert!(text.borrow().is_constant_string());

        // A node of another kind, and an absent one, are not strings either.
        let nil = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let absent: Option<ArenaNode<'_>> = None;
        assert!(!nil.is_constant_string());
        assert!(!absent.is_constant_string());
    }

    #[test]
    fn a_guard_tells_flow_from_jump() {
        let alloc = &Allocator::new();
        let flow = warp(alloc, UnconditionalWarpKind::Flow);
        let jump = warp(alloc, UnconditionalWarpKind::Jump);

        // Both are unconditional warps, and only one of them is each of the two.
        assert!(flow.is_unconditional_warp() && jump.is_unconditional_warp());
        assert!(flow.is_flow() && !flow.is_jump());
        assert!(jump.is_jump() && !jump.is_flow());
        assert!(flow.borrow().is_flow());
        assert!(jump.borrow().is_jump());
    }

    #[test]
    fn the_and_form_looks_inside_the_payload() {
        let alloc = &Allocator::new();
        let nil = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let true_node = ArenaNode::new_primitive(alloc, PrimitiveKind::True);

        // Both are primitives, and only one of them is nil.
        assert!(nil.is_primitive() && true_node.is_primitive());
        assert!(nil.is_primitive_and(|primitive| primitive.kind == PrimitiveKind::Nil));
        assert!(!true_node.is_primitive_and(|primitive| primitive.kind == PrimitiveKind::Nil));

        // A predicate on the wrong kind is never reached.
        let block = warp(alloc, UnconditionalWarpKind::Flow);
        let mut asked = false;
        assert!(!block.is_primitive_and(|_| {
            asked = true;
            true
        }));
        assert!(!asked, "the predicate should not run for another variant");

        // The reference form answers the same question.
        assert!(
            nil.borrow()
                .is_primitive_and(|p| p.kind == PrimitiveKind::Nil)
        );
        assert!(!block.borrow().is_primitive_and(|_| true));
    }

    #[test]
    fn the_and_form_reaches_a_boxed_payload() {
        let alloc = &Allocator::new();
        let target = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let jump = warp(alloc, UnconditionalWarpKind::Jump);
        jump.borrow_mut().set_target(Some(target));

        // The payload is a boxed struct, and the closure sees the struct.
        assert!(jump.is_unconditional_warp_and(|warp| warp.kind == UnconditionalWarpKind::Jump));
        assert!(!jump.is_unconditional_warp_and(|warp| warp.kind == UnconditionalWarpKind::Flow));
        assert!(jump.is_unconditional_warp_and(|warp| warp.target.is_some()));
        assert!(
            jump.borrow()
                .is_unconditional_warp_and(|warp| !warp.is_uclo)
        );
    }

    #[test]
    fn an_absent_node_never_runs_an_and_predicate() {
        let alloc = &Allocator::new();
        let node = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let absent: Option<ArenaNode<'_>> = None;

        let mut asked = false;
        assert!(!absent.is_primitive_and(|_| {
            asked = true;
            true
        }));
        assert!(!asked);
        assert!(Some(node).is_primitive_and(|p| p.kind == PrimitiveKind::Nil));
    }

    #[test]
    fn identity_is_the_node_not_the_reference() {
        let alloc = &Allocator::new();
        let one = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let other = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let alias = one;

        assert!(one.same_as(alias));
        assert!(!one.same_as(other));
        assert!(Some(one).same_as(Some(alias)));
        assert!(!Some(one).same_as(Some(other)));
    }

    #[test]
    fn an_absent_node_is_never_the_same_as_anything() {
        let alloc = &Allocator::new();
        let node = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let absent: Option<ArenaNode<'_>> = None;

        // `same_as` says no, even to another absent node; `same_optional_as`
        // is the one that treats two absent nodes as agreeing.
        assert!(!absent.same_as(absent));
        assert!(!absent.same_as(Some(node)));
        assert!(!Some(node).same_as(absent));
        assert!(absent.same_optional_as(absent));
        assert!(!absent.same_optional_as(Some(node)));
        assert!(!Some(node).same_optional_as(absent));
        assert!(Some(node).same_optional_as(Some(node)));
    }

    #[test]
    fn a_warp_reports_and_replaces_its_target() {
        let alloc = &Allocator::new();
        let target = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let jump = warp(alloc, UnconditionalWarpKind::Jump);

        assert!(jump.target(false).is_none());
        jump.borrow_mut().set_target(Some(target));
        assert!(jump.target(false).unwrap().same_as(target));

        // A node that is not a warp has no target and quietly ignores one.
        let primitive = ArenaNode::new_primitive(alloc, PrimitiveKind::True);
        assert!(primitive.target(false).is_none());
        primitive.borrow_mut().set_target(Some(target));
    }

    #[test]
    fn an_end_warp_needs_to_be_asked_for() {
        let alloc = &Allocator::new();
        let end = ArenaNode::new(
            alloc,
            Node::EndWarp(ArenaBox::new_in(
                EndWarp {
                    target: None,
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );
        assert!(end.target(false).is_none());
        assert!(end.target(true).is_none());
        assert!(end.is_end_warp());
    }

    #[test]
    fn a_conditional_reports_both_branches() {
        let alloc = &Allocator::new();
        let node = ArenaNode::new(
            alloc,
            Node::ConditionalWarp(ArenaBox::new_in(
                ConditionalWarp {
                    condition: None,
                    true_target: None,
                    false_target: None,
                    slot: Some(3),
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );

        // A branch is described by its false target, so that is what `target`
        // reports, while `conditional_targets` hands over the pair.
        assert!(node.target(false).is_none());
        let (true_target, false_target, slot) = node.conditional_targets().unwrap();
        assert!(true_target.is_none());
        assert!(false_target.is_none());
        assert_eq!(slot, Some(3));
        assert!(node.jump_of().is_none());
        assert!(!node.is_flow());
    }

    #[test]
    fn the_recovery_markers_read_the_same_from_a_reference() {
        let alloc = &Allocator::new();
        let node = ArenaNode::new(
            alloc,
            Node::NoOp(ArenaBox::new_in(
                NoOp {
                    meta: Meta::default(),
                },
                &alloc,
            )),
        );

        assert!(!node.has_error() && !node.has_failure() && !node.has_recovery());
        node.mark_error();
        assert!(node.has_error() && node.has_recovery());
        assert!(!node.has_failure());
        node.mark_failure();
        assert!(node.has_failure() && node.has_recovery());
    }
}
