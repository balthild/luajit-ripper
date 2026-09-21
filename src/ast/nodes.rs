//! The decompiler's intermediate representation.
//!
//! The representation mirrors ljd's AST: every node lives in an
//! [`oxc_allocator::Allocator`] and passes hold [`NodeRef`]s to it, so that they
//! can hold on to a node, compare identities and rewrite nodes in place, exactly
//! like the Python implementation does with its objects. Because the nodes are
//! arena allocated, the control flow graph's cycles cost nothing to free: the
//! whole arena is released at once when the decompiler is done with it.
//!
//! Child nodes are always [`NodeRef`]s, including lists: a statement list is a
//! node of its own so that a pass can find it and rewrite its contents.

use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;

use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use crate::bytecode::DebugInfo;

// MARK: building nodes

/// A shared, mutable AST node.
///
/// Nodes are allocated in an arena and never freed individually, so a plain
/// shared reference to a [`RefCell`] is all that is needed: identity is the
/// address of the node, and the borrow flag is what keeps the passes honest.
pub type NodeRef<'a> = &'a RefCell<Node<'a>>;

/// Where a node came from in the bytecode.
///
/// `addr` is the instruction address and `line` the source line; both are `0`
/// when unknown (instruction `0` is the synthesised function header and line
/// `0` does not exist in a real source file).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Meta {
    pub addr: u32,
    pub line: u32,
    /// Whether a pass gave up on this node.
    ///
    /// The writer points such a node out with a comment, so that a reader knows
    /// the code around it is not to be trusted. Only the unwarper's recovery
    /// mode ever sets it; see [`crate::ast::unwarper::Recovery`].
    pub error_here: bool,
    /// Whether this function could not be decompiled at all.
    ///
    /// Unlike [`Meta::error_here`], which says that a region inside a function
    /// was given up on, this one says that the function itself had to be
    /// replaced by a call that reports the failure. Nothing is written from it,
    /// so it marks the function for a caller that wants to tell a chunk that
    /// came out whole from one that only came out in part.
    pub failed_here: bool,
}

impl Meta {
    pub fn new(addr: u32, line: u32) -> Self {
        Meta {
            addr,
            line,
            error_here: false,
            failed_here: false,
        }
    }
}

