//! Turning a dump into a chunk.
//!
//! Tests keep the whole chunk alive while they inspect it, so the arena and the
//! chunk are leaked rather than threaded through every call site.

use luajit_ripper::bytecode::Chunk;

/// An allocator that lives for the rest of the program.
pub fn arena() -> &'static oxc_allocator::Allocator {
    Box::leak(Box::new(oxc_allocator::Allocator::default()))
}

/// Parses a dump into a chunk that lives for the rest of the program.
pub fn parse_dump(dump: &[u8]) -> &'static Chunk<'static> {
    try_parse_dump(dump).unwrap_or_else(|error| panic!("cannot parse the dump: {error}"))
}

/// Parses a dump, keeping the error instead of panicking.
pub fn try_parse_dump(dump: &[u8]) -> Result<&'static Chunk<'static>, luajit_ripper::Error> {
    let chunk = luajit_ripper::bytecode::parse(arena(), dump)?;
    Ok(Box::leak(Box::new(chunk)))
}
