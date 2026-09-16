//! Turns a finished AST back into Lua source.
//!
//! This is a port of ljd's `lua/writer.py`. The tree is walked once to build a
//! queue of printing commands, and the queue is then turned into text. Keeping
//! the two apart means the tree walk only decides *what* is printed and the
//! second pass only decides *how*: indentation, line breaks and the blank lines
//! between statements all live in the queue processing step.
//!
//! Three things are worth knowing when reading the walk:
//!
//! * A node can be reached "too early" and marked as skipped, so that it is
//!   printed from wherever it belongs instead. The array and record lists of a
//!   table constructor are the usual case: they are printed as one list, in
//!   order, so both are skipped where they sit in the tree.
//! * The trailing `return` of a function is left out, because Lua does not need
//!   it.
//! * `t.f = function() end` is written as `function t.f() end` when the
//!   destination is simple enough for that, which is what makes methods come
//!   out as `function Mod:name()`.

use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::nodes::*;
use crate::ast::traverse;
use crate::bytecode::{SLOT_FALSE, SLOT_TRUE};
use crate::error::{Error, Result};

/// How the output is indented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Indent {
    /// One tab per level.
    #[default]
    Tabs,
    /// A number of spaces per level.
    Spaces(u8),
}

impl Indent {
    /// The text one level of indentation is made of.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Indent::Tabs => "\t",
            // Only the common widths get their own string; anything else falls
            // back to four spaces, which is the usual choice for Lua sources.
            Indent::Spaces(1) => " ",
            Indent::Spaces(2) => "  ",
            Indent::Spaces(3) => "   ",
            Indent::Spaces(8) => "        ",
            Indent::Spaces(_) => "    ",
        }
    }
}

/// How bit operations are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitOpStyle {
    /// The operators LuaJIT understands, like `a & b`.
    #[default]
    Operator,
    /// Calls into the `bit` library, like `bit.band(a, b)`.
    BitLibrary,
}

/// How the writer formats its output.
#[derive(Debug, Clone)]
pub struct Options {
    /// How statements are indented.
    pub indent: Indent,
    /// Whether a table constructor with one entry stays on a single line.
    pub compact_table_constructors: bool,
    /// Whether an empty block is marked with a `-- Nothing` comment.
    pub comment_empty_blocks: bool,
    /// Whether registers that were never named carry the ids of the
    /// definitions they might refer to.
    pub show_slot_ids: bool,
    /// Whether `t.f = function() end` becomes `function t.f() end`.
    pub function_definition_sugar: bool,
    /// Whether the `self` argument of a method is written out.
    pub write_function_definition_self_arg: bool,
    /// How bit operations are written.
    pub bitop_style: BitOpStyle,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            indent: Indent::Tabs,
            compact_table_constructors: false,
            comment_empty_blocks: true,
            show_slot_ids: false,
            function_definition_sugar: false,
            write_function_definition_self_arg: false,
            bitop_style: BitOpStyle::Operator,
        }
    }
}

/// Renders a function definition and everything inside it.
///
/// The header of the function itself is not written: a chunk is written as the
/// statements of its main function, and a nested function is written where the
/// assignment that defines it is.
pub fn write_function(root: &NodeRef, options: &Options) -> Result<String> {
    let statements = match &*root.borrow() {
        Node::FunctionDefinition(inner) => inner.statements.clone(),
        _ => {
            return Err(Error::Internal(
                "the writer needs a function definition".to_string(),
            ));
        }
    };

    let mut writer = Writer::new(options);
    writer.visit_node(&statements);

    Ok(process_queue(&writer.queue, options))
}

/// What a statement is, for the blank line rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Statement {
    None = -1,
    Assignment = 0,
    FunctionCall = 1,
    Return = 2,
    Break = 3,
    If = 4,
    IteratorFor = 5,
    NumericFor = 6,
    RepeatUntil = 7,
    While = 8,
    Function = 9,
}

impl Statement {
    /// Whether the statement owns the lines it covers.
    fn is_block(self) -> bool {
        self as i32 >= Statement::If as i32
    }
}

enum Command {
    StartStatement(Statement),
    EndStatement(Statement),
    EndLine,
    StartBlock,
    EndBlock,
    Write(String),
}

/// The part of the writer's state that belongs to one function.
struct State {
    current_statement: Statement,
    function_name: Option<NodeRef>,
    function_local: bool,
    function_method: bool,
}

impl State {
    fn new() -> Self {
        State {
            current_statement: Statement::None,
            function_name: None,
            function_local: false,
            function_method: false,
        }
    }
}

struct Writer<'a> {
    options: &'a Options,
    queue: Vec<Command>,
    path: Vec<NodeRef>,
    skipped: Vec<HashSet<usize>>,
    states: Vec<State>,
}

fn node_key(node: &NodeRef) -> usize {
    Rc::as_ptr(node) as usize
}

impl<'a> Writer<'a> {
    fn new(options: &'a Options) -> Self {
        Writer {
            options,
            queue: Vec::new(),
            path: Vec::new(),
            skipped: vec![HashSet::new()],
            states: vec![State::new()],
        }
    }

    // -- the printing queue ------------------------------------------------

    fn start_statement(&mut self, statement: Statement) {
        self.state().current_statement = statement;
        self.queue.push(Command::StartStatement(statement));
    }

    fn end_statement(&mut self, statement: Statement) {
        self.state().current_statement = Statement::None;
        self.queue.push(Command::EndStatement(statement));
    }

