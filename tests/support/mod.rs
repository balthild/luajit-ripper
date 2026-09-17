//! Helpers shared by the integration tests.
//!
//! This is a plain module, not a test target of its own: cargo only picks up
//! the `.rs` files directly inside `tests/`.
//!
//! Most of what is here is meant for a test over *any* collection of dumps, not
//! just this repository's: [`Corpus`] reads a directory out of `LJR_CORPUS`,
//! [`luajit`] and [`compile`] build dumps to check against, [`prepare_function`]
//! runs the pipeline over one function of a chunk, and [`round_trip`] feeds
//! what the decompiler wrote back through it.
//!
//! ```text
//! luajit.rs    talking to the LuaJIT binary
//! fixtures.rs  the committed sources under tests/fixtures
//! parse.rs     a dump to a chunk
//! ast.rs       the graph, and the pipeline stages over it
//! corpus.rs    a directory of dumps, and sampling it
//! rng.rs       the seeded generator the sample is drawn with
//! roundtrip.rs comparing what a round trip reads and writes
//! ```

// Every test binary compiles this module on its own and uses a part of it, so
// what one of them does not touch is unused rather than dead.
#![allow(dead_code)]
#![allow(unused_imports)]

mod ast;
pub use ast::*;

mod corpus;
pub use corpus::*;

mod fixtures;
pub use fixtures::*;

mod luajit;
pub use luajit::*;

mod parse;
pub use parse::*;

mod rng;
pub use rng::*;

mod roundtrip;
pub use roundtrip::*;