/// Every kind of node the decompiler knows about.
#[derive(Debug)]
pub enum Node<'a> {
    // MARK: list kinds
    /// A list of statements.
    Statements(ArenaVec<'a, NodeRef<'a>>),
    /// A list of expressions.
    Expressions(ArenaVec<'a, NodeRef<'a>>),
    /// A list of assignable variables.
    Variables(ArenaVec<'a, NodeRef<'a>>),
    /// A list of identifiers.
    Identifiers(ArenaVec<'a, NodeRef<'a>>),
    /// A list of table constructor records.
    Records(ArenaVec<'a, NodeRef<'a>>),

    // MARK: statement kinds
    /// An assignment; `kind` distinguishes `local x = 1` from `x = 1`.
    Assignment(ArenaBox<'a, Assignment<'a>>),
    /// A function call used as a statement.
    FunctionCall(ArenaBox<'a, FunctionCall<'a>>),
    /// A `return` statement.
    Return(ArenaBox<'a, Return<'a>>),
    /// A `break` statement.
    Break,
    /// A no-op, used as a placeholder for empty blocks.
    NoOp(ArenaBox<'a, NoOp>),
    /// An `if` statement.
    If(ArenaBox<'a, If<'a>>),
    /// A `while` loop.
    While(ArenaBox<'a, While<'a>>),
    /// A `repeat ... until` loop.
    RepeatUntil(ArenaBox<'a, RepeatUntil<'a>>),
    /// A numeric `for` loop.
    NumericFor(ArenaBox<'a, NumericFor<'a>>),
    /// A generic `for ... in` loop.
    IteratorFor(ArenaBox<'a, IteratorFor<'a>>),
    /// A `function ... end` definition, as a statement or as an expression.
    FunctionDefinition(ArenaBox<'a, FunctionDefinition<'a>>),
    /// An `elseif` branch.
    ElseIf(ArenaBox<'a, ElseIf<'a>>),

    // MARK: expression kinds
    /// A variable reference.
    Identifier(ArenaBox<'a, Identifier<'a>>),
    /// A table indexing expression.
    TableElement(ArenaBox<'a, TableElement<'a>>),
    /// A literal constant.
    Constant(ArenaBox<'a, Constant<'a>>),
    /// `nil`, `true` or `false`.
    Primitive(Primitive),
    /// A table constructor.
    TableConstructor(ArenaBox<'a, TableConstructor<'a>>),
    /// `...`
    Vararg,
    /// The result of a previous multi result call.
    MulTres,
    /// A binary operator.
    BinaryOperator(ArenaBox<'a, BinaryOperator<'a>>),
    /// A unary operator.
    UnaryOperator(ArenaBox<'a, UnaryOperator<'a>>),
    /// An array element inside a table constructor.
    ArrayRecord(ArenaBox<'a, ArrayRecord<'a>>),
    /// A key/value pair inside a table constructor.
    TableRecord(ArenaBox<'a, TableRecord<'a>>),

    // MARK: control flow kinds
    /// A basic block. Only present before unwarping.
    Block(ArenaBox<'a, Block<'a>>),
    /// An unconditional jump or fallthrough.
    UnconditionalWarp(ArenaBox<'a, UnconditionalWarp<'a>>),
    /// A conditional branch.
    ConditionalWarp(ArenaBox<'a, ConditionalWarp<'a>>),
    /// A generic `for ... in` loop head.
    IteratorWarp(ArenaBox<'a, IteratorWarp<'a>>),
    /// A numeric `for` loop head.
    NumericLoopWarp(ArenaBox<'a, NumericLoopWarp<'a>>),
    /// The end of a function.
    EndWarp(ArenaBox<'a, EndWarp<'a>>),
}

// MARK: constructors

impl<'a> Node<'a> {
    /// Wraps a node into an arena allocated node.
    pub fn emplace(alloc: &'a Allocator, node: Node<'a>) -> NodeRef<'a> {
        alloc.alloc(RefCell::new(node))
    }

    /// Creates a statement list node.
    pub fn emplace_statements(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = NodeRef<'a>>,
    ) -> NodeRef<'a> {
        Node::emplace(
            alloc,
            Node::Statements(ArenaVec::from_iter_in(contents, &alloc)),
        )
    }

    /// Creates an expression list node.
    pub fn emplace_expressions(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = NodeRef<'a>>,
    ) -> NodeRef<'a> {
        Node::emplace(
            alloc,
            Node::Expressions(ArenaVec::from_iter_in(contents, &alloc)),
        )
    }

    /// Creates a variable list node.
    pub fn emplace_variables(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = NodeRef<'a>>,
    ) -> NodeRef<'a> {
        Node::emplace(
            alloc,
            Node::Variables(ArenaVec::from_iter_in(contents, &alloc)),
        )
    }

    /// Creates an identifier list node.
    pub fn emplace_identifiers(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = NodeRef<'a>>,
    ) -> NodeRef<'a> {
        Node::emplace(
            alloc,
            Node::Identifiers(ArenaVec::from_iter_in(contents, &alloc)),
        )
    }

    /// Creates a record list node.
    pub fn emplace_records(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = NodeRef<'a>>,
    ) -> NodeRef<'a> {
        Node::emplace(
            alloc,
            Node::Records(ArenaVec::from_iter_in(contents, &alloc)),
        )
    }

    /// Creates a `nil`, `true` or `false` node.
    pub fn emplace_primitive(alloc: &'a Allocator, kind: PrimitiveKind) -> NodeRef<'a> {
        Node::emplace(alloc, Node::Primitive(Primitive { kind }))
    }
}