    fn end_line(&mut self) {
        self.queue.push(Command::EndLine);
    }

    fn start_block(&mut self) {
        self.queue.push(Command::StartBlock);
    }

    fn end_block(&mut self) {
        self.queue.push(Command::EndBlock);
    }

    fn write(&mut self, text: impl Into<String>) {
        self.queue.push(Command::Write(text.into()));
    }

    fn state(&mut self) -> &mut State {
        self.states
            .last_mut()
            .expect("the state stack is never empty")
    }

    /// Marks a node as printed, so it is not printed again from elsewhere.
    fn skip(&mut self, node: &NodeRef) {
        self.skipped
            .last_mut()
            .expect("the skip stack is never empty")
            .insert(node_key(node));
    }

    /// Writes the text of a constant that stands for a name.
    fn write_name(&mut self, key: &NodeRef) {
        if let Node::Constant(constant) = &*key.borrow()
            && let ConstantValue::String(text) = &constant.value
        {
            let text = decode_string(text);
            self.write(text);
            return;
        }

        self.visit_node(key);
    }

    // -- statements --------------------------------------------------------

    fn visit_function_definition(&mut self, node: &NodeRef) {
        let (arguments, statements) = {
            let borrowed = node.borrow();
            let Node::FunctionDefinition(inner) = &*borrowed else {
                return;
            };
            (inner.arguments.clone(), inner.statements.clone())
        };

        let is_statement = self.state().function_name.is_some();
        let is_method = self.state().function_method;

        if is_statement {
            self.start_statement(Statement::Function);

            if self.state().function_local {
                self.write("local ");
            }
            self.write("function ");

            let name = self
                .state()
                .function_name
                .clone()
                .expect("checked by is_statement");

            if is_method && !self.options.write_function_definition_self_arg {
                let element = match &*name.borrow() {
                    Node::TableElement(element) => {
                        Some((element.table.clone(), element.key.clone()))
                    }
                    _ => None,
                };

                match element {
                    Some((table, key)) => {
                        self.visit_node(&table);
                        self.write(":");
                        self.write_name(&key);
                    }
                    None => self.visit_node(&name),
                }
            } else {
                self.visit_node(&name);
            }

            self.write("(");
            self.state().function_name = None;
        } else {
            self.write("function (");
        }

        let mut arguments = traverse::list_contents(&arguments);

        // A method receives its `self` from the call, so it is not written.
        if is_method && !self.options.write_function_definition_self_arg {
            if !arguments.is_empty() {
                arguments.remove(0);
            }
            self.state().function_method = false;
        }

        self.visit_comma_separated_list(&arguments);
        self.write(")");
        self.end_line();

        // A function ends with an implicit `return`; it is not written out.
        let contents = traverse::list_contents(&statements);
        let ends_with_empty_return = contents.len() > 1
            && contents.last().is_some_and(|last| {
                matches!(&*last.borrow(), Node::Return(inner)
                    if traverse::list_contents(&inner.returns).is_empty())
            });
        if ends_with_empty_return {
            let mut contents = contents;
            contents.pop();
            traverse::set_list_contents(&statements, contents);
        }

        self.visit_node(&statements);
        self.write("end");

        if is_statement {
            self.end_statement(Statement::Function);
        }
    }

    fn visit_table_constructor(&mut self, constructor: &NodeRef) {
        let (array, records) = {
            let borrowed = constructor.borrow();
            let Node::TableConstructor(inner) = &*borrowed else {
                return;
            };
            (inner.array.clone(), inner.records.clone())
        };

        self.write("{");
        self.skip(&array);
        self.skip(&records);

        let array_contents = traverse::list_contents(&array);
        let mut contents = array_contents.clone();
        contents.extend(traverse::list_contents(&records));

        if !array_contents.is_empty() {
            // The array part comes first, so its first entry is the first entry
            // of the combined list. Index zero is only in the array when it was
            // written explicitly; an empty placeholder stands for the value
            // that was never there.
            let first = contents.remove(0);
            let value = match &*first.borrow() {
                Node::ArrayRecord(record) => record.value.clone(),
                _ => first.clone(),
            };
            let is_nil = matches!(&*value.borrow(), Node::Primitive(primitive)
                if primitive.kind == PrimitiveKind::Nil);

            if !is_nil {
                let key = crate::ast::nodes::node(Node::Constant(Box::new(Constant {
                    value: ConstantValue::Integer(0),
                    meta: Meta::default(),
                })));
                contents.insert(
                    0,
                    crate::ast::nodes::node(Node::TableRecord(Box::new(TableRecord {
                        key,
                        value,
                        meta: Meta::default(),
                    }))),
                );
            }
        }

        if self.options.compact_table_constructors && contents.len() == 1 {
            self.visit_node(&contents[0]);
        } else if !contents.is_empty() {
            self.end_line();
            self.start_block();
            self.visit_record_list(&contents);
            self.end_block();
        }

        self.write("}");
    }

    fn visit_table_record(&mut self, node: &NodeRef) {
        let (key, value) = {
            let borrowed = node.borrow();
            let Node::TableRecord(inner) = &*borrowed else {
                return;
            };
            (inner.key.clone(), inner.value.clone())
        };

        if is_valid_name(&key) {
            self.write_name(&key);
            self.skip(&key);
            self.write(" = ");
        } else {
            self.write("[");
            self.visit_node(&key);
            self.write("] = ");
        }

        self.visit_node(&value);
    }

