//! The extension traits, gathered in one place.
//!
//! A trait is the only way to add a method to a type from another module, so the
//! questions a pass asks of a node are spread over the modules that answer them:
//! [`accessors`](crate::ast::accessors) has the predicates, and
//! [`traverse`](crate::ast::traverse) has the list and block accessors. Importing
//! them one by one at every call site is noise, so they are re-exported here and
//! a pass writes a single `use crate::ast::ext::*;` instead.
//!
//! [`AsNode`] comes along: it is what the traits are written over, so a caller
//! that names one of them in a bound needs it too.

pub use crate::ast::accessors::NodeAccessorExt;
pub use crate::ast::nodes::AsNode;
pub use crate::ast::traverse::{NodeBlockExt, NodeListExt};