// MARK: accessors

impl<'a> Node<'a> {
    /// Human readable node kind, used in error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Node::Statements(_) => "statement list",
            Node::Expressions(_) => "expression list",
            Node::Variables(_) => "variable list",
            Node::Identifiers(_) => "identifier list",
            Node::Records(_) => "record list",
            Node::Assignment(_) => "assignment",
            Node::FunctionCall(_) => "call",
            Node::Return(_) => "return",
            Node::Break => "break",
            Node::NoOp(_) => "no-op",
            Node::If(_) => "if",
            Node::While(_) => "while",
            Node::RepeatUntil(_) => "repeat",
            Node::NumericFor(_) => "numeric for",
            Node::IteratorFor(_) => "iterator for",
            Node::FunctionDefinition(_) => "function",
            Node::ElseIf(_) => "elseif",
            Node::Identifier(_) => "identifier",
            Node::TableElement(_) => "table element",
            Node::Constant(_) => "constant",
            Node::Primitive(_) => "primitive",
            Node::TableConstructor(_) => "table constructor",
            Node::Vararg => "vararg",
            Node::MulTres => "multi result",
            Node::BinaryOperator(_) => "binary operator",
            Node::UnaryOperator(_) => "unary operator",
            Node::ArrayRecord(_) => "array record",
            Node::TableRecord(_) => "table record",
            Node::Block(_) => "block",
            Node::UnconditionalWarp(_) => "unconditional warp",
            Node::ConditionalWarp(_) => "conditional warp",
            Node::IteratorWarp(_) => "iterator warp",
            Node::NumericLoopWarp(_) => "numeric loop warp",
            Node::EndWarp(_) => "end warp",
        }
    }

    /// The instruction address this node was built from, if it has one.
    ///
    /// Only statements, the root of the expression an assignment computes, the
    /// warp of a block and placeholder nodes carry one, mirroring the `_addr`
    /// attribute ljd assigns while building the AST. Address `0` is the
    /// synthesised function header and is never attached to a node, so it is
    /// reported as "no address".
    pub fn addr(&self) -> Option<u32> {
        let addr = self.meta()?.addr;
        (addr != 0).then_some(addr)
    }

    /// The metadata of this node, if it carries any.
    ///
    /// List nodes and `Primitive` carry none.
    pub fn meta(&self) -> Option<&Meta> {
        match self {
            Node::Assignment(inner) => Some(&inner.meta),
            Node::FunctionCall(inner) => Some(&inner.meta),
            Node::Return(inner) => Some(&inner.meta),
            Node::NoOp(inner) => Some(&inner.meta),
            Node::If(inner) => Some(&inner.meta),
            Node::ElseIf(inner) => Some(&inner.meta),
            Node::While(inner) => Some(&inner.meta),
            Node::RepeatUntil(inner) => Some(&inner.meta),
            Node::NumericFor(inner) => Some(&inner.meta),
            Node::IteratorFor(inner) => Some(&inner.meta),
            Node::FunctionDefinition(inner) => Some(&inner.meta),
            Node::Identifier(inner) => Some(&inner.meta),
            Node::TableElement(inner) => Some(&inner.meta),
            Node::Constant(inner) => Some(&inner.meta),
            Node::TableConstructor(inner) => Some(&inner.meta),
            Node::BinaryOperator(inner) => Some(&inner.meta),
            Node::UnaryOperator(inner) => Some(&inner.meta),
            Node::ArrayRecord(inner) => Some(&inner.meta),
            Node::TableRecord(inner) => Some(&inner.meta),
            Node::UnconditionalWarp(inner) => Some(&inner.meta),
            Node::ConditionalWarp(inner) => Some(&inner.meta),
            Node::IteratorWarp(inner) => Some(&inner.meta),
            Node::NumericLoopWarp(inner) => Some(&inner.meta),
            Node::EndWarp(inner) => Some(&inner.meta),
            _ => None,
        }
    }

    /// The metadata of this node, for mutation.
    pub fn meta_mut(&mut self) -> Option<&mut Meta> {
        match self {
            Node::Assignment(inner) => Some(&mut inner.meta),
            Node::FunctionCall(inner) => Some(&mut inner.meta),
            Node::Return(inner) => Some(&mut inner.meta),
            Node::NoOp(inner) => Some(&mut inner.meta),
            Node::If(inner) => Some(&mut inner.meta),
            Node::ElseIf(inner) => Some(&mut inner.meta),
            Node::While(inner) => Some(&mut inner.meta),
            Node::RepeatUntil(inner) => Some(&mut inner.meta),
            Node::NumericFor(inner) => Some(&mut inner.meta),
            Node::IteratorFor(inner) => Some(&mut inner.meta),
            Node::FunctionDefinition(inner) => Some(&mut inner.meta),
            Node::Identifier(inner) => Some(&mut inner.meta),
            Node::TableElement(inner) => Some(&mut inner.meta),
            Node::Constant(inner) => Some(&mut inner.meta),
            Node::TableConstructor(inner) => Some(&mut inner.meta),
            Node::BinaryOperator(inner) => Some(&mut inner.meta),
            Node::UnaryOperator(inner) => Some(&mut inner.meta),
            Node::ArrayRecord(inner) => Some(&mut inner.meta),
            Node::TableRecord(inner) => Some(&mut inner.meta),
            Node::UnconditionalWarp(inner) => Some(&mut inner.meta),
            Node::ConditionalWarp(inner) => Some(&mut inner.meta),
            Node::IteratorWarp(inner) => Some(&mut inner.meta),
            Node::NumericLoopWarp(inner) => Some(&mut inner.meta),
            Node::EndWarp(inner) => Some(&mut inner.meta),
            _ => None,
        }
    }

    /// The contents of a list node.
    pub fn list(&self) -> Option<&ArenaVec<'a, NodeRef<'a>>> {
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
    pub fn list_mut(&mut self) -> Option<&mut ArenaVec<'a, NodeRef<'a>>> {
        match self {
            Node::Statements(items)
            | Node::Expressions(items)
            | Node::Variables(items)
            | Node::Identifiers(items)
            | Node::Records(items) => Some(items),
            _ => None,
        }
    }

    /// The identifier payload, if this is an identifier.
    pub fn identifier(&self) -> Option<&Identifier<'a>> {
        match self {
            Node::Identifier(identifier) => Some(identifier),
            _ => None,
        }
    }
}