    fn visit_assignment(&mut self, node: &NodeRef) {
        let (kind, destinations, expressions) = {
            let borrowed = node.borrow();
            let Node::Assignment(inner) = &*borrowed else {
                return;
            };
            (
                inner.kind,
                inner.destinations.clone(),
                inner.expressions.clone(),
            )
        };

        let is_local = kind == AssignmentKind::LocalDefinition;
        let dsts = traverse::list_contents(&destinations);
        let srcs = traverse::list_contents(&expressions);

        // A register that stands for a constant is not a variable, so an
        // assignment to one cannot be written out. It is left over from a
        // branch the decompiler could not make sense of; dropping it keeps the
        // output readable Lua instead of `false = ...`.
        if !dsts.is_empty() && dsts.iter().all(is_constant_slot) {
            return;
        }

        let mut source_is_function = false;
        if dsts.len() == 1 && srcs.len() == 1 {
            let destination = dsts[0].clone();
            let source = srcs[0].clone();
            source_is_function = matches!(&*source.borrow(), Node::FunctionDefinition(_));

            if source_is_function {
                let acceptable = if self.options.function_definition_sugar {
                    is_acceptable_function_destination(&destination)
                } else {
                    is_variable(&destination)
                };

                if acceptable {
                    self.state().function_name = Some(destination.clone());
                    self.state().function_local = is_local;
                    self.state().function_method = is_method(&destination, &source);

                    self.visit_node(&source);
                    self.skip(&destinations);
                    self.skip(&expressions);
                    return;
                }
            }
        }

        if is_local {
            self.write("local ");
        }

        let statement = if source_is_function {
            Statement::Function
        } else {
            Statement::Assignment
        };
        self.start_statement(statement);

        self.visit_node(&destinations);
        self.write(" = ");
        self.visit_node(&expressions);

        self.end_statement(statement);
    }

    fn visit_binary_operator(&mut self, node: &NodeRef) {
        let (kind, left, right) = {
            let borrowed = node.borrow();
            let Node::BinaryOperator(inner) = &*borrowed else {
                return;
            };
            (inner.kind, inner.left.clone(), inner.right.clone())
        };

        if self.options.bitop_style == BitOpStyle::BitLibrary
            && let Some(name) = bit_library_name(kind)
        {
            // The operands become arguments, so they never need braces.
            self.write(format!("{name}("));
            self.visit_node(&left);
            self.write(", ");
            self.visit_node(&right);
            self.write(")");
            return;
        }

        let left_precedence = precedence_of(&left);
        let right_precedence = precedence_of(&right);

        // A subexpression only needs braces when it binds less tightly than the
        // operator it sits under; at the same precedence it needs them on the
        // side the operator does not associate to.
        let mut left_parentheses = false;
        if let Some(precedence) = left_precedence {
            left_parentheses = if kind.is_right_associative() {
                precedence <= kind.precedence()
            } else {
                precedence < kind.precedence()
            };
        }

        let mut right_parentheses = false;
        if let Some(precedence) = right_precedence {
            right_parentheses = if kind.is_right_associative() {
                precedence < kind.precedence()
            } else {
                precedence <= kind.precedence()
            };

            // `a + (b + c)` and `a * (b * c)` are the same as `a + b + c` and
            // `a * b * c`, so the braces can go.
            if !kind.is_right_associative() && Some(kind.precedence()) == right_precedence {
                use BinaryOperatorKind::*;
                let droppable = matches!(
                    (kind, binary_kind_of(&right)),
                    (Add, Some(Add | Subtract)) | (Multiply, Some(Multiply | Division))
                );
                if droppable {
                    right_parentheses = false;
                }
            }
        }

        if left_parentheses {
            self.write("(");
        }
        self.visit_node(&left);
        if left_parentheses {
            self.write(")");
        }

        if kind == BinaryOperatorKind::Pow {
            // `^` is written without spaces, both here and by ljd.
            self.write("^");
        } else {
            let operator = kind.as_str();
            self.write(format!(" {operator} "));
        }

        if right_parentheses {
            self.write("(");
        }
        self.visit_node(&right);
        if right_parentheses {
            self.write(")");
        }
    }

    fn visit_unary_operator(&mut self, node: &NodeRef) {
        let (kind, operand) = {
            let borrowed = node.borrow();
            let Node::UnaryOperator(inner) = &*borrowed else {
                return;
            };
            (inner.kind, inner.operand.clone())
        };

        match kind {
            UnaryOperatorKind::Not => {
                // LuaJIT compiles the constant `false` into a register of its
                // own, so `not false` has to be written as `true`.
                let slot = match &*operand.borrow() {
                    Node::Identifier(identifier) => Some(identifier.slot),
                    _ => None,
                };

                if slot == Some(SLOT_FALSE) {
                    if let Node::Identifier(identifier) = &mut *operand.borrow_mut() {
                        identifier.slot = SLOT_TRUE;
                    }
                } else {
                    self.write("not ");
                }
            }
            UnaryOperatorKind::ToString | UnaryOperatorKind::ToNumber => {
                // ljd writes these without the call, which cannot be parsed
                // back; they are written as calls here.
                let name = if kind == UnaryOperatorKind::ToString {
                    "tostring"
                } else {
                    "tonumber"
                };
                self.write(format!("{name}("));
                self.visit_node(&operand);
                self.write(")");
                return;
            }
            UnaryOperatorKind::BitNot if self.options.bitop_style == BitOpStyle::BitLibrary => {
                self.write("bit.bnot(");
                self.visit_node(&operand);
                self.write(")");
                return;
            }
            _ => self.write(kind.as_str()),
        }

        let need_parentheses = needs_parentheses_around(kind, &operand);
        if need_parentheses {
            self.write("(");
        }

        // `- -x` has to keep a space, or it turns into a comment.
        if matches!(kind, UnaryOperatorKind::Minus)
            && !need_parentheses
            && matches!(&*operand.borrow(), Node::UnaryOperator(inner)
                if inner.kind == UnaryOperatorKind::Minus)
        {
            self.write(" ");
        }

        self.visit_node(&operand);
        if need_parentheses {
            self.write(")");
        }
    }

