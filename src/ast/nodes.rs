//! The decompiler's intermediate representation.
//!
//! The representation mirrors ljd's AST: every node is reference counted so
//! that passes can hold on to a node, compare identities and rewrite nodes in
//! place, exactly like the Python implementation does with its objects.
//!
//! Child nodes are always [`NodeRef`]s, including lists: a statement list is a
//! node of its own so that a pass can find it and rewrite its contents.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::bytecode::DebugInfo;

/// A shared, mutable AST node.
pub type NodeRef = Rc<RefCell<Node>>;

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
}

impl Meta {
    pub fn new(addr: u32, line: u32) -> Self {
        Meta {
            addr,
            line,
            error_here: false,
        }
    }
}

/// Wraps a node into a reference counted node.
pub fn node(node: Node) -> NodeRef {
    Rc::new(RefCell::new(node))
}

/// Creates a statement list node.
pub fn statements(contents: Vec<NodeRef>) -> NodeRef {
    node(Node::Statements(contents))
}

/// Creates an expression list node.
pub fn expressions(contents: Vec<NodeRef>) -> NodeRef {
    node(Node::Expressions(contents))
}

/// Creates a variable list node.
pub fn variables(contents: Vec<NodeRef>) -> NodeRef {
    node(Node::Variables(contents))
}

/// Creates an identifier list node.
pub fn identifiers(contents: Vec<NodeRef>) -> NodeRef {
    node(Node::Identifiers(contents))
}

/// Creates a record list node.
pub fn records(contents: Vec<NodeRef>) -> NodeRef {
    node(Node::Records(contents))
}

/// Creates a `nil`, `true` or `false` node.
pub fn primitive(kind: PrimitiveKind) -> NodeRef {
    node(Node::Primitive(Primitive { kind }))
}

/// Every kind of node the decompiler knows about.
#[derive(Debug)]
pub enum Node {
    // -- lists -------------------------------------------------------------
    /// A list of statements.
    Statements(Vec<NodeRef>),
    /// A list of expressions.
    Expressions(Vec<NodeRef>),
    /// A list of assignable variables.
    Variables(Vec<NodeRef>),
    /// A list of identifiers.
    Identifiers(Vec<NodeRef>),
    /// A list of table constructor records.
    Records(Vec<NodeRef>),

    // -- statements --------------------------------------------------------
    /// An assignment; `kind` distinguishes `local x = 1` from `x = 1`.
    Assignment(Box<Assignment>),
    /// A function call used as a statement.
    FunctionCall(Box<FunctionCall>),
    /// A `return` statement.
    Return(Box<Return>),
    /// A `break` statement.
    Break,
    /// A no-op, used as a placeholder for empty blocks.
    NoOp(Box<NoOp>),
    /// An `if` statement.
    If(Box<If>),
    /// A `while` loop.
    While(Box<While>),
    /// A `repeat ... until` loop.
    RepeatUntil(Box<RepeatUntil>),
    /// A numeric `for` loop.
    NumericFor(Box<NumericFor>),
    /// A generic `for ... in` loop.
    IteratorFor(Box<IteratorFor>),
    /// A `function ... end` definition, as a statement or as an expression.
    FunctionDefinition(Box<FunctionDefinition>),
    /// An `elseif` branch.
    ElseIf(Box<ElseIf>),

    // -- expressions -------------------------------------------------------
    /// A variable reference.
    Identifier(Box<Identifier>),
    /// A table indexing expression.
    TableElement(Box<TableElement>),
    /// A literal constant.
    Constant(Box<Constant>),
    /// `nil`, `true` or `false`.
    Primitive(Primitive),
    /// A table constructor.
    TableConstructor(Box<TableConstructor>),
    /// `...`
    Vararg,
    /// The result of a previous multi result call.
    MulTres,
    /// A binary operator.
    BinaryOperator(Box<BinaryOperator>),
    /// A unary operator.
    UnaryOperator(Box<UnaryOperator>),
    /// An array element inside a table constructor.
    ArrayRecord(Box<ArrayRecord>),
    /// A key/value pair inside a table constructor.
    TableRecord(Box<TableRecord>),