impl fmt::Display for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Identifier(identifier) => f.write_str(&identifier.name_or_slot()),
            _ => f.write_str(self.kind()),
        }
    }
}

// MARK: lists

/// Reads the contents of a list node.
///
/// The copy is a plain `Vec`: it is a snapshot the caller works on, not part of
/// the tree.
pub fn list_contents<'a>(node: NodeRef<'a>) -> Vec<NodeRef<'a>> {
    match node.borrow().list() {
        Some(items) => items.iter().copied().collect(),
        None => Vec::new(),
    }
}

/// Replaces the contents of a list node.
pub fn set_list_contents<'a>(
    alloc: &'a Allocator,
    node: NodeRef<'a>,
    contents: impl IntoIterator<Item = NodeRef<'a>>,
) {
    if let Some(items) = node.borrow_mut().list_mut() {
        *items = ArenaVec::from_iter_in(contents, &alloc);
    }
}

/// Appends to a list node.
///
/// The list already knows the arena it was allocated in, so no allocator is
/// needed here.
pub fn push<'a>(node: NodeRef<'a>, child: NodeRef<'a>) {
    if let Some(items) = node.borrow_mut().list_mut() {
        items.push(child);
    }
}

// MARK: metadata

/// Replaces the metadata of a node.
///
/// Nodes that carry no metadata of their own are left alone.
pub fn set_meta<'a>(node: NodeRef<'a>, meta: Meta) {
    if let Some(target) = node.borrow_mut().meta_mut() {
        *target = meta;
    }
}