    fn visit_function_call(&mut self, node: &NodeRef) {
        let (function, arguments, is_method) = {
            let borrowed = node.borrow();
            let Node::FunctionCall(inner) = &*borrowed else {
                return;
            };
            (
                inner.function.clone(),
                inner.arguments.clone(),
                inner.is_method,
            )
        };

        let is_statement = self.state().current_statement == Statement::None;
        if is_statement {
            self.start_statement(Statement::FunctionCall);
        }

        if is_method {
            let (table, key) = match &*function.borrow() {
                Node::TableElement(element) => (element.table.clone(), element.key.clone()),
                _ => (function.clone(), function.clone()),
            };

            let needs_parentheses = neighbours_a_constructor(&table);
            if needs_parentheses {
                self.write("(");
            }
            self.visit_node(&table);
            if needs_parentheses {
                self.write(")");
            }

            self.write(":");
            self.write_name(&key);
            self.skip(&key);
            self.skip(&function);

            self.write("(");
            self.visit_node(&arguments);
            self.write(")");
            self.skip(&arguments);
        } else {
            let needs_parentheses = neighbours_a_constructor(&function);
            if needs_parentheses {
                self.write("(");
            }
            self.visit_node(&function);
            if needs_parentheses {
                self.write(")");
            }
            self.write("(");
            self.visit_node(&arguments);
            self.write(")");
        }

        if is_statement {
            self.end_statement(Statement::FunctionCall);
        }
    }

    fn visit_if(&mut self, node: &NodeRef) {
        let (expression, then_block, elseifs, else_block) = {
            let borrowed = node.borrow();
            let Node::If(inner) = &*borrowed else {
                return;
            };
            (
                inner.expression.clone(),
                inner.then_block.clone(),
                inner.elseifs.clone(),
                inner.else_block.clone(),
            )
        };

        self.start_statement(Statement::If);
        self.write("if ");
        self.visit_node(&expression);
        self.write(" then");
        self.end_line();

        self.visit_node(&then_block);

        for branch in &elseifs {
            self.visit_node(branch);
        }

        if traverse::list_contents(&else_block).is_empty() {
            self.skip(&else_block);
        } else {
            self.write("else");
            self.end_line();
            self.visit_node(&else_block);
        }

        self.write("end");
        self.end_statement(Statement::If);
    }

    fn visit_elseif(&mut self, node: &NodeRef) {
        let (expression, then_block) = {
            let borrowed = node.borrow();
            let Node::ElseIf(inner) = &*borrowed else {
                return;
            };
            (inner.expression.clone(), inner.then_block.clone())
        };

        self.write("elseif ");
        self.visit_node(&expression);
        self.write(" then");
        self.end_line();
        self.visit_node(&then_block);
    }

    fn visit_while(&mut self, node: &NodeRef) {
        let (expression, statements) = {
            let borrowed = node.borrow();
            let Node::While(inner) = &*borrowed else {
                return;
            };
            (inner.expression.clone(), inner.statements.clone())
        };

        self.start_statement(Statement::While);
        self.write("while ");
        self.visit_node(&expression);
        self.write(" do");
        self.end_line();
        self.visit_node(&statements);
        self.write("end");
        self.end_statement(Statement::While);
    }

    fn visit_repeat_until(&mut self, node: &NodeRef) {
        let (expression, statements) = {
            let borrowed = node.borrow();
            let Node::RepeatUntil(inner) = &*borrowed else {
                return;
            };
            (inner.expression.clone(), inner.statements.clone())
        };

        self.start_statement(Statement::RepeatUntil);
        self.write("repeat");
        self.end_line();
        self.visit_node(&statements);
        self.write("until ");
        self.visit_node(&expression);
        self.end_statement(Statement::RepeatUntil);
    }

    fn visit_numeric_for(&mut self, node: &NodeRef) {
        let (variable, expressions, statements) = {
            let borrowed = node.borrow();
            let Node::NumericFor(inner) = &*borrowed else {
                return;
            };
            (
                inner.variable.clone(),
                inner.expressions.clone(),
                inner.statements.clone(),
            )
        };

        self.start_statement(Statement::NumericFor);
        self.write("for ");
        self.visit_node(&variable);
        self.write(" = ");
        self.skip(&expressions);

        // The step is left out when it is one.
        let mut expressions = traverse::list_contents(&expressions);
        let default_step = expressions.len() == 3
            && matches!(&*expressions[2].borrow(), Node::Constant(constant)
                if constant.value == ConstantValue::Integer(1));
        if default_step {
            expressions.pop();
        }

        if expressions.is_empty() {
            return;
        }
        for part in &expressions[..expressions.len() - 1] {
            self.visit_node(part);
            self.write(", ");
        }
        self.visit_node(expressions.last().expect("checked above"));

        self.write(" do");
        self.end_line();
        self.visit_node(&statements);
        self.write("end");
        self.end_statement(Statement::NumericFor);
    }

