//! The node model.
//!
//! The [`Node`] enum, every payload it can hold, and the methods those types
//! own. This is what the rest of the decompiler builds and rewrites; reading a
//! node from the outside lives in [`super::accessors`], what reads and replaces
//! a node's list or block contents lives in [`super::traverse`], and the traits
//! that put those methods on a node are gathered in [`super::ext`].
//!
//! The free functions of those two modules are re-exported here, so that
//! `use crate::ast::nodes::*` still brings in the whole model along with the few
//! helpers that are used as functions rather than as methods.

use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use crate::bytecode::DebugInfo;

// MARK: meta

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

// MARK: node

/// Every kind of node the decompiler knows about.
#[derive(Debug)]
pub enum Node<'a> {
    // MARK: node - list
    /// A list of statements.
    Statements(ArenaVec<'a, ArenaNode<'a>>),
    /// A list of expressions.
    Expressions(ArenaVec<'a, ArenaNode<'a>>),
    /// A list of assignable variables.
    Variables(ArenaVec<'a, ArenaNode<'a>>),
    /// A list of identifiers.
    Identifiers(ArenaVec<'a, ArenaNode<'a>>),
    /// A list of table constructor records.
    Records(ArenaVec<'a, ArenaNode<'a>>),

    // MARK: node - statement
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

    // MARK: node - expression
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

    // MARK: node - control flow
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

impl<'a> Node<'a> {
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

    /// Replaces the metadata of this node.
    ///
    /// Nodes that carry no metadata of their own are left alone.
    pub fn set_meta(&mut self, meta: Meta) {
        if let Some(target) = self.meta_mut() {
            *target = meta;
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

// MARK: arena wrapper

/// A shared, mutable AST node on the arena.
///
/// Nodes are allocated in an arena and never freed individually, so a plain
/// shared reference to a [`RefCell`] is all that is needed: identity is the
/// address of the node, and the borrow flag is what keeps the passes honest.
#[derive(Clone, Copy)]
pub struct ArenaNode<'a> {
    inner: &'a RefCell<Node<'a>>,
}

impl fmt::Debug for ArenaNode<'_> {
    /// Names the node rather than printing it.
    ///
    /// The graph is cyclic — a block's warp points at another block — so
    /// recursing into the payload would not terminate. The key is the stable
    /// name of the node, which is what a debugging pass wants anyway, and
    /// printing it also keeps a `{:?}` of a whole tree from taking the stack
    /// down with it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ArenaNode({:#x})", self.key())
    }
}

/// Whether two wrappers are the same node.
///
/// This is the same question
/// [`same_as`](crate::ast::accessors::NodeAccessorExt::same_as) asks, written
/// the way a [`HashSet`](std::collections::HashSet) needs it. It is deliberately
/// *not* derived: a derived `PartialEq` would compare the payloads, which would
/// read through the borrow flag and recurse into a cyclic graph.
impl PartialEq for ArenaNode<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for ArenaNode<'_> {}

/// Hashes the node's identity, not its contents.
///
/// Written by hand for the same reason as [`PartialEq`]: a derived `Hash` would
/// walk the payload, and a node that appears in its own subtree — every loop in
/// the control flow graph — would not terminate.
impl Hash for ArenaNode<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl<'a> ArenaNode<'a> {
    /// The address of a node, which is what identity means in this tree.
    ///
    /// The arena never moves what it has handed out, so the address of a node is
    /// a stable name for it, just like the pointer `Rc` used to expose.
    pub fn key(&self) -> usize {
        self.inner.as_ptr().addr()
    }

    pub fn borrow(&self) -> std::cell::Ref<'_, Node<'a>> {
        self.inner.borrow()
    }

    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, Node<'a>> {
        self.inner.borrow_mut()
    }
}

// MARK: constructors

impl<'a> ArenaNode<'a> {
    /// Wraps a node into an arena allocated node.
    pub fn new(alloc: &'a Allocator, node: Node<'a>) -> Self {
        let inner = alloc.alloc(RefCell::new(node));
        Self { inner }
    }