// MARK: recovery markers

/// Records that a pass gave up on `node`.
pub fn mark_error<'a>(node: NodeRef<'a>) {
    if let Some(meta) = node.borrow_mut().meta_mut() {
        meta.error_here = true;
    }
}

/// Whether a pass gave up on `node`.
pub fn has_error<'a>(node: NodeRef<'a>) -> bool {
    node.borrow().meta().is_some_and(|meta| meta.error_here)
}

/// Records that `node`'s function could not be decompiled.
pub fn mark_failure<'a>(node: NodeRef<'a>) {
    if let Some(meta) = node.borrow_mut().meta_mut() {
        meta.failed_here = true;
    }
}

/// Whether `node`'s function could not be decompiled.
pub fn has_failure<'a>(node: NodeRef<'a>) -> bool {
    node.borrow().meta().is_some_and(|meta| meta.failed_here)
}

/// Whether the decompiler had to give up on `node`, wholly or in part.
///
/// This is what tells a chunk that came out whole from one that only came out
/// in part: either a region around the node was recovered, or the function the
/// node belongs to was replaced by a call that reports the failure.
pub fn has_recovery<'a>(node: NodeRef<'a>) -> bool {
    has_error(node) || has_failure(node)
}

// MARK: statements

/// `local x = 1` versus `x = 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentKind {
    /// The assignment introduces new locals.
    LocalDefinition,
    /// The assignment writes to existing variables.
    Normal,
}

/// An assignment statement.
#[derive(Debug)]
pub struct Assignment<'a> {
    pub expressions: NodeRef<'a>,
    pub destinations: NodeRef<'a>,
    pub kind: AssignmentKind,
    pub meta: Meta,
}

/// A function call.
#[derive(Debug)]
pub struct FunctionCall<'a> {
    pub function: NodeRef<'a>,
    pub arguments: NodeRef<'a>,
    pub is_method: bool,
    pub meta: Meta,
}

/// A `return` statement.
#[derive(Debug)]
pub struct Return<'a> {
    pub returns: NodeRef<'a>,
    pub meta: Meta,
}

/// An `if` statement.
#[derive(Debug)]
pub struct If<'a> {
    pub expression: NodeRef<'a>,
    pub then_block: NodeRef<'a>,
    pub elseifs: ArenaVec<'a, NodeRef<'a>>,
    pub else_block: NodeRef<'a>,
    pub meta: Meta,
}

/// An `elseif` branch.
#[derive(Debug)]
pub struct ElseIf<'a> {
    pub expression: NodeRef<'a>,
    pub then_block: NodeRef<'a>,
    pub meta: Meta,
}

/// A `while` loop.
#[derive(Debug)]
pub struct While<'a> {
    pub expression: NodeRef<'a>,
    pub statements: NodeRef<'a>,
    pub meta: Meta,
}

/// A `repeat ... until` loop.
#[derive(Debug)]
pub struct RepeatUntil<'a> {
    pub expression: NodeRef<'a>,
    pub statements: NodeRef<'a>,
    pub meta: Meta,
}

/// A numeric `for` loop.
#[derive(Debug)]
pub struct NumericFor<'a> {
    pub variable: NodeRef<'a>,
    pub expressions: NodeRef<'a>,
    pub statements: NodeRef<'a>,
    pub meta: Meta,
}

/// A generic `for ... in` loop.
#[derive(Debug)]
pub struct IteratorFor<'a> {
    pub identifiers: NodeRef<'a>,
    pub expressions: NodeRef<'a>,
    pub statements: NodeRef<'a>,
    pub meta: Meta,
}