    // -- control flow graph ------------------------------------------------
    /// A basic block. Only present before unwarping.
    Block(Box<Block>),
    /// An unconditional jump or fallthrough.
    UnconditionalWarp(Box<UnconditionalWarp>),
    /// A conditional branch.
    ConditionalWarp(Box<ConditionalWarp>),
    /// A generic `for ... in` loop head.
    IteratorWarp(Box<IteratorWarp>),
    /// A numeric `for` loop head.
    NumericLoopWarp(Box<NumericLoopWarp>),
    /// The end of a function.
    EndWarp(Box<EndWarp>),
}

impl Node {
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
    pub fn list(&self) -> Option<&Vec<NodeRef>> {
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
    pub fn list_mut(&mut self) -> Option<&mut Vec<NodeRef>> {
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
    pub fn identifier(&self) -> Option<&Identifier> {
        match self {
            Node::Identifier(identifier) => Some(identifier),
            _ => None,
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Identifier(identifier) => f.write_str(&identifier.name_or_slot()),
            _ => f.write_str(self.kind()),
        }
    }
}

/// Reads the contents of a list node.
pub fn list_contents(node: &NodeRef) -> Vec<NodeRef> {
    node.borrow().list().cloned().unwrap_or_default()
}
/// Replaces the contents of a list node.
pub fn set_list_contents(node: &NodeRef, contents: Vec<NodeRef>) {
    if let Some(items) = node.borrow_mut().list_mut() {
        *items = contents;
    }
}

/// Appends to a list node.
pub fn push(node: &NodeRef, child: NodeRef) {
    if let Some(items) = node.borrow_mut().list_mut() {
        items.push(child);
    }
}

/// Replaces the metadata of a node.
///
/// Nodes that carry no metadata of their own are left alone.
pub fn set_meta(node: &NodeRef, meta: Meta) {
    if let Some(target) = node.borrow_mut().meta_mut() {
        *target = meta;
    }
}

/// Records that a pass gave up on `node`.
pub fn mark_error(node: &NodeRef) {
    if let Some(meta) = node.borrow_mut().meta_mut() {
        meta.error_here = true;
    }
}

/// Whether a pass gave up on `node`.
pub fn has_error(node: &NodeRef) -> bool {
    node.borrow().meta().is_some_and(|meta| meta.error_here)
}

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
pub struct Assignment {
    pub expressions: NodeRef,
    pub destinations: NodeRef,
    pub kind: AssignmentKind,
    pub meta: Meta,
}

/// A function call.
#[derive(Debug)]
pub struct FunctionCall {
    pub function: NodeRef,
    pub arguments: NodeRef,
    pub is_method: bool,
    pub meta: Meta,
}

/// A `return` statement.
#[derive(Debug)]
pub struct Return {
    pub returns: NodeRef,
    pub meta: Meta,
}

/// An `if` statement.
#[derive(Debug)]
pub struct If {
    pub expression: NodeRef,
    pub then_block: NodeRef,
    pub elseifs: Vec<NodeRef>,
    pub else_block: NodeRef,
    pub meta: Meta,
}

/// An `elseif` branch.
#[derive(Debug)]
pub struct ElseIf {
    pub expression: NodeRef,
    pub then_block: NodeRef,
    pub meta: Meta,
}

/// A `while` loop.
#[derive(Debug)]
pub struct While {
    pub expression: NodeRef,
    pub statements: NodeRef,
    pub meta: Meta,
}

/// A `repeat ... until` loop.
#[derive(Debug)]
pub struct RepeatUntil {
    pub expression: NodeRef,
    pub statements: NodeRef,
    pub meta: Meta,
}

/// A numeric `for` loop.
#[derive(Debug)]
pub struct NumericFor {
    pub variable: NodeRef,
    pub expressions: NodeRef,
    pub statements: NodeRef,
    pub meta: Meta,
}

/// A generic `for ... in` loop.
#[derive(Debug)]
pub struct IteratorFor {
    pub identifiers: NodeRef,
    pub expressions: NodeRef,
    pub statements: NodeRef,
    pub meta: Meta,
}

/// A function definition.
#[derive(Debug)]
pub struct FunctionDefinition {
    pub arguments: NodeRef,
    pub statements: NodeRef,
    /// Raw upvalue references of the prototype.
    pub upvalues: Vec<u16>,
    /// Debug information of the prototype.
    pub debug: Rc<DebugInfo>,
    /// Number of instructions, header included.
    pub instruction_count: usize,
    pub meta: Meta,
}

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
pub struct Identifier {
    pub kind: IdentifierKind,
    /// Name, once one is known.
    pub name: Option<String>,
    pub slot: u32,
    /// Identifier of the assignment this value came from, filled in by the
    /// slot handling passes.
    pub id: Option<u32>,
    /// Identifiers a reference may refer to, when the slot handling passes
    /// cannot pin down a single one.
    pub possible_ids: Vec<u32>,
    /// First address the local variable that named this identifier is dead at.
    pub local_end: Option<u32>,
    pub meta: Meta,
}

impl Identifier {
    pub fn new(kind: IdentifierKind, slot: u32, meta: Meta) -> Self {
        Identifier {
            kind,
            name: None,
            slot,
            id: None,
            possible_ids: Vec::new(),
            local_end: None,
            meta,
        }
    }

    /// The name to print, falling back to a synthetic one.
    pub fn name_or_slot(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        match self.kind {
            IdentifierKind::Upvalue => format!("uv{}", self.slot),
            IdentifierKind::Builtin => "_env".to_string(),
            _ => format!("slot{}", self.slot),
        }
    }

    /// Whether this identifier is still an unnamed register.
    pub fn is_slot(&self) -> bool {
        self.kind == IdentifierKind::Slot
    }
}

/// A table indexing expression.
#[derive(Debug)]
pub struct TableElement {
    pub table: NodeRef,
    pub key: NodeRef,
    pub meta: Meta,
}

/// The value of a literal constant.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstantValue {
    /// An integer constant.
    Integer(i32),
    /// A floating point constant.
    Float(f64),
    /// A string constant, as raw bytes.
    String(Box<[u8]>),
    /// A `cdata` constant, already rendered.
    CData(Box<[u8]>),
}

/// A literal constant.
#[derive(Debug)]
pub struct Constant {
    pub value: ConstantValue,
    pub meta: Meta,
}

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

/// A table constructor.
#[derive(Debug)]
pub struct TableConstructor {
    pub array: NodeRef,
    pub records: NodeRef,
    pub meta: Meta,
}

/// An array part entry of a table constructor.
#[derive(Debug)]
pub struct ArrayRecord {
    pub value: NodeRef,
    pub meta: Meta,
}

/// A hash part entry of a table constructor.
#[derive(Debug)]
pub struct TableRecord {
    pub key: NodeRef,
    pub value: NodeRef,
    pub meta: Meta,
}

/// A binary operator.
#[derive(Debug)]
pub struct BinaryOperator {
    pub kind: BinaryOperatorKind,
    pub left: NodeRef,
    pub right: NodeRef,
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
pub struct UnaryOperator {
    pub kind: UnaryOperatorKind,
    pub operand: NodeRef,
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

/// A basic block of the control flow graph.
#[derive(Debug)]
pub struct Block {
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
    pub contents: Vec<NodeRef>,
    pub warp: Option<NodeRef>,
}

impl Block {
    pub fn new(index: u32, first_address: u32, last_address: u32) -> Self {
        Block {
            index,
            first_address,
            last_address,
            last_body_address: last_address,
            warpins_count: 0,
            is_loop: false,
            contents: Vec::new(),
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
pub struct EndWarp {
    /// Where control would have gone, if the block that ends here had a warp
    /// that pointed somewhere. Keeping it lets the passes look through a
    /// region that has already been closed off.
    pub target: Option<NodeRef>,
    pub meta: Meta,
}

/// An unconditional jump or a fallthrough.
#[derive(Debug)]
pub struct UnconditionalWarp {
    pub kind: UnconditionalWarpKind,
    pub target: Option<NodeRef>,
    pub is_uclo: bool,
    pub meta: Meta,
}

/// A conditional branch.
#[derive(Debug)]
pub struct ConditionalWarp {
    pub condition: Option<NodeRef>,
    pub true_target: Option<NodeRef>,
    pub false_target: Option<NodeRef>,
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
pub struct IteratorWarp {
    pub variables: NodeRef,
    pub controls: NodeRef,
    pub body: Option<NodeRef>,
    pub way_out: Option<NodeRef>,
    pub meta: Meta,
}

/// A numeric `for` loop head.
#[derive(Debug)]
pub struct NumericLoopWarp {
    pub index: NodeRef,
    pub controls: NodeRef,
    pub body: Option<NodeRef>,
    pub way_out: Option<NodeRef>,
    pub meta: Meta,
}