    /// Creates a statement list node.
    pub fn new_statements(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = ArenaNode<'a>>,
    ) -> Self {
        let node = Node::Statements(ArenaVec::from_iter_in(contents, &alloc));
        Self::new(alloc, node)
    }

    /// Creates an expression list node.
    pub fn new_expressions(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = ArenaNode<'a>>,
    ) -> Self {
        let node = Node::Expressions(ArenaVec::from_iter_in(contents, &alloc));
        Self::new(alloc, node)
    }

    /// Creates a variable list node.
    pub fn new_variables(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = ArenaNode<'a>>,
    ) -> Self {
        let node = Node::Variables(ArenaVec::from_iter_in(contents, &alloc));
        Self::new(alloc, node)
    }

    /// Creates an identifier list node.
    pub fn new_identifiers(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = ArenaNode<'a>>,
    ) -> Self {
        let node = Node::Identifiers(ArenaVec::from_iter_in(contents, &alloc));
        Self::new(alloc, node)
    }

    /// Creates a record list node.
    pub fn new_records(
        alloc: &'a Allocator,
        contents: impl IntoIterator<Item = ArenaNode<'a>>,
    ) -> Self {
        let node = Node::Records(ArenaVec::from_iter_in(contents, &alloc));
        Self::new(alloc, node)
    }

    /// Creates a `nil`, `true` or `false` node.
    pub fn new_primitive(alloc: &'a Allocator, kind: PrimitiveKind) -> Self {
        let node = Node::Primitive(Primitive { kind });
        Self::new(alloc, node)
    }
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
    pub expressions: ArenaNode<'a>,
    pub destinations: ArenaNode<'a>,
    pub kind: AssignmentKind,
    pub meta: Meta,
}

/// A function call.
#[derive(Debug)]
pub struct FunctionCall<'a> {
    pub function: ArenaNode<'a>,
    pub arguments: ArenaNode<'a>,
    pub is_method: bool,
    pub meta: Meta,
}

/// A `return` statement.
#[derive(Debug)]
pub struct Return<'a> {
    pub returns: ArenaNode<'a>,
    pub meta: Meta,
}

/// An `if` statement.
#[derive(Debug)]
pub struct If<'a> {
    pub expression: ArenaNode<'a>,
    pub then_block: ArenaNode<'a>,
    pub elseifs: ArenaVec<'a, ArenaNode<'a>>,
    pub else_block: ArenaNode<'a>,
    pub meta: Meta,
}

/// An `elseif` branch.
#[derive(Debug)]
pub struct ElseIf<'a> {
    pub expression: ArenaNode<'a>,
    pub then_block: ArenaNode<'a>,
    pub meta: Meta,
}

/// A `while` loop.
#[derive(Debug)]
pub struct While<'a> {
    pub expression: ArenaNode<'a>,
    pub statements: ArenaNode<'a>,
    pub meta: Meta,
}

/// A `repeat ... until` loop.
#[derive(Debug)]
pub struct RepeatUntil<'a> {
    pub expression: ArenaNode<'a>,
    pub statements: ArenaNode<'a>,
    pub meta: Meta,
}

/// A numeric `for` loop.
#[derive(Debug)]
pub struct NumericFor<'a> {
    pub variable: ArenaNode<'a>,
    pub expressions: ArenaNode<'a>,
    pub statements: ArenaNode<'a>,
    pub meta: Meta,
}

/// A generic `for ... in` loop.
#[derive(Debug)]
pub struct IteratorFor<'a> {
    pub identifiers: ArenaNode<'a>,
    pub expressions: ArenaNode<'a>,
    pub statements: ArenaNode<'a>,
    pub meta: Meta,
}

/// A function definition.
#[derive(Debug)]
pub struct FunctionDefinition<'a> {
    pub arguments: ArenaNode<'a>,
    pub statements: ArenaNode<'a>,
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
    pub table: ArenaNode<'a>,
    pub key: ArenaNode<'a>,
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
    pub array: ArenaNode<'a>,
    pub records: ArenaNode<'a>,
    pub meta: Meta,
}

/// An array part entry of a table constructor.
#[derive(Debug)]
pub struct ArrayRecord<'a> {
    pub value: ArenaNode<'a>,
    pub meta: Meta,
}