/// A function definition.
#[derive(Debug)]
pub struct FunctionDefinition<'a> {
    pub arguments: NodeRef<'a>,
    pub statements: NodeRef<'a>,
    /// Raw upvalue references of the prototype.
    pub upvalues: ArenaVec<'a, u16>,
    /// Debug information of the prototype.
    ///
    /// This is shared with the prototype the function was built from rather
    /// than copied, because every instruction of the function points into it.
    pub debug: &'a DebugInfo<'a>,
    /// Number of instructions, header included.
    pub instruction_count: usize,
    pub meta: Meta,
}

// MARK: identifiers

/// What an identifier refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierKind {
    /// A register; still unnamed.
    Slot,
    /// A named local variable.
    Local,
    /// An upvalue.
    Upvalue,
    /// The `_env` builtin used for global accesses.
    Builtin,
}

/// A variable reference.
#[derive(Debug)]
pub struct Identifier<'a> {
    pub kind: IdentifierKind,
    /// Name, once one is known.
    pub name: Option<&'a str>,
    pub slot: u32,
    /// Identifier of the assignment this value came from, filled in by the
    /// slot handling passes.
    pub id: Option<u32>,
    /// Identifiers a reference may refer to, when the slot handling passes
    /// cannot pin down a single one.
    pub possible_ids: ArenaVec<'a, u32>,
    /// First address the local variable that named this identifier is dead at.
    pub local_end: Option<u32>,
    pub meta: Meta,
}

impl<'a> Identifier<'a> {
    pub fn new(alloc: &'a Allocator, kind: IdentifierKind, slot: u32, meta: Meta) -> Self {
        Identifier {
            kind,
            name: None,
            slot,
            id: None,
            possible_ids: ArenaVec::new_in(&alloc),
            local_end: None,
            meta,
        }
    }

    /// The name to print, falling back to a synthetic one.
    pub fn name_or_slot(&self) -> Cow<'a, str> {
        if let Some(name) = self.name {
            return Cow::Borrowed(name);
        }
        match self.kind {
            IdentifierKind::Upvalue => Cow::Owned(format!("uv{}", self.slot)),
            IdentifierKind::Builtin => Cow::Borrowed("_env"),
            _ => Cow::Owned(format!("slot{}", self.slot)),
        }
    }

    /// Whether this identifier is still an unnamed register.
    pub fn is_slot(&self) -> bool {
        self.kind == IdentifierKind::Slot
    }
}

// MARK: table elements

/// A table indexing expression.
#[derive(Debug)]
pub struct TableElement<'a> {
    pub table: NodeRef<'a>,
    pub key: NodeRef<'a>,
    pub meta: Meta,
}

// MARK: constants

/// The value of a literal constant.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstantValue<'a> {
    /// An integer constant.
    Integer(i32),
    /// A floating point constant.
    Float(f64),
    /// A string constant, as raw bytes.
    String(&'a [u8]),
    /// A `cdata` constant, already rendered.
    CData(&'a [u8]),
}

/// A literal constant.
#[derive(Debug)]
pub struct Constant<'a> {
    pub value: ConstantValue<'a>,
    pub meta: Meta,
}

// MARK: primitives

/// `nil`, `false` or `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveKind {
    Nil,
    True,
    False,
}

/// A primitive value.
#[derive(Debug, Clone, Copy)]
pub struct Primitive {
    pub kind: PrimitiveKind,
}

// MARK: tables

/// A table constructor.
#[derive(Debug)]
pub struct TableConstructor<'a> {
    pub array: NodeRef<'a>,
    pub records: NodeRef<'a>,
    pub meta: Meta,
}

/// An array part entry of a table constructor.
#[derive(Debug)]
pub struct ArrayRecord<'a> {
    pub value: NodeRef<'a>,
    pub meta: Meta,
}

/// A hash part entry of a table constructor.
#[derive(Debug)]
pub struct TableRecord<'a> {
    pub key: NodeRef<'a>,
    pub value: NodeRef<'a>,
    pub meta: Meta,
}

