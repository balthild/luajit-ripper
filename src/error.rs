//! Error types returned by this crate.

use thiserror::Error;

/// Convenience alias for results produced by this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while parsing or decompiling a bytecode dump.
///
/// The decompiler never panics on malformed input: every failure mode is
/// reported through this type.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The input does not start with a known bytecode dump magic.
    #[error("input is not a LuaJIT bytecode dump (bad magic)")]
    BadMagic,

    /// The dump was produced by a LuaJIT revision we do not support yet.
    #[error("unsupported bytecode version {0}: only LuaJIT 2.1 (version 2) dumps are supported")]
    UnsupportedVersion(u8),

    /// The dump header contains flag bits this crate does not understand.
    #[error("unsupported bytecode dump flags 0x{0:02x}")]
    UnsupportedFlags(u32),

    /// A prototype header contains flag bits this crate does not understand.
    #[error("unsupported prototype flags 0x{0:02x}")]
    UnsupportedProtoFlags(u8),

    /// Reading past the end of the input.
    #[error("truncated input: need {needed} byte(s) at offset {offset}, {available} available")]
    Truncated {
        /// Offset the read started at.
        offset: usize,
        /// Number of bytes the read wanted.
        needed: usize,
        /// Number of bytes actually left in the input.
        available: usize,
    },

    /// The dump is structurally inconsistent.
    #[error("malformed bytecode dump: {0}")]
    Malformed(String),

    /// The dump uses a LuaJIT feature this crate deliberately does not handle.
    #[error("unsupported bytecode feature: {0}")]
    Unsupported(String),

    /// A rewriting pass reached a state it cannot handle.
    ///
    /// This reports a limitation of the decompiler rather than a problem with
    /// the input: the bytecode is valid, but the pass could not make sense of
    /// it. It is worth reporting when one is seen.
    #[error("internal decompiler error: {0}")]
    Internal(String),

    /// A pass failed while decompiling one function of the chunk.
    #[error("decompilation failed in {function}: {reason}")]
    DecompilationFailed {
        /// Name of the function (as far as it could be determined).
        function: String,
        /// Human readable reason.
        reason: String,
    },
}
