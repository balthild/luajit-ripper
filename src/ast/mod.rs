//! The decompiler's AST and the passes that rewrite it.
//!
//! The pipeline mirrors ljd's:
//!
//! 1. [`builder`] turns instructions into statements and a control flow graph.
//! 2. the passes in [`locals`], [`slotworks`] and [`mutator`] recover local
//!    variable names and inline temporaries.
//! 3. [`unwarper`] rebuilds structured control flow out of the graph.
//! 4. the Lua writer renders the result.

#![allow(dead_code)] // Parts of the node model are only used by later passes.

pub mod builder;
pub mod dump;
pub mod helpers;
pub mod locals;
pub mod mutator;
pub mod nodes;
pub mod slotworks;
pub mod traverse;
pub mod unwarper;