// MARK: operators

/// A binary operator.
#[derive(Debug)]
pub struct BinaryOperator<'a> {
    pub kind: BinaryOperatorKind,
    pub left: NodeRef<'a>,
    pub right: NodeRef<'a>,
    pub meta: Meta,
}

/// Every binary operator the decompiler produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperatorKind {
    LogicalOr,
    LogicalAnd,
    LessThan,
    GreaterThan,
    LessOrEqual,
    GreaterOrEqual,
    NotEqual,
    Equal,
    Concat,
    Add,
    Subtract,
    Multiply,
    Division,
    Mod,
    Pow,
    /// LuaJIT 2.1 bit operators.
    BitOr,
    BitXor,
    BitAnd,
    ShiftLeft,
    ShiftRight,
    ShiftArithmeticRight,
}

impl BinaryOperatorKind {
    /// The precedence class of this operator.
    pub fn precedence(self) -> Precedence {
        use BinaryOperatorKind::*;
        match self {
            LogicalOr => Precedence::Or,
            LogicalAnd => Precedence::And,
            LessThan | GreaterThan | LessOrEqual | GreaterOrEqual | NotEqual | Equal => {
                Precedence::Comparison
            }
            Concat => Precedence::Concatenate,
            Add | Subtract | BitOr | BitXor => Precedence::MathAddSub,
            Multiply | Division | Mod | BitAnd | ShiftLeft | ShiftRight | ShiftArithmeticRight => {
                Precedence::Math
            }
            Pow => Precedence::Exponent,
        }
    }

    /// `^` is the only right associative binary operator LuaJIT emits.
    pub fn is_right_associative(self) -> bool {
        matches!(self, BinaryOperatorKind::Pow)
    }

    /// Whether the operand order of this operator does not matter.
    pub fn is_commutative(self) -> bool {
        use BinaryOperatorKind::*;
        matches!(
            self,
            LogicalOr | LogicalAnd | Equal | NotEqual | Add | Multiply | BitOr | BitXor | BitAnd
        )
    }

    /// The operator as it is written in Lua.
    pub fn as_str(self) -> &'static str {
        use BinaryOperatorKind::*;
        match self {
            LogicalOr => "or",
            LogicalAnd => "and",
            LessThan => "<",
            GreaterThan => ">",
            LessOrEqual => "<=",
            GreaterOrEqual => ">=",
            NotEqual => "~=",
            Equal => "==",
            Concat => "..",
            Add => "+",
            Subtract => "-",
            Multiply => "*",
            Division => "/",
            Mod => "%",
            Pow => "^",
            BitOr => "|",
            BitXor => "~",
            BitAnd => "&",
            ShiftLeft => "<<",
            ShiftRight => ">>",
            ShiftArithmeticRight => "~>>",
        }
    }
}

/// Operator precedence classes, shared between binary and unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precedence {
    Or = 1,
    And = 2,
    Comparison = 3,
    Concatenate = 4,
    MathAddSub = 5,
    Math = 6,
    Unary = 7,
    Exponent = 8,
}

/// A unary operator.
#[derive(Debug)]
pub struct UnaryOperator<'a> {
    pub kind: UnaryOperatorKind,
    pub operand: NodeRef<'a>,
    pub meta: Meta,
}

/// Every unary operator the decompiler produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperatorKind {
    Not,
    Length,
    Minus,
    /// `ISTYPE`, rendered as a call to `tostring`.
    ToString,
    /// `ISNUM`, rendered as a call to `tonumber`.
    ToNumber,
    /// `BNOT`, rendered as `~`.
    BitNot,
}

impl UnaryOperatorKind {
    /// Unary operators bind tighter than every binary operator but `^`.
    pub fn precedence(self) -> Precedence {
        Precedence::Unary
    }