    fn visit_iterator_for(&mut self, node: &NodeRef) {
        let (identifiers, expressions, statements) = {
            let borrowed = node.borrow();
            let Node::IteratorFor(inner) = &*borrowed else {
                return;
            };
            (
                inner.identifiers.clone(),
                inner.expressions.clone(),
                inner.statements.clone(),
            )
        };

        self.start_statement(Statement::IteratorFor);
        self.write("for ");
        self.visit_node(&identifiers);
        self.write(" in ");
        self.visit_node(&expressions);
        self.write(" do");
        self.end_line();
        self.visit_node(&statements);
        self.write("end");
        self.end_statement(Statement::IteratorFor);
    }

    fn visit_return(&mut self, node: &NodeRef) {
        let returns = match &*node.borrow() {
            Node::Return(inner) => inner.returns.clone(),
            _ => return,
        };

        self.start_statement(Statement::Return);
        if traverse::list_contents(&returns).is_empty() {
            self.write("return");
        } else {
            self.write("return ");
        }
        self.visit_node(&returns);
        self.end_statement(Statement::Return);
    }

    fn visit_break(&mut self) {
        self.start_statement(Statement::Break);
        self.write("break");
        self.end_statement(Statement::Break);
    }

    // -- lists and values --------------------------------------------------

    fn visit_statements_list(&mut self, node: &NodeRef) {
        if self.states.len() > 1 {
            self.start_block();
        }

        // Statements inside a function get a state of their own, so that the
        // name of the function a definition belongs to is not carried over.
        self.states.push(State::new());

        if self.options.comment_empty_blocks && self.path.len() > 1 {
            let contents = traverse::list_contents(node);
            let block_is_shape = matches!(
                &*self.path[self.path.len() - 2].borrow(),
                Node::IteratorFor(_) | Node::If(_) | Node::ElseIf(_)
            );

            let add_comment = if contents.is_empty() {
                block_is_shape
            } else {
                contents.len() == 1 && matches!(&*contents[0].borrow(), Node::NoOp(_))
            };

            if add_comment {
                self.write("-- Nothing");
                self.end_line();
            }
        }
    }

    fn leave_statements_list(&mut self) {
        self.states.pop();

        if self.states.len() > 1 {
            self.end_block();
        }
    }

    fn visit_comma_separated_list(&mut self, contents: &[NodeRef]) {
        if contents.is_empty() {
            return;
        }

        for subnode in &contents[..contents.len() - 1] {
            self.visit_node(subnode);
            self.write(", ");
        }

        self.visit_node(contents.last().expect("checked above"));
    }

    fn visit_record_list(&mut self, contents: &[NodeRef]) {
        if contents.is_empty() {
            return;
        }

        for subnode in &contents[..contents.len() - 1] {
            self.visit_node(subnode);
            self.write(",");
            self.end_line();
        }

        self.visit_node(contents.last().expect("checked above"));
        self.end_line();
    }

    fn visit_identifier(&mut self, node: &NodeRef) {
        enum Action {
            Slot {
                slot: u32,
                id: Option<u32>,
                possible_ids: Vec<u32>,
            },
            Upvalue(u32),
            Named(String),
            Nothing,
        }

        let action = match &*node.borrow() {
            Node::Identifier(identifier) => {
                if identifier.kind == IdentifierKind::Slot {
                    Action::Slot {
                        slot: identifier.slot,
                        id: identifier.id,
                        possible_ids: identifier.possible_ids.clone(),
                    }
                } else if let Some(name) = &identifier.name {
                    Action::Named(name.clone())
                } else if identifier.kind == IdentifierKind::Upvalue {
                    Action::Upvalue(identifier.slot)
                } else {
                    Action::Nothing
                }
            }
            _ => Action::Nothing,
        };

        match action {
            Action::Slot {
                slot,
                id,
                possible_ids,
            } => {
                if slot == SLOT_FALSE {
                    self.write("false");
                } else if slot == SLOT_TRUE {
                    self.write("true");
                } else {
                    let mut name = format!("slot{slot}");
                    if self.options.show_slot_ids {
                        let ids = match id {
                            Some(id) => vec![id],
                            None => possible_ids,
                        };

                        if !ids.is_empty() {
                            name.push('#');
                            if ids.len() == 1 {
                                name.push_str(&ids[0].to_string());
                            } else {
                                name.push('{');
                                for (index, id) in ids.iter().enumerate() {
                                    if index > 0 {
                                        name.push('|');
                                    }
                                    name.push_str(&id.to_string());
                                }
                                name.push('}');
                            }
                        }
                    }
                    self.write(name);
                }
            }
            Action::Upvalue(slot) => {
                let placeholder = format!("uv{slot}");
                self.write(placeholder);
            }
            Action::Named(name) => self.write(name),
            Action::Nothing => {}
        }
    }