/// A hash part entry of a table constructor.
#[derive(Debug)]
pub struct TableRecord<'a> {
    pub key: ArenaNode<'a>,
    pub value: ArenaNode<'a>,
    pub meta: Meta,
}

// MARK: operators

/// A binary operator.
#[derive(Debug)]
pub struct BinaryOperator<'a> {
    pub kind: BinaryOperatorKind,
    pub left: ArenaNode<'a>,
    pub right: ArenaNode<'a>,
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
    pub operand: ArenaNode<'a>,
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
    pub contents: ArenaVec<'a, ArenaNode<'a>>,
    pub warp: Option<ArenaNode<'a>>,
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
    pub target: Option<ArenaNode<'a>>,
    pub meta: Meta,
}

/// An unconditional jump or a fallthrough.
#[derive(Debug)]
pub struct UnconditionalWarp<'a> {
    pub kind: UnconditionalWarpKind,
    pub target: Option<ArenaNode<'a>>,
    pub is_uclo: bool,
    pub meta: Meta,
}

/// A conditional branch.
#[derive(Debug)]
pub struct ConditionalWarp<'a> {
    pub condition: Option<ArenaNode<'a>>,
    pub true_target: Option<ArenaNode<'a>>,
    pub false_target: Option<ArenaNode<'a>>,
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
    pub variables: ArenaNode<'a>,
    pub controls: ArenaNode<'a>,
    pub body: Option<ArenaNode<'a>>,
    pub way_out: Option<ArenaNode<'a>>,
    pub meta: Meta,
}

/// A numeric `for` loop head.
#[derive(Debug)]
pub struct NumericLoopWarp<'a> {
    pub index: ArenaNode<'a>,
    pub controls: ArenaNode<'a>,
    pub body: Option<ArenaNode<'a>>,
    pub way_out: Option<ArenaNode<'a>>,
    pub meta: Meta,
}

// MARK: conversion

/// A value that may hold a node.
///
/// This is what lets one predicate be asked of a node, of a reference to one, or
/// of an `Option` of either, so that a call site reads the same whichever of the
/// four it happens to have. It is the bound of the `Node*Ext` traits.
///
/// The node comes back behind a [`Deref`]: a [`Node`] is handed over as itself,
/// while an [`ArenaNode`] borrows the [`RefCell`](std::cell::RefCell) it points
/// at and hands over the [`Ref`](std::cell::Ref) that keeps the borrow alive.
pub trait AsNode<'a> {
    /// The node, if there is one.
    fn as_node(&self) -> Option<impl Deref<Target = Node<'a>>>;

    /// Identity of the node, if there is one.
    ///
    /// This is the one answer to "which node is this": the address of the node
    /// itself, the same number [`ArenaNode::key`] reports.
    /// [`same_as`](crate::ast::accessors::NodeAccessorExt::same_as) compares
    /// these, and the passes key their sets on them, so an implementation that
    /// named the wrapper rather than the node would quietly make every set hold
    /// a different number for one node.
    ///
    /// Taking it never borrows the cell: a pass may ask whether two nodes are
    /// the same while it holds a mutable borrow of one of them, which a
    /// borrow-based answer would turn into a panic. The [`ArenaNode`] forms
    /// therefore override this instead of going through [`as_node`](Self::as_node).
    fn as_key(&self) -> Option<usize> {
        self.as_node().map(|node| std::ptr::from_ref(&*node).addr())
    }
}

impl<'a> AsNode<'a> for Node<'a> {
    fn as_node(&self) -> Option<impl Deref<Target = Node<'a>>> {
        Some(self)
    }
}

impl<'a> AsNode<'a> for Option<Node<'a>> {
    fn as_node(&self) -> Option<impl Deref<Target = Node<'a>>> {
        self.as_ref()
    }
}

impl<'a> AsNode<'a> for Option<&Node<'a>> {
    fn as_node(&self) -> Option<impl Deref<Target = Node<'a>>> {
        *self
    }
}