    /// Where the operator sits among the unary operators.
    ///
    /// The order is the one ljd uses, which decides when an operand needs
    /// braces: `-(#x)` keeps them, `#(-x)` does not.
    pub fn rank(self) -> u8 {
        match self {
            UnaryOperatorKind::Not | UnaryOperatorKind::BitNot => 0,
            UnaryOperatorKind::Length => 1,
            UnaryOperatorKind::Minus => 2,
            UnaryOperatorKind::ToString => 3,
            UnaryOperatorKind::ToNumber => 4,
        }
    }

    /// The operator as it is written in Lua.
    pub fn as_str(self) -> &'static str {
        match self {
            UnaryOperatorKind::Not => "not ",
            UnaryOperatorKind::Length => "#",
            UnaryOperatorKind::Minus => "-",
            UnaryOperatorKind::BitNot => "~",
            UnaryOperatorKind::ToString => "tostring",
            UnaryOperatorKind::ToNumber => "tonumber",
        }
    }
}

// MARK: control flow graph

/// A basic block of the control flow graph.
#[derive(Debug)]
pub struct Block<'a> {
    pub index: u32,
    pub first_address: u32,
    pub last_address: u32,
    /// Last address that still produces statements, i.e. the address just
    /// before the warp instructions.
    pub last_body_address: u32,
    /// How many warps target this block.
    pub warpins_count: u32,
    /// Whether the block contains a loop marker instruction.
    pub is_loop: bool,
    pub contents: ArenaVec<'a, NodeRef<'a>>,
    pub warp: Option<NodeRef<'a>>,
}

impl<'a> Block<'a> {
    pub fn new(alloc: &'a Allocator, index: u32, first_address: u32, last_address: u32) -> Self {
        Block {
            index,
            first_address,
            last_address,
            last_body_address: last_address,
            warpins_count: 0,
            is_loop: false,
            contents: ArenaVec::new_in(&alloc),
            warp: None,
        }
    }
}

/// Whether an unconditional warp jumps or simply flows into the next block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnconditionalWarpKind {
    Jump,
    Flow,
}

/// A no-op, used as a placeholder for empty blocks.
#[derive(Debug)]
pub struct NoOp {
    pub meta: Meta,
}

/// The end of a function.
#[derive(Debug)]
pub struct EndWarp<'a> {
    /// Where control would have gone, if the block that ends here had a warp
    /// that pointed somewhere. Keeping it lets the passes look through a
    /// region that has already been closed off.
    pub target: Option<NodeRef<'a>>,
    pub meta: Meta,
}

/// An unconditional jump or a fallthrough.
#[derive(Debug)]
pub struct UnconditionalWarp<'a> {
    pub kind: UnconditionalWarpKind,
    pub target: Option<NodeRef<'a>>,
    pub is_uclo: bool,
    pub meta: Meta,
}

/// A conditional branch.
#[derive(Debug)]
pub struct ConditionalWarp<'a> {
    pub condition: Option<NodeRef<'a>>,
    pub true_target: Option<NodeRef<'a>>,
    pub false_target: Option<NodeRef<'a>>,
    /// Register the condition was computed into.
    ///
    /// A comparison leaves the condition in no register at all, so this is
    /// `None` for those; the passes use the distinction to tell a condition
    /// that was materialised into a slot from one that was not.
    pub slot: Option<u32>,
    pub meta: Meta,
}

/// A generic `for ... in` loop head.
#[derive(Debug)]
pub struct IteratorWarp<'a> {
    pub variables: NodeRef<'a>,
    pub controls: NodeRef<'a>,
    pub body: Option<NodeRef<'a>>,
    pub way_out: Option<NodeRef<'a>>,
    pub meta: Meta,
}

/// A numeric `for` loop head.
#[derive(Debug)]
pub struct NumericLoopWarp<'a> {
    pub index: NodeRef<'a>,
    pub controls: NodeRef<'a>,
    pub body: Option<NodeRef<'a>>,
    pub way_out: Option<NodeRef<'a>>,
    pub meta: Meta,
}