    fn visit_table_element(&mut self, node: &NodeRef) {
        let (key, table) = {
            let borrowed = node.borrow();
            let Node::TableElement(inner) = &*borrowed else {
                return;
            };
            (inner.key.clone(), inner.table.clone())
        };

        if is_global(node) {
            // A global is written as its name, without the environment table.
            self.skip(&table);
            self.skip(&key);
            self.write_name(&key);
            return;
        }

        let needs_parentheses = neighbours_a_constructor(&table);
        if needs_parentheses {
            self.write("(");
        }
        self.visit_node(&table);
        if needs_parentheses {
            self.write(")");
        }

        if is_valid_name(&key) {
            self.write(".");
            self.write_name(&key);
            self.skip(&key);
        } else {
            self.write("[");
            self.visit_node(&key);
            self.write("]");
        }
    }

    fn visit_constant(&mut self, node: &NodeRef) {
        let value = match &*node.borrow() {
            Node::Constant(constant) => constant.value.clone(),
            _ => return,
        };

        match value {
            ConstantValue::Integer(value) => {
                let text = value.to_string();
                self.write(text);
            }
            ConstantValue::Float(value) => {
                let text = format_number(value);
                self.write(text);
            }
            ConstantValue::CData(bytes) => {
                let text = decode_string(&bytes);
                self.write(text);
            }
            ConstantValue::String(bytes) => {
                let text = decode_string(&bytes);
                self.write_string_literal(&text);
            }
        }
    }

    /// Writes a string the way ljd does: long strings for anything with more
    /// than two newlines, an escaped literal otherwise.
    fn write_string_literal(&mut self, text: &str) {
        if text.matches('\n').count() > 2 {
            self.write("[[\n");
            self.write(text.to_string());
            self.write("]]");
            return;
        }

        let mut escaped = String::with_capacity(text.len() + 2);
        escaped.push('"');
        for character in text.chars() {
            match character {
                '\\' => escaped.push_str("\\\\"),
                '\t' => escaped.push_str("\\t"),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '"' => escaped.push_str("\\\""),
                // ljd only handles the escapes above; anything else that
                // cannot sit in a literal is written as a numeric escape so
                // that the text can be read back.
                character if character.is_control() && character != '\t' => {
                    for byte in character.to_string().as_bytes() {
                        escaped.push_str(&format!("\\{byte}"));
                    }
                }
                character => escaped.push(character),
            }
        }
        escaped.push('"');
        self.write(escaped);
    }

    fn visit_primitive(&mut self, node: &NodeRef) {
        let kind = match &*node.borrow() {
            Node::Primitive(inner) => inner.kind,
            _ => return,
        };

        let text = match kind {
            PrimitiveKind::False => "false",
            PrimitiveKind::True => "true",
            PrimitiveKind::Nil => "nil",
        };
        self.write(text);
    }

    // -- the walk ----------------------------------------------------------

    /// Prints a node and its children.
    fn visit_node(&mut self, node: &NodeRef) {
        if self
            .skipped
            .last()
            .expect("the skip stack is never empty")
            .contains(&node_key(node))
        {
            // A `nil` right after a comma is an empty slot in an argument
            // list, and those are written out.
            let after_comma =
                matches!(self.queue.last(), Some(Command::Write(text)) if text == ", ");
            let is_nil = matches!(&*node.borrow(), Node::Primitive(inner)
                if inner.kind == PrimitiveKind::Nil);
            if !(is_nil && after_comma) {
                return;
            }
        }

        self.skip(node);
        self.skipped.push(HashSet::new());
        self.path.push(node.clone());

        self.dispatch(node);

        self.path.pop();
        self.skipped.pop();
    }

    fn dispatch(&mut self, node: &NodeRef) {
        let action = {
            let borrowed = node.borrow();
            match &*borrowed {
                Node::FunctionDefinition(_) => Action::FunctionDefinition,
                Node::TableConstructor(_) => Action::TableConstructor,
                Node::TableRecord(_) => Action::TableRecord,
                Node::Assignment(_) => Action::Assignment,
                Node::BinaryOperator(_) => Action::BinaryOperator,
                Node::UnaryOperator(_) => Action::UnaryOperator,
                Node::FunctionCall(_) => Action::FunctionCall,
                Node::If(_) => Action::If,
                Node::ElseIf(_) => Action::ElseIf,
                Node::While(_) => Action::While,
                Node::RepeatUntil(_) => Action::RepeatUntil,
                Node::NumericFor(_) => Action::NumericFor,
                Node::IteratorFor(_) => Action::IteratorFor,
                Node::Return(_) => Action::Return,
                Node::Break => Action::Break,
                Node::Statements(_) => Action::StatementsList,
                Node::Identifiers(_) | Node::Variables(_) | Node::Expressions(_) => Action::List,
                Node::Records(_) => Action::RecordsList,
                Node::Identifier(_) => Action::Identifier,
                Node::TableElement(_) => Action::TableElement,
                Node::Constant(_) => Action::Constant,
                Node::Primitive(_) => Action::Primitive,
                Node::ArrayRecord(_) => Action::Transparent,
                Node::Vararg => Action::Vararg,
                Node::MulTres => Action::MulTres,
                Node::NoOp(_) => Action::Nothing,
                _ => Action::Transparent,
            }
        };

        match action {
            Action::FunctionDefinition => self.visit_function_definition(node),
            Action::TableConstructor => self.visit_table_constructor(node),
            Action::TableRecord => self.visit_table_record(node),
            Action::Assignment => self.visit_assignment(node),
            Action::BinaryOperator => self.visit_binary_operator(node),
            Action::UnaryOperator => self.visit_unary_operator(node),
            Action::FunctionCall => self.visit_function_call(node),
            Action::If => self.visit_if(node),
            Action::ElseIf => self.visit_elseif(node),
            Action::While => self.visit_while(node),
            Action::RepeatUntil => self.visit_repeat_until(node),
            Action::NumericFor => self.visit_numeric_for(node),
            Action::IteratorFor => self.visit_iterator_for(node),
            Action::Return => self.visit_return(node),
            Action::Break => self.visit_break(),
            Action::StatementsList => {
                self.visit_statements_list(node);
                self.visit_children(node);
                self.leave_statements_list();
            }
            Action::List => {
                let contents = traverse::list_contents(node);
                self.visit_comma_separated_list(&contents);
            }
            Action::RecordsList => {
                let contents = traverse::list_contents(node);
                self.visit_record_list(&contents);
            }
            Action::Identifier => self.visit_identifier(node),
            Action::TableElement => self.visit_table_element(node),
            Action::Constant => self.visit_constant(node),
            Action::Primitive => self.visit_primitive(node),
            Action::Vararg => self.write("..."),
            Action::MulTres => self.write("MULTRES"),
            Action::Nothing => {}
            Action::Transparent => self.visit_children(node),
        }
    }