impl<'a> AsNode<'a> for ArenaNode<'a> {
    fn as_node(&self) -> Option<impl Deref<Target = Node<'a>>> {
        Some(self.borrow())
    }

    fn as_key(&self) -> Option<usize> {
        Some(self.key())
    }
}

impl<'a> AsNode<'a> for Option<ArenaNode<'a>> {
    fn as_node(&self) -> Option<impl Deref<Target = Node<'a>>> {
        Some(self.as_ref()?.borrow())
    }

    fn as_key(&self) -> Option<usize> {
        self.map(|node| node.key())
    }
}

impl<'a> AsNode<'a> for Option<&ArenaNode<'a>> {
    fn as_node(&self) -> Option<impl Deref<Target = Node<'a>>> {
        Some(self.as_ref()?.borrow())
    }

    fn as_key(&self) -> Option<usize> {
        self.map(|node| node.key())
    }
}

// MARK: tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::accessors::NodeAccessorExt;

    #[test]
    fn identity_names_the_node_rather_than_the_wrapper() {
        let alloc = &Allocator::new();
        let node = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let other = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);

        // `key` and `same_as` have to agree: when one of them named the wrapper
        // instead of the node, every identity-keyed set in the passes held a
        // different number for one node and the output came out garbled with
        // no error anywhere.
        assert_eq!(node.as_key(), Some(node.key()));
        assert_ne!(node.key(), other.key());
        assert!(node.same_as(node));
        assert!(!node.same_as(other));

        // A copy of the wrapper is the same node, because it is the same cell.
        let copy = node;
        assert_eq!(copy.key(), node.key());
        assert!(node.same_as(copy));

        // The `Option` forms answer the same way, which is what the graph
        // passes actually hold.
        let absent: Option<ArenaNode<'_>> = None;
        assert!(Some(node).same_as(Some(copy)));
        assert!(!Some(node).same_as(Some(other)));
        assert!(!Some(node).same_as(absent));
        assert!(Some(node).same_optional_as(Some(copy)));
        assert!(!node.same_optional_as(Some(other)));
    }

    #[test]
    fn identity_can_be_asked_while_the_node_is_mutably_borrowed() {
        let alloc = &Allocator::new();
        let node = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);

        // Identity must not borrow: a pass asks whether a node is the one it
        // wants while it is in the middle of rewriting it, and a borrow-based
        // answer would panic there instead. A plain borrow really does panic
        // with this one alive, which is what makes the two lines below mean
        // something.
        let held = node.borrow_mut();
        let plain_borrow = std::panic::AssertUnwindSafe(|| {
            let _ = node.borrow();
        });
        assert!(
            std::panic::catch_unwind(plain_borrow).is_err(),
            "a borrow while the node is mutably borrowed has to panic"
        );
        assert!(node.same_as(node));
        assert!(node.same_optional_as(Some(node)));
        assert_eq!(node.as_key(), Some(node.key()));
        drop(held);
    }

    #[test]
    fn equality_and_hashing_are_identity() {
        use std::collections::HashSet;

        let alloc = &Allocator::new();
        let node = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        let other = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);

        // Two wrappers of one node are one node, whatever path they came by,
        // and two identical-looking nodes are still two nodes.
        let copy = node;
        assert_eq!(node, copy);
        assert_ne!(node, other);
        assert!(node != other);

        // Equality and hashing have to agree, or a set would keep two entries
        // for one node and never find the one it stored.
        let mut seen = HashSet::new();
        assert!(seen.insert(node));
        assert!(!seen.insert(copy), "the same node must not be stored twice");
        assert!(seen.insert(other));
        assert_eq!(seen.len(), 2);
        assert!(seen.contains(&copy));

        // A table the node is keyed in behaves the same way.
        let mut names = std::collections::HashMap::new();
        names.insert(node, "first");
        names.insert(other, "second");
        assert_eq!(names.get(&copy), Some(&"first"));
        assert_eq!(names.len(), 2);

        // Identity, not contents: nodes that would compare equal by their
        // payload are still distinct keys.
        let nil_again = ArenaNode::new_primitive(alloc, PrimitiveKind::Nil);
        assert_ne!(node, nil_again);
    }
}
