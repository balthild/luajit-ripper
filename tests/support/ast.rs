//! The graph a chunk turns into, and the pipeline stages over it.
//!
//! A harness that wants to check a whole corpus runs the same passes the
//! decompiler does, one function at a time, and needs to walk every prototype
//! of a chunk to do it. That is what is collected here.

/// A shared, mutable AST node.
pub use luajit_ripper::ast::nodes::NodeRef;
use luajit_ripper::bytecode::{Chunk, Prototype};

use super::parse;

// MARK: prototypes

/// Every prototype of a chunk, the root first and nested ones after it.
pub fn prototypes<'a>(chunk: &'a Chunk<'a>) -> Vec<&'a Prototype<'a>> {
    let mut out: Vec<&'a Prototype<'a>> = vec![chunk.root];
    let mut index = 0;
    while index < out.len() {
        let children: Vec<&'a Prototype<'a>> = out[index].children().collect();
        out.extend(children);
        index += 1;
    }
    out
}

/// How many prototypes a chunk holds, nested functions included.
pub fn count_prototypes(chunk: &Chunk) -> usize {
    prototypes(chunk).len()
}

// MARK: graph

/// Builds the AST of a chunk and runs the passes that come before unwarping.
///
/// This is the order the decompiler uses: the graph is repaired, the local
/// variable names are recovered, and the temporary registers are inlined.
pub fn prepare(chunk: &'static Chunk<'static>) -> Result<NodeRef<'static>, luajit_ripper::Error> {
    let alloc = parse::arena();
    let root = luajit_ripper::ast::builder::build(alloc, chunk)?;
    pre_pass(alloc, root)?;
    Ok(root)
}

/// Builds the graph of one function up to slot elimination, the state unwarping
/// starts from.
///
/// Unwarping rewrites the tree in place, so a retry at the same function needs a
/// tree of its own; it would otherwise start from a half-rewritten graph. That
/// is why this takes a prototype rather than a built graph: it can be called
/// twice for the same function.
pub fn prepare_function(
    chunk: &'static Chunk<'static>,
    prototype: &'static Prototype<'static>,
) -> Option<NodeRef<'static>> {
    let alloc = parse::arena();
    let root = luajit_ripper::ast::builder::build_function(alloc, chunk, prototype).ok()?;
    pre_pass(alloc, root).ok()?;
    Some(root)
}

/// The passes that run between building the graph and unwarping it.
fn pre_pass(
    alloc: &'static oxc_allocator::Allocator,
    root: NodeRef<'static>,
) -> Result<(), luajit_ripper::Error> {
    luajit_ripper::ast::mutator::pre_pass(alloc, root);
    luajit_ripper::ast::locals::mark_locals(root, false);
    luajit_ripper::ast::slotworks::eliminate_temporary(
        alloc,
        root,
        luajit_ripper::ast::slotworks::Options {
            identify_slots: true,
            ..Default::default()
        },
    )
}

/// How many blocks a graph holds.
pub fn count_blocks(root: NodeRef<'static>) -> usize {
    use luajit_ripper::ast::nodes::Node;

    let mut count = 0;
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        let borrowed = current.borrow();
        match &*borrowed {
            Node::Block(block) => {
                count += 1;
                stack.extend(block.contents.iter().copied());
                if let Some(warp) = block.warp {
                    stack.push(warp);
                }
            }
            Node::Assignment(inner) => {
                stack.push(inner.expressions);
                stack.push(inner.destinations);
            }
            Node::FunctionDefinition(inner) => {
                stack.push(inner.statements);
                stack.push(inner.arguments);
            }
            other => {
                if let Some(items) = other.list() {
                    stack.extend(items.iter().copied());
                }
            }
        }
    }
    count
}

// MARK: unwarping

/// What became of one function when it was unwarped.
#[derive(Debug)]
pub enum Unwarped {
    /// The strict pass structured it.
    Strict,
    /// The strict pass gave up, and recovery saved it from a fresh graph.
    Recovered,
    /// Neither pass could, with the error the strict pass ended on.
    Failed(luajit_ripper::Error),
}

/// Structures one function the way the decompiler does.
///
/// The strict pass is tried first and recovery is used as the fallback, from a
/// fresh graph: that is the policy the decompiler itself follows, and a harness
/// over a corpus wants to know which of the two did the work.
///
/// `None` means the graph could not be built at all, so the function never
/// reached unwarping.
pub fn unwarp_function(
    chunk: &'static Chunk<'static>,
    prototype: &'static Prototype<'static>,
) -> Option<Unwarped> {
    let root = prepare_function(chunk, prototype)?;

    match unwarp(root, luajit_ripper::ast::unwarper::Recovery::Off) {
        Ok(()) => Some(Unwarped::Strict),
        Err(error) => {
            let recovered = prepare_function(chunk, prototype).is_some_and(|root| {
                unwarp(root, luajit_ripper::ast::unwarper::Recovery::On).is_ok()
            });
            Some(if recovered {
                Unwarped::Recovered
            } else {
                Unwarped::Failed(error)
            })
        }
    }
}

/// Structures one already prepared graph.
pub fn unwarp(
    root: NodeRef<'static>,
    recovery: luajit_ripper::ast::unwarper::Recovery,
) -> Result<(), luajit_ripper::Error> {
    luajit_ripper::ast::unwarper::unwarp_chunk(parse::arena(), root, recovery)
}