    fn visit_children(&mut self, node: &NodeRef) {
        let children = traverse::children(&node.borrow());
        for child in children {
            self.visit_node(&child);
        }
    }
}

enum Action {
    FunctionDefinition,
    TableConstructor,
    TableRecord,
    Assignment,
    BinaryOperator,
    UnaryOperator,
    FunctionCall,
    If,
    ElseIf,
    While,
    RepeatUntil,
    NumericFor,
    IteratorFor,
    Return,
    Break,
    StatementsList,
    List,
    RecordsList,
    Identifier,
    TableElement,
    Constant,
    Primitive,
    /// A node that only holds a value, like an array record.
    Transparent,
    Vararg,
    MulTres,
    Nothing,
}

/// The `bit` library function for an operator, when there is one.
fn bit_library_name(kind: BinaryOperatorKind) -> Option<&'static str> {
    use BinaryOperatorKind::*;
    match kind {
        BitOr => Some("bit.bor"),
        BitXor => Some("bit.bxor"),
        BitAnd => Some("bit.band"),
        ShiftLeft => Some("bit.lshift"),
        ShiftRight => Some("bit.rshift"),
        ShiftArithmeticRight => Some("bit.arshift"),
        _ => None,
    }
}

/// How tightly a node binds, when it is an operator.
fn precedence_of(node: &NodeRef) -> Option<Precedence> {
    match &*node.borrow() {
        Node::BinaryOperator(inner) => Some(inner.kind.precedence()),
        Node::UnaryOperator(_) => Some(Precedence::Unary),
        _ => None,
    }
}

/// Whether an operand has to be wrapped in braces.
///
/// Every binary operator binds less tightly than a unary one, so it always
/// needs them; between two unary operators it depends on which one comes first.
fn needs_parentheses_around(kind: UnaryOperatorKind, operand: &NodeRef) -> bool {
    match &*operand.borrow() {
        Node::BinaryOperator(_) => true,
        Node::UnaryOperator(inner) => inner.kind.rank() < kind.rank(),
        _ => false,
    }
}

/// The operator of a binary operator node.
fn binary_kind_of(node: &NodeRef) -> Option<BinaryOperatorKind> {
    match &*node.borrow() {
        Node::BinaryOperator(inner) => Some(inner.kind),
        _ => None,
    }
}

/// Whether a node is a register that stands for one of the constants `false`
/// and `true`.
fn is_constant_slot(node: &NodeRef) -> bool {
    matches!(&*node.borrow(), Node::Identifier(identifier)
        if identifier.kind == IdentifierKind::Slot
            && (identifier.slot == SLOT_FALSE || identifier.slot == SLOT_TRUE))
}

/// Whether a value has to be wrapped before it can be indexed or called.
fn neighbours_a_constructor(node: &NodeRef) -> bool {
    match &*node.borrow() {
        Node::TableConstructor(_)
        | Node::BinaryOperator(_)
        | Node::UnaryOperator(_)
        | Node::FunctionDefinition(_) => true,
        Node::Constant(constant) => matches!(constant.value, ConstantValue::String(_)),
        _ => false,
    }
}

/// Whether a destination can be written as the name of a function.
fn is_variable(node: &NodeRef) -> bool {
    matches!(&*node.borrow(), Node::Identifier(_)) || is_global(node)
}

/// Whether the node reads a global through the function environment.
fn is_global(node: &NodeRef) -> bool {
    match &*node.borrow() {
        Node::TableElement(inner) => is_builtin(&inner.table),
        _ => false,
    }
}

fn is_builtin(node: &NodeRef) -> bool {
    matches!(&*node.borrow(), Node::Identifier(identifier)
        if identifier.kind == IdentifierKind::Builtin)
}

/// Whether the destination can carry a `function name()` prefix.
fn is_acceptable_function_destination(destination: &NodeRef) -> bool {
    match &*destination.borrow() {
        Node::Identifier(_) => true,
        Node::TableElement(element) => {
            let Node::Constant(key) = &*element.key.borrow() else {
                return false;
            };
            let ConstantValue::String(name) = &key.value else {
                return false;
            };
            let name = decode_string(name);

            match name.chars().next() {
                None => return false,
                Some(first) if first.is_ascii_digit() => return false,
                Some(_) => {}
            }
            if !name
                .chars()
                .all(|character| character.is_alphanumeric() || character == '_')
            {
                return false;
            }

            is_acceptable_function_destination(&element.table)
        }
        _ => false,
    }
}

