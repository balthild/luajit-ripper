#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! A LuaJIT 2.1 bytecode decompiler.
//!
//! The crate turns a raw LuaJIT bytecode dump into readable Lua source. It is
//! organised as a pipeline of independent stages, each of which is usable on
//! its own:
//!
//! * [`bytecode`] parses the binary dump into prototypes, instructions and
//!   constants.
//! * [`listing`] renders the parsed dump as a disassembly that matches
//!   `luajit -bl`.
//! * the AST stages build a control flow graph out of the instructions and then
//!   rewrite it back into structured statements.
//! * the Lua writer renders the structured AST as source text.
//!
//! The high level entry point is `decompile`, which runs the whole pipeline.
//!
//! # Limits
//!
//! Rebuilding source from bytecode is not always possible, because a jump
//! target does not record which construct produced it. Two shapes of input are
//! known to fall outside what the passes can recover:
//!
//! * A branch whose two arms only meet again through a chain of empty jumps
//!   cannot be told from straight line code. [`Options::on_function_error`]
//!   decides what happens: the chunk fails, or the region is written as the
//!   statements it holds and pointed out with a
//!   `-- Decompilation error in this vicinity:` comment. The recovered code is
//!   usually right, but a branch that could not be told apart loses the arm that
//!   was not taken.
//! * A few control flow graphs are reconstructed into statements that Lua will
//!   not parse, such as a `return` that ends up in front of the statements that
//!   follow it. Nothing detects this case, so the result has to be compiled to
//!   be sure of it.
//!
//! Both are inherited from the original decompiler; they are not specific to
//! one version of the bytecode.

pub mod bytecode;
mod error;
pub mod listing;

#[doc(hidden)]
pub mod ast;

#[doc(hidden)]
pub mod lua;

pub use crate::error::{Error, Result};
pub use crate::lua::writer::{BitOpStyle, Indent};

use crate::ast::nodes::{
    Constant, ConstantValue, FunctionCall, Identifier, IdentifierKind, Meta, Node, NodeRef,
};
use crate::ast::traverse;

/// What to do when a function cannot be decompiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnFunctionError {
    /// Stop and report the error.
    #[default]
    Fail,
    /// Write an `error("Decompilation failed")` call into the function and
    /// carry on with the rest of the chunk.
    ///
    /// The regions a pass gives up on are written as the statements they hold
    /// and pointed out with a comment, so that as much of the function as
    /// possible is still usable. The call is only written for a function that
    /// could not be finished at all.
    Mark,
}

/// How a chunk is decompiled.
#[derive(Debug, Clone)]
pub struct Options {
    /// How the output is indented.
    pub indent: Indent,
    /// How bit operations are written.
    pub bitop_style: BitOpStyle,
    /// What to do when one function cannot be decompiled.
    pub on_function_error: OnFunctionError,
    /// Whether registers that could not be named carry the ids of the
    /// definitions they may refer to.
    pub show_slot_ids: bool,
    /// Whether `t.f = function() end` is written as `function t.f() end`.
    pub function_definition_sugar: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            indent: Indent::Tabs,
            bitop_style: Default::default(),
            on_function_error: Default::default(),
            show_slot_ids: false,
            function_definition_sugar: false,
        }
    }
}

/// Decompiles a raw LuaJIT bytecode dump into Lua source.
///
/// The result is a complete chunk: running it defines everything the original
/// dump did. A function that cannot be decompiled is reported through
/// [`Options::on_function_error`].
pub fn decompile(data: &[u8], options: &Options) -> Result<String> {
    let chunk = bytecode::parse(data)?;
    let root = ast::builder::build(&chunk)?;
    decompile_ast(&root, options)
}

/// Decompiles an AST that the builder has already produced.
pub fn decompile_ast(root: &NodeRef, options: &Options) -> Result<String> {
    ast::mutator::pre_pass(root);

    ast::locals::mark_locals(root, false);

    ast::slotworks::eliminate_temporary(
        root,
        ast::slotworks::Options {
            identify_slots: true,
            ..Default::default()
        },
    )?;

    unwarp_chunk(root, options)?;

    ast::locals::mark_local_definitions(root);

    ast::mutator::primary_pass(root);

    ast::locals::mark_locals(root, true);
    ast::locals::mark_local_definitions(root);

    let writer_options = lua::writer::Options {
        indent: options.indent,
        bitop_style: options.bitop_style,
        show_slot_ids: options.show_slot_ids,
        function_definition_sugar: options.function_definition_sugar,
        ..Default::default()
    };

    lua::writer::write_function(root, &writer_options)
}

/// Rebuilds the control flow of every function of a chunk.
///
/// Each function is done on its own, so that a function that cannot be
/// decompiled does not take the rest of the chunk down with it.
fn unwarp_chunk(root: &NodeRef, options: &Options) -> Result<()> {
    // The nested functions come first, so that a function is finished before
    // the one that contains it is looked at.
    let mut functions = traverse::functions(root);
    functions.reverse();

    // Recovering is asked for when the caller wants the chunk to come out
    // whole: a function that cannot be structured is then written as far as it
    // got instead of being reported.
    let recovery = match options.on_function_error {
        OnFunctionError::Fail => ast::unwarper::Recovery::Off,
        OnFunctionError::Mark => ast::unwarper::Recovery::On,
    };

    for function in functions {
        // Recovering already handles the regions a pass gives up on, so the
        // fallback is only reached by a function that could not be written at
        // all, such as one whose control flow never got structured.
        if let Err(error) = ast::unwarper::unwarp(&function, recovery) {
            match options.on_function_error {
                OnFunctionError::Fail => return Err(error),
                OnFunctionError::Mark => mark_function_failed(&function),
            }
        }
    }

    Ok(())
}

/// Replaces the body of a function with a call that reports the failure.
fn mark_function_failed(function: &NodeRef) {
    let error_name = crate::ast::nodes::node(Node::Identifier(Box::new(Identifier {
        kind: IdentifierKind::Builtin,
        name: Some("error".to_string()),
        slot: 0,
        id: None,
        possible_ids: Vec::new(),
        local_end: None,
        meta: Meta::default(),
    })));

    let message = crate::ast::nodes::node(Node::Constant(Box::new(Constant {
        value: ConstantValue::String(b"Decompilation failed".to_vec().into_boxed_slice()),
        meta: Meta::default(),
    })));

    let call = crate::ast::nodes::node(Node::FunctionCall(Box::new(FunctionCall {
        function: error_name,
        arguments: crate::ast::nodes::statements(vec![message]),
        is_method: false,
        meta: Meta::default(),
    })));

    if let Node::FunctionDefinition(inner) = &mut *function.borrow_mut() {
        traverse::set_list_contents(&inner.statements, vec![call]);
    }
}