/// Whether the function's first argument is the `self` a method takes.
fn is_method(destination: &NodeRef, function: &NodeRef) -> bool {
    let arguments = match &*function.borrow() {
        Node::FunctionDefinition(inner) => traverse::list_contents(&inner.arguments),
        _ => return false,
    };
    let Some(first) = arguments.first() else {
        return false;
    };
    let is_self = matches!(&*first.borrow(), Node::Identifier(identifier)
        if identifier.name.as_deref() == Some("self"));
    if !is_self {
        return false;
    }

    matches!(&*destination.borrow(), Node::TableElement(_))
}

/// Whether a key can be written as a name.
fn is_valid_name(key: &NodeRef) -> bool {
    let Node::Constant(constant) = &*key.borrow() else {
        return false;
    };
    let ConstantValue::String(name) = &constant.value else {
        return false;
    };
    let name = decode_string(name);

    is_valid_identifier(&name) && !RESERVED_WORDS.contains(&name.as_str())
}

/// Whether a string can be written as a Lua name.
fn is_valid_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_alphabetic() || first == '_' => {}
        _ => return false,
    }

    characters.all(|character| character.is_alphanumeric() || character == '_')
}

const RESERVED_WORDS: [&str; 21] = [
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Reads the bytes of a string constant as text.
///
/// Constants are raw bytes. Whatever is valid UTF-8 is kept as it is, and every
/// byte that is not is written as a `\xNN` escape, which is what Python's
/// `backslashreplace` does. Keeping the valid parts matters: a newline after an
/// invalid byte is still a newline, and decides how the string is written.
fn decode_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;

    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                // `error_len` is `None` when the input simply ends in the
                // middle of a sequence.
                let invalid = error.error_len().unwrap_or(rest.len() - valid);

                out.push_str(std::str::from_utf8(&rest[..valid]).expect("checked by the error"));
                for byte in &rest[valid..valid + invalid] {
                    out.push_str(&format!("\\x{byte:02x}"));
                }
                rest = &rest[valid + invalid..];
            }
        }
    }

    out
}

/// Formats a number so that Lua reads back the same value.
///
/// The result matches what Python's `repr` produces for the same double, which
/// is what ljd writes: the shortest form that round trips, in ordinary notation
/// unless the exponent is very small or very large.
fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "(0/0)".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "math.huge".to_string()
        } else {
            "-math.huge".to_string()
        };
    }

    let sign = if value.is_sign_negative() { "-" } else { "" };
    let value = value.abs();

    // `{:e}` gives the shortest digits that round trip, as `d.ddde<exp>`.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("the scientific form always has an exponent");
    let exponent: i32 = exponent.parse().expect("the exponent is a number");

    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };

    // The exponent of the leading digit; a value is written in ordinary
    // notation while that is at least -4 and below 16.
    let leading = exponent + mantissa.chars().take_while(|c| *c != '.').count() as i32 - 1;

    if !(-4..16).contains(&leading) {
        let fraction = digits.get(1..).unwrap_or("");
        let mantissa = if fraction.is_empty() {
            digits.to_string()
        } else {
            format!("{}.{fraction}", &digits[..1])
        };
        let sign_of_exponent = if leading < 0 { '-' } else { '+' };
        return format!("{sign}{mantissa}e{sign_of_exponent}{:02}", leading.abs());
    }

    let mut text = sign.to_string();
    if leading >= 0 {
        let whole = (leading + 1) as usize;
        if digits.len() > whole {
            text.push_str(&digits[..whole]);
            text.push('.');
            text.push_str(&digits[whole..]);
        } else {
            text.push_str(digits);
            text.push_str("0".repeat(whole - digits.len()).as_str());
            text.push_str(".0");
        }
    } else {
        text.push_str("0.");
        text.push_str("0".repeat((-leading - 1) as usize).as_str());
        text.push_str(digits);
    }

    text
}

// -- printing the queue ----------------------------------------------------

/// The next command that is neither a line break nor text.
fn next_significant(queue: &[Command], index: usize) -> Option<&Command> {
    let mut i = index + 1;

    while i < queue.len() {
        if !matches!(queue[i], Command::EndLine | Command::Write(_)) {
            return Some(&queue[i]);
        }
        i += 1;
    }

    None
}

/// Turns the commands into the text they stand for.
fn process_queue(queue: &[Command], options: &Options) -> String {
    let mut out = String::new();
    let mut indent = 0usize;
    let mut line_broken = true;

    for (index, command) in queue.iter().enumerate() {
        match command {
            Command::StartStatement(_) => {}
            Command::EndStatement(statement) => {
                out.push('\n');
                line_broken = true;

                // Statements of different kinds are separated by a blank line,
                // and a block statement always gets one around it.
                if let Some(Command::StartStatement(next)) = next_significant(queue, index)
                    && (next != statement || statement.is_block() || next.is_block())
                {
                    out.push('\n');
                }
            }
            Command::EndLine => {
                out.push('\n');
                line_broken = true;
            }
            Command::StartBlock => indent += 1,
            Command::EndBlock => indent = indent.saturating_sub(1),
            Command::Write(text) => {
                if line_broken {
                    for _ in 0..indent {
                        out.push_str(options.indent.as_str());
                    }
                    line_broken = false;
                }
                out.push_str(text);
            }
        }
    }

    out
}
