//! Builds an AST with a control flow graph from a prototype.
//!
//! This is a port of ljd's `ast/builder.py`. The instructions of a prototype
//! are split into basic blocks that end in a "warp" (a branch, a loop head or a
//! fallthrough), and every remaining instruction becomes a statement.
//!
//! Three repair passes run first, because LuaJIT's code generator produces a
//! few instruction sequences that would otherwise be misread: inverted
//! comparisons, `repeat until true` loops and a broken form of unary
//! expression assignment.

use std::collections::HashMap;

use oxc_allocator::{Allocator, ArenaBox, ArenaVec};

use super::nodes::*;
use super::traverse;
use crate::bytecode::constants::NumConst;
use crate::bytecode::debuginfo::VariableInfo;
use crate::bytecode::opcodes::{Mode, Opcode};
use crate::bytecode::{Chunk, Const, ConstKey, Ins, Prototype};
use crate::error::{Error, Result};

// MARK: instruction tables

/// Instructions that end a block and encode a jump target.
const JUMP_WARP_INSTRUCTIONS: [Opcode; 5] = [
    Opcode::UCLO,
    Opcode::ISNEXT,
    Opcode::JMP,
    Opcode::FORI,
    Opcode::JFORI,
];

/// Instructions that end a block.
const WARP_INSTRUCTIONS: [Opcode; 12] = [
    Opcode::UCLO,
    Opcode::ISNEXT,
    Opcode::JMP,
    Opcode::FORI,
    Opcode::JFORI,
    Opcode::FORL,
    Opcode::IFORL,
    Opcode::JFORL,
    Opcode::ITERL,
    Opcode::IITERL,
    Opcode::JITERL,
    Opcode::LOOP,
];

// MARK: sentinel slots

/// Register slot number that stands for the constant `true`.
const SLOT_TRUE: u32 = 2_000_000_001;

/// Register slot number that stands for the constant `false`.
#[allow(dead_code)]
const SLOT_FALSE: u32 = 2_000_000_000;

// MARK: warp predicates

fn is_jump_warp(op: Opcode) -> bool {
    JUMP_WARP_INSTRUCTIONS.contains(&op)
}

fn is_warp(op: Opcode) -> bool {
    WARP_INSTRUCTIONS.contains(&op)
}

// MARK: builder

/// Mutable state of one prototype while it is being built.
struct Builder<'a> {
    /// The arena every node of this function goes into.
    alloc: &'a Allocator,
    chunk: &'a Chunk<'a>,
    prototype: &'a Prototype<'a>,
    /// Instructions of the prototype; the repair passes insert into this copy.
    instructions: Vec<Ins>,
    /// Source line for every instruction address; mutated by the repair passes.
    line_map: Vec<u32>,
    /// Local variable info; mutated by the repair passes.
    variables: Vec<VariableInfo<'a>>,
    blocks: Vec<NodeRef<'a>>,
    block_starts: HashMap<u32, NodeRef<'a>>,
    /// Number of trailing instructions consumed by the warp that was just
    /// built.
    warp_shift: u32,
}

/// Builds the AST of the root prototype of a chunk into `alloc`.
pub fn build<'a>(alloc: &'a Allocator, chunk: &'a Chunk<'a>) -> Result<NodeRef<'a>> {
    build_function(alloc, chunk, chunk.root)
}

/// Builds the AST of a single prototype.
pub fn build_function<'a>(
    alloc: &'a Allocator,
    chunk: &'a Chunk<'a>,
    prototype: &'a Prototype<'a>,
) -> Result<NodeRef<'a>> {
    let mut builder = Builder {
        alloc,
        chunk,
        prototype,
        instructions: prototype.instructions.to_vec(),
        line_map: prototype.debug.addr_to_line.to_vec(),
        variables: prototype.debug.variable_info.to_vec(),
        blocks: Vec::new(),
        block_starts: HashMap::new(),
        warp_shift: 0,
    };

    builder.build_function_definition(prototype)
}

impl<'a> Builder<'a> {
    // MARK: node construction

    /// Allocates a node in the arena this builder writes to.
    fn node(&self, value: Node<'a>) -> NodeRef<'a> {
        node(self.alloc, value)
    }

    /// Allocates a payload struct in the arena this builder writes to.
    fn payload<T>(&self, value: T) -> ArenaBox<'a, T> {
        ArenaBox::new_in(value, &self.alloc)
    }

    // MARK: entry points

    fn build_function_definition(&mut self, prototype: &Prototype<'a>) -> Result<NodeRef<'a>> {
        let mut arguments = Vec::new();
        for slot in 0..u32::from(prototype.num_params) {
            arguments.push(self.build_slot(0, slot));
        }
        if prototype.flags.is_variadic() {
            arguments.push(self.node(Node::Vararg));
        }

        let statements = self.build_function_blocks()?;
        let arguments = identifiers(self.alloc, arguments);
        let definition = FunctionDefinition {
            arguments,
            statements,
            upvalues: ArenaVec::from_iter_in(
                prototype.constants.upvalue_refs.iter().copied(),
                &self.alloc,
            ),
            debug: prototype.debug,
            instruction_count: prototype.instructions.len(),
            meta: Meta::default(),
        };

        Ok(self.node(Node::FunctionDefinition(self.payload(definition))))
    }

    fn build_function_blocks(&mut self) -> Result<NodeRef<'a>> {
        self.blockenize()?;
        self.establish_warps()?;

        self.set_warpins_count(0, 1);

        let blocks = self.blocks.clone();
        let mut previous: Option<NodeRef> = None;

        for block in blocks {
            let (first, last, last_body) = self.block_addresses(block);
            self.fill_block(block, first, last, last_body)?;

            // A block that is empty, jumps away and directly follows a
            // conditional branch is an empty `then` body.
            let empty_jump_block = {
                let borrowed = block.borrow();
                match &*borrowed {
                    Node::Block(block) => {
                        block.contents.is_empty()
                            && block.warp.as_ref().is_some_and(|warp| {
                                matches!(&*warp.borrow(), Node::UnconditionalWarp(w)
                                    if w.kind == UnconditionalWarpKind::Jump)
                            })
                    }
                    _ => false,
                }
            };
            let previous_is_conditional = previous.as_ref().is_some_and(|previous| {
                matches!(&*previous.borrow(), Node::Block(block)
                    if block.warp.as_ref()
                        .is_some_and(|warp| matches!(&*warp.borrow(), Node::ConditionalWarp(_))))
            });

            if empty_jump_block && previous_is_conditional {
                self.create_no_op(first, block);
            }

            previous = Some(block);
        }

        Ok(statements(self.alloc, self.blocks.iter().copied()))
    }

    /// Turns the instructions of a block into statements.
    fn fill_block(
        &mut self,
        block: NodeRef<'a>,
        first: u32,
        last: u32,
        last_body: u32,
    ) -> Result<()> {
        for addr in first..=last {
            let instruction = self.instructions[addr as usize];

            // Loop markers are searched up to `last`, not `last_body`,
            // otherwise a trailing `FORI` would be missed.
            let is_loop_marker = (Opcode::LOOP..=Opcode::JLOOP).contains(&instruction.op)
                || (Opcode::FORI..=Opcode::JFORI).contains(&instruction.op);
            if is_loop_marker {
                self.mark_loop(block);
            }

            if addr > last_body {
                continue;
            }

            if let Some((statement, marked)) = self.build_statement(addr, instruction)? {
                let meta = Meta::new(addr, self.line_of(addr));
                for element in marked.iter().copied().chain(std::iter::once(statement)) {
                    set_meta(element, meta);
                }
                if let Node::Block(inner) = &mut *block.borrow_mut() {
                    inner.contents.push(statement);
                }
            }
        }
        Ok(())
    }

    fn block_addresses(&self, block: NodeRef<'a>) -> (u32, u32, u32) {
        match &*block.borrow() {
            Node::Block(block) => (
                block.first_address,
                block.last_address,
                block.last_body_address,
            ),
            _ => unreachable!("only blocks are expected here"),
        }
    }

    fn set_warpins_count(&self, index: usize, count: u32) {
        if let Node::Block(block) = &mut *self.blocks[index].borrow_mut() {
            block.warpins_count = count;
        }
    }

    fn mark_loop(&self, block: NodeRef<'a>) {
        if let Node::Block(inner) = &mut *block.borrow_mut() {
            inner.is_loop = true;
        }
    }

    fn line_of(&self, addr: u32) -> u32 {
        self.line_map.get(addr as usize).copied().unwrap_or(0)
    }

    fn kgc(&self, index: u32) -> Result<&'a Const<'a>> {
        let prototype = self.prototype;
        prototype
            .constants
            .kgc_at(index)
            .ok_or_else(|| self.malformed(&format!("constant {index} does not exist")))
    }

    fn knum(&self, index: u32) -> Result<NumConst> {
        self.prototype
            .constants
            .knum_at(index)
            .ok_or_else(|| self.malformed(&format!("number constant {index} does not exist")))
    }

    fn malformed(&self, what: &str) -> Error {
        Error::DecompilationFailed {
            function: self.chunk.header.chunk_name().to_string(),
            reason: what.to_string(),
        }
    }

    // MARK: block splitting

    fn blockenize(&mut self) -> Result<()> {
        self.fix_inverted_comparison_expressions();
        self.fix_broken_repeat_until_loops()?;
        self.fix_broken_unary_expressions()?;

        let mut last_addresses: Vec<u32> = Vec::new();
        let mut addr = 1u32;

        while (addr as usize) < self.instructions.len() {
            let instruction = self.instructions[addr as usize];
            if !is_warp(instruction.op) {
                addr += 1;
                continue;
            }

            if is_jump_warp(instruction.op) {
                let destination = self.jump_destination(addr, &instruction)?;
                // A `UCLO` that does not branch is not a block boundary.
                if instruction.op != Opcode::UCLO || destination != addr + 1 {
                    if destination == 0 {
                        return Err(self.malformed("jump to the function header"));
                    }
                    last_addresses.push(destination - 1);
                    last_addresses.push(addr);
                }
            } else {
                last_addresses.push(addr);
            }

            addr += 1;
        }

        last_addresses.sort_unstable();
        last_addresses.dedup();
        last_addresses.push(self.instructions.len() as u32 - 1);

        // A jump to address 0 would create a block holding only the function
        // header, which is never useful.
        if last_addresses.first() == Some(&0) {
            last_addresses.remove(0);
        }

        let mut previous_last_address = 0u32;
        for (index, last_address) in last_addresses.into_iter().enumerate() {
            let block = self.node(Node::Block(self.payload(Block::new(
                self.alloc,
                index as u32,
                previous_last_address + 1,
                last_address,
            ))));
            self.blocks.push(block);
            self.block_starts.insert(previous_last_address + 1, block);
            previous_last_address = last_address;
        }

        Ok(())
    }

    fn establish_warps(&mut self) -> Result<()> {
        self.set_warpins_count(0, 1);

        // Building a warp may insert a placeholder block, so the loop has to
        // walk a snapshot of the block list rather than indices into it.
        let count = self.blocks.len();
        let blocks: Vec<NodeRef<'a>> = self.blocks[..count.saturating_sub(1)].to_vec();
        for block in blocks {
            let (first, last, _) = self.block_addresses(block);

            let start_addr = std::cmp::max(last - 1, first);
            let end_addr = last + 1;
            let warp = self.build_warp(last, start_addr, end_addr)?;
            let shift = self.warp_shift;
            let last_body = last - shift;

            if let Node::Block(inner) = &mut *block.borrow_mut() {
                inner.last_body_address = last_body;
                inner.warp = Some(warp);
            }
            set_meta(warp, Meta::new(last_body + 1, 0));
        }

        let last_block = self
            .blocks
            .last()
            .copied()
            .ok_or_else(|| self.malformed("function without blocks"))?;
        let (_, last, _) = self.block_addresses(last_block);
        let end_warp = self.node(Node::EndWarp(self.payload(EndWarp {
            target: None,
            meta: Meta::new(last, 0),
        })));
        if let Node::Block(inner) = &mut *last_block.borrow_mut() {
            inner.last_body_address = last;
            inner.warp = Some(end_warp);
        }

        Ok(())
    }

    /// Builds the warp of the block that ends at `last_addr`, from the one or
    /// two instructions that end the block.
    fn build_warp(
        &mut self,
        last_addr: u32,
        start_addr: u32,
        end_addr: u32,
    ) -> Result<NodeRef<'a>> {
        let last = self.instructions[last_addr as usize];
        let span = (end_addr - start_addr) as usize;

        if matches!(last.op, Opcode::JMP | Opcode::UCLO | Opcode::ISNEXT) {
            self.build_jump_warp(last_addr, span)
        } else if (Opcode::ITERL..=Opcode::JITERL).contains(&last.op) {
            if span != 2 {
                return Err(self.malformed("iterator loop warp without an iterator call"));
            }
            self.build_iterator_warp(last_addr)
        } else if (Opcode::FORL..=Opcode::JFORL).contains(&last.op) {
            self.build_numeric_loop_warp(last_addr, last)
        } else {
            self.build_flow_warp(last_addr, last)
        }
    }

    fn build_jump_warp(&mut self, last_addr: u32, span: usize) -> Result<NodeRef<'a>> {
        let last = self.instructions[last_addr as usize];
        let previous = if span == 1 {
            None
        } else {
            Some(self.instructions[(last_addr - 1) as usize].op)
        };

        let is_conditional = previous.is_some_and(|op| (op as u32) <= Opcode::ISF as u32);
        if is_conditional {
            if last.op == Opcode::ISNEXT {
                return Err(self.malformed("ISNEXT used as a comparison"));
            }
            self.build_conditional_warp(last_addr)
        } else {
            self.build_unconditional_warp(last_addr, last)
        }
    }

    fn build_conditional_warp(&mut self, last_addr: u32) -> Result<NodeRef<'a>> {
        let condition_addr = last_addr - 1;
        let condition = self.instructions[condition_addr as usize];
        let jump = self.instructions[last_addr as usize];

        let (expression, slot) = if matches!(condition.op, Opcode::ISTC | Opcode::ISFC) {
            (
                self.build_unary_expression(condition_addr, &condition)?,
                Some(condition.a),
            )
        } else if condition.op >= Opcode::IST {
            (
                self.build_unary_expression(condition_addr, &condition)?,
                Some(condition.cd),
            )
        } else {
            // A comparison keeps its result in no register at all.
            (
                self.build_comparison_expression(condition_addr, &condition)?,
                None,
            )
        };

        let destination = self.jump_destination(last_addr, &jump)?;
        let false_target = self.warp_in_block(destination)?;
        let original_true_target = self.warp_in_block(last_addr + 1)?;

        // A jump to the very next instruction means an empty `then` or `else`
        // body; a placeholder block keeps the shape of the graph intact.
        let true_target = if destination == last_addr + 1
            && !matches!(condition.op, Opcode::ISTC | Opcode::ISFC)
        {
            let block = self.node(Node::Block(self.payload(Block::new(
                self.alloc,
                block_index(original_true_target),
                last_addr + 1,
                last_addr + 1,
            ))));
            let flow = self.node(Node::UnconditionalWarp(self.payload(UnconditionalWarp {
                kind: UnconditionalWarpKind::Flow,
                target: Some(original_true_target),
                is_uclo: false,
                meta: Meta::new(last_addr, 0),
            })));
            if let Node::Block(inner) = &mut *block.borrow_mut() {
                inner.warpins_count = 1;
                // The placeholder holds no instructions of its own: its body
                // ends before it starts, exactly like an empty branch.
                inner.last_body_address = last_addr - 1;
                inner.warp = Some(flow);
            }

            let position = self
                .blocks
                .iter()
                .position(|candidate| traverse::same_node(candidate, original_true_target))
                .ok_or_else(|| self.malformed("conditional target is not a block"))?;
            self.blocks.insert(position, block);
            self.create_no_op(last_addr, block);
            block
        } else {
            original_true_target
        };

        self.warp_shift = 2;
        Ok(
            self.node(Node::ConditionalWarp(self.payload(ConditionalWarp {
                condition: Some(expression),
                true_target: Some(true_target),
                false_target: Some(false_target),
                slot,
                meta: Meta::default(),
            }))),
        )
    }

    fn build_unconditional_warp(&mut self, addr: u32, instruction: Ins) -> Result<NodeRef<'a>> {
        let is_uclo = instruction.op == Opcode::UCLO;
        if is_uclo && instruction.jump_offset() == 0 {
            return self.build_flow_warp(addr, instruction);
        }
        let destination = self.jump_destination(addr, &instruction)?;
        let target = self.warp_in_block(destination)?;

        self.warp_shift = 1;
        Ok(
            self.node(Node::UnconditionalWarp(self.payload(UnconditionalWarp {
                kind: UnconditionalWarpKind::Jump,
                target: Some(target),
                is_uclo,
                meta: Meta::default(),
            }))),
        )
    }

    fn build_iterator_warp(&mut self, last_addr: u32) -> Result<NodeRef<'a>> {
        let iterator_addr = last_addr - 1;
        let iterator = self.instructions[iterator_addr as usize];
        if !matches!(iterator.op, Opcode::ITERC | Opcode::ITERN) {
            return Err(self.malformed("iterator loop without an iterator call"));
        }

        let base = iterator.a;
        let controls = expressions(
            self.alloc,
            vec![
                self.build_slot(iterator_addr, base.wrapping_sub(3)),
                self.build_slot(iterator_addr, base.wrapping_sub(2)),
                self.build_slot(iterator_addr, base.wrapping_sub(1)),
            ],
        );

        let last_slot = base + iterator.b - 2;
        let mut loop_variables = Vec::new();
        for slot in base..=last_slot {
            loop_variables.push(self.build_slot(iterator_addr - 1, slot));
        }

        let jump = self.instructions[last_addr as usize];
        let destination = self.jump_destination(last_addr, &jump)?;
        let way_out = self.warp_in_block(last_addr + 1)?;
        let body = self.warp_in_block(destination)?;

        self.warp_shift = 2;
        Ok(self.node(Node::IteratorWarp(self.payload(IteratorWarp {
            variables: variables(self.alloc, loop_variables),
            controls,
            body: Some(body),
            way_out: Some(way_out),
            meta: Meta::default(),
        }))))
    }

    fn build_numeric_loop_warp(&mut self, addr: u32, instruction: Ins) -> Result<NodeRef<'a>> {
        let base = instruction.a;
        let index = self.build_slot(addr, base + 3);
        let controls = expressions(
            self.alloc,
            vec![
                self.build_slot(addr, base),
                self.build_slot(addr, base + 1),
                self.build_slot(addr, base + 2),
            ],
        );

        let destination = self.jump_destination(addr, &instruction)?;
        let body = self.warp_in_block(destination)?;
        let way_out = self.warp_in_block(addr + 1)?;

        self.warp_shift = 1;
        Ok(
            self.node(Node::NumericLoopWarp(self.payload(NumericLoopWarp {
                index,
                controls,
                body: Some(body),
                way_out: Some(way_out),
                meta: Meta::default(),
            }))),
        )
    }

    fn build_flow_warp(&mut self, addr: u32, instruction: Ins) -> Result<NodeRef<'a>> {
        let target = self.warp_in_block(addr + 1)?;
        self.warp_shift = if matches!(instruction.op, Opcode::FORI | Opcode::UCLO) {
            1
        } else {
            0
        };
        Ok(
            self.node(Node::UnconditionalWarp(self.payload(UnconditionalWarp {
                kind: UnconditionalWarpKind::Flow,
                target: Some(target),
                is_uclo: false,
                meta: Meta::default(),
            }))),
        )
    }

    fn warp_in_block(&mut self, addr: u32) -> Result<NodeRef<'a>> {
        let block = self.block_starts.get(&addr).copied().ok_or_else(|| {
            self.malformed(&format!("branch target {addr} is not the start of a block"))
        })?;
        if let Node::Block(inner) = &mut *block.borrow_mut() {
            inner.warpins_count += 1;
        }
        Ok(block)
    }

    fn jump_destination(&self, addr: u32, instruction: &Ins) -> Result<u32> {
        let target = instruction.jump_target(addr);
        u32::try_from(target).map_err(|_| self.malformed("jump target is outside the function"))
    }

    fn create_no_op(&self, addr: u32, block: NodeRef<'a>) {
        let statement = self.node(Node::NoOp(self.payload(NoOp {
            meta: Meta::new(addr, self.line_of(addr)),
        })));
        if let Node::Block(inner) = &mut *block.borrow_mut() {
            inner.contents.push(statement);
        }
    }

    // MARK: statements

    fn build_statement(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<Option<(NodeRef<'a>, Vec<NodeRef<'a>>)>> {
        if matches!(instruction.def().a, Mode::Dst | Mode::Uv) {
            return self.build_var_assignment(addr, instruction).map(Some);
        }

        match instruction.op {
            Opcode::KNIL => self.build_knil(addr, instruction).map(Some),
            Opcode::GSET => self.build_global_assignment(addr, instruction).map(Some),
            Opcode::TSETV | Opcode::TSETS | Opcode::TSETB | Opcode::TSETR => {
                self.build_table_assignment(addr, instruction).map(Some)
            }
            Opcode::TSETM => self
                .build_table_mass_assignment(addr, instruction)
                .map(Some),
            Opcode::CALLM | Opcode::CALL | Opcode::CALLMT | Opcode::CALLT => {
                self.build_call(addr, instruction)
            }
            Opcode::VARG => self.build_vararg(addr, instruction).map(Some),
            Opcode::RETM | Opcode::RET | Opcode::RET0 | Opcode::RET1 => {
                self.build_return(addr, instruction).map(Some)
            }
            // `UCLO` and the loop markers only produce warps.
            _ => Ok(None),
        }
    }

    fn build_var_assignment(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<(NodeRef<'a>, Vec<NodeRef<'a>>)> {
        let op = instruction.op;

        let expression = if op.is_unary() {
            self.build_unary_expression(addr, &instruction)?
        } else if (Opcode::ADDVN..=Opcode::POW).contains(&op) {
            self.build_binary_expression(addr, &instruction)?
        } else if op.is_bitop() {
            self.build_bitop_expression(addr, &instruction)?
        } else if op == Opcode::CAT {
            self.build_concat_expression(addr, &instruction)?
        } else if (Opcode::KSTR..=Opcode::KPRI).contains(&op) {
            self.build_const_expression(&instruction)?
        } else if op == Opcode::UGET {
            self.build_upvalue(addr, instruction.cd)
        } else if op == Opcode::USETV {
            self.build_slot(addr, instruction.cd)
        } else if (Opcode::USETS..=Opcode::USETP).contains(&op) {
            self.build_const_expression(&instruction)?
        } else if op == Opcode::FNEW {
            self.build_child(instruction.cd)?
        } else if op == Opcode::TNEW {
            self.node(Node::TableConstructor(self.payload(TableConstructor {
                array: records(self.alloc, Vec::new()),
                records: records(self.alloc, Vec::new()),
                meta: Meta::default(),
            })))
        } else if op == Opcode::TDUP {
            self.build_table_copy(instruction.cd)?
        } else if op == Opcode::GGET {
            self.build_global_variable(instruction.cd)
        } else if op.is_table_get() {
            self.build_table_element(addr, &instruction)?
        } else {
            return Err(self.malformed(&format!(
                "unsupported assignment instruction {} at address {addr}",
                op.name()
            )));
        };

        let destination = if instruction.def().a == Mode::Uv {
            self.build_upvalue(addr, instruction.a)
        } else {
            self.build_slot(addr, instruction.a)
        };

        let assignment = Assignment {
            expressions: expressions(self.alloc, vec![expression]),
            destinations: variables(self.alloc, vec![destination]),
            kind: AssignmentKind::Normal,
            meta: Meta::default(),
        };

        Ok((
            self.node(Node::Assignment(self.payload(assignment))),
            vec![expression],
        ))
    }

    fn build_knil(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<(NodeRef<'a>, Vec<NodeRef<'a>>)> {
        let mut assignment = self.build_range_assignment(addr, instruction.a, instruction.cd);
        let primitive = primitive(self.alloc, PrimitiveKind::Nil);
        assignment.expressions = expressions(self.alloc, vec![primitive]);
        Ok((
            self.node(Node::Assignment(self.payload(assignment))),
            vec![primitive],
        ))
    }

    fn build_global_assignment(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<(NodeRef<'a>, Vec<NodeRef<'a>>)> {
        let variable = self.build_global_variable(instruction.cd);
        let expression = self.build_slot(addr, instruction.a);
        let assignment = Assignment {
            expressions: expressions(self.alloc, vec![expression]),
            destinations: variables(self.alloc, vec![variable]),
            kind: AssignmentKind::Normal,
            meta: Meta::default(),
        };
        Ok((
            self.node(Node::Assignment(self.payload(assignment))),
            vec![expression],
        ))
    }

    fn build_table_assignment(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<(NodeRef<'a>, Vec<NodeRef<'a>>)> {
        let destination = self.build_table_element(addr, &instruction)?;
        let expression = self.build_slot(addr, instruction.a);
        let assignment = Assignment {
            expressions: expressions(self.alloc, vec![expression]),
            destinations: variables(self.alloc, vec![destination]),
            kind: AssignmentKind::Normal,
            meta: Meta::default(),
        };
        Ok((
            self.node(Node::Assignment(self.payload(assignment))),
            vec![expression],
        ))
    }

    fn build_table_mass_assignment(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<(NodeRef<'a>, Vec<NodeRef<'a>>)> {
        let base = instruction.a;
        let table = self.build_slot(addr, base.wrapping_sub(1));
        let destination = self.node(Node::TableElement(self.payload(TableElement {
            table,
            key: self.node(Node::MulTres),
            meta: Meta::default(),
        })));
        let multres = self.node(Node::MulTres);
        let assignment = Assignment {
            expressions: expressions(self.alloc, vec![multres]),
            destinations: variables(self.alloc, vec![destination]),
            kind: AssignmentKind::Normal,
            meta: Meta::default(),
        };
        Ok((
            self.node(Node::Assignment(self.payload(assignment))),
            vec![multres],
        ))
    }

    fn build_call(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<Option<(NodeRef<'a>, Vec<NodeRef<'a>>)>> {
        let function = self.build_slot(addr, instruction.a);
        let arguments = self.build_call_arguments(addr, &instruction);
        let call = self.node(Node::FunctionCall(self.payload(FunctionCall {
            function,
            arguments,
            is_method: false,
            meta: Meta::default(),
        })));

        let mut marked = vec![call];

        let statement = if instruction.op <= Opcode::CALL {
            if instruction.b == 0 {
                let destinations = variables(self.alloc, vec![self.node(Node::MulTres)]);
                let assignment = Assignment {
                    expressions: expressions(self.alloc, vec![call]),
                    destinations,
                    kind: AssignmentKind::Normal,
                    meta: Meta::default(),
                };
                self.node(Node::Assignment(self.payload(assignment)))
            } else if instruction.b == 1 {
                call
            } else {
                let from_slot = instruction.a;
                let to_slot = instruction.a + instruction.b - 2;
                let mut assignment = self.build_range_assignment(addr, from_slot, to_slot);
                assignment.expressions = expressions(self.alloc, vec![call]);
                self.node(Node::Assignment(self.payload(assignment)))
            }
        } else {
            let returns = expressions(self.alloc, vec![call]);
            let return_node = self.node(Node::Return(self.payload(Return {
                returns,
                meta: Meta::default(),
            })));
            marked.push(return_node);
            return_node
        };

        Ok(Some((statement, marked)))
    }

    fn build_call_arguments(&mut self, addr: u32, instruction: &Ins) -> NodeRef<'a> {
        let base = instruction.a;
        let is_variadic = matches!(instruction.op, Opcode::CALLM | Opcode::CALLMT);
        let mut last_argument_slot = i64::from(base) + i64::from(instruction.cd);
        if !is_variadic {
            last_argument_slot -= 1;
        }

        let mut slot = i64::from(base) + 1;

        // In GC64 mode every call frame reserves a slot right after the
        // function, which the runtime uses to store metadata.
        if self.chunk.header.flags.fr2 {
            slot += 1;
            last_argument_slot += 1;
        }

        let mut arguments = Vec::new();
        while slot <= last_argument_slot {
            arguments.push(self.build_slot(addr, slot as u32));
            slot += 1;
        }
        if is_variadic {
            arguments.push(self.node(Node::MulTres));
        }

        expressions(self.alloc, arguments)
    }

    fn build_vararg(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<(NodeRef<'a>, Vec<NodeRef<'a>>)> {
        let base = instruction.a;
        let last_slot = i64::from(base) + i64::from(instruction.b) - 2;
        let vararg = self.node(Node::Vararg);

        let assignment = if last_slot < i64::from(base) {
            let destinations = variables(self.alloc, vec![self.node(Node::MulTres)]);
            Assignment {
                expressions: expressions(self.alloc, vec![vararg]),
                destinations,
                kind: AssignmentKind::Normal,
                meta: Meta::default(),
            }
        } else {
            let mut assignment = self.build_range_assignment(addr, base, last_slot as u32);
            assignment.expressions = expressions(self.alloc, vec![vararg]);
            assignment
        };

        Ok((
            self.node(Node::Assignment(self.payload(assignment))),
            vec![vararg],
        ))
    }

    fn build_return(
        &mut self,
        addr: u32,
        instruction: Ins,
    ) -> Result<(NodeRef<'a>, Vec<NodeRef<'a>>)> {
        let base = instruction.a;
        let mut last_slot = i64::from(base) + i64::from(instruction.cd) - 1;
        if instruction.op != Opcode::RETM {
            last_slot -= 1;
        }

        let mut returns = Vec::new();
        let mut slot = i64::from(base);
        while slot <= last_slot {
            returns.push(self.build_slot(addr, slot as u32));
            slot += 1;
        }
        if instruction.op == Opcode::RETM {
            returns.push(self.node(Node::MulTres));
        }

        let returns_node = expressions(self.alloc, returns.iter().copied());
        let return_node = self.node(Node::Return(self.payload(Return {
            returns: returns_node,
            meta: Meta::default(),
        })));

        let mut marked = vec![return_node];
        marked.extend(returns);
        Ok((return_node, marked))
    }

    fn build_range_assignment(
        &mut self,
        addr: u32,
        from_slot: u32,
        to_slot: u32,
    ) -> Assignment<'a> {
        let mut destinations = Vec::new();
        let mut slot = from_slot;
        while slot <= to_slot {
            destinations.push(self.build_slot(addr, slot));
            slot += 1;
        }
        Assignment {
            expressions: expressions(self.alloc, Vec::new()),
            destinations: variables(self.alloc, destinations),
            kind: AssignmentKind::Normal,
            meta: Meta::default(),
        }
    }

    // MARK: expressions

    fn build_binary_expression(&mut self, addr: u32, instruction: &Ins) -> Result<NodeRef<'a>> {
        let kind = binary_operator_kind(instruction.op)
            .ok_or_else(|| self.malformed("unknown binary operator"))?;

        let (left, right) = if instruction.op.is_vn_arith() {
            (
                self.build_slot(addr, instruction.b),
                self.build_numeric_constant(instruction.cd)?,
            )
        } else if instruction.op.is_nv_arith() {
            (
                self.build_numeric_constant(instruction.cd)?,
                self.build_slot(addr, instruction.b),
            )
        } else {
            (
                self.build_slot(addr, instruction.b),
                self.build_slot(addr, instruction.cd),
            )
        };

        Ok(self.node(Node::BinaryOperator(self.payload(BinaryOperator {
            kind,
            left,
            right,
            meta: Meta::default(),
        }))))
    }

    fn build_bitop_expression(&mut self, addr: u32, instruction: &Ins) -> Result<NodeRef<'a>> {
        let kind = match instruction.op {
            Opcode::BNOT => {
                let operand = self.build_slot(addr, instruction.cd);
                return Ok(self.node(Node::UnaryOperator(self.payload(UnaryOperator {
                    kind: UnaryOperatorKind::BitNot,
                    operand,
                    meta: Meta::default(),
                }))));
            }
            Opcode::BAND => BinaryOperatorKind::BitAnd,
            Opcode::BOR => BinaryOperatorKind::BitOr,
            Opcode::BXOR => BinaryOperatorKind::BitXor,
            Opcode::BSHL => BinaryOperatorKind::ShiftLeft,
            Opcode::BSHR => BinaryOperatorKind::ShiftRight,
            Opcode::BSAR => BinaryOperatorKind::ShiftArithmeticRight,
            _ => return Err(self.malformed("unknown bit operator")),
        };

        let left = self.build_slot(addr, instruction.b);
        let right = self.build_slot(addr, instruction.cd);
        Ok(self.node(Node::BinaryOperator(self.payload(BinaryOperator {
            kind,
            left,
            right,
            meta: Meta::default(),
        }))))
    }

    fn build_concat_expression(&mut self, addr: u32, instruction: &Ins) -> Result<NodeRef<'a>> {
        let mut slot = instruction.b;
        let mut operator = BinaryOperator {
            kind: BinaryOperatorKind::Concat,
            left: self.build_slot(addr, slot),
            right: self.build_slot(addr, slot + 1),
            meta: Meta::default(),
        };
        slot += 2;

        while slot <= instruction.cd {
            let left = self.node(Node::BinaryOperator(self.payload(operator)));
            let right = self.build_slot(addr, slot);
            operator = BinaryOperator {
                kind: BinaryOperatorKind::Concat,
                left,
                right,
                meta: Meta::default(),
            };
            slot += 1;
        }

        Ok(self.node(Node::BinaryOperator(self.payload(operator))))
    }

    fn build_const_expression(&mut self, instruction: &Ins) -> Result<NodeRef<'a>> {
        match instruction.def().c {
            Mode::Str => self.build_string_constant(instruction.cd),
            Mode::CData => self.build_cdata_constant(instruction.cd),
            Mode::LitS => Ok(self.build_literal(i64::from(instruction.lits()))),
            Mode::Lit => Ok(self.build_literal(i64::from(instruction.cd))),
            Mode::Num => self.build_numeric_constant(instruction.cd),
            Mode::Pri => Ok(self.build_primitive(u64::from(instruction.cd))),
            _ => Err(self.malformed("unsupported constant operand")),
        }
    }

    fn build_table_element(&mut self, addr: u32, instruction: &Ins) -> Result<NodeRef<'a>> {
        let table = self.build_slot(addr, instruction.b);
        let key = if instruction.def().c == Mode::Var {
            self.build_slot(addr, instruction.cd)
        } else {
            self.build_const_expression(instruction)?
        };
        Ok(self.node(Node::TableElement(self.payload(TableElement {
            table,
            key,
            meta: Meta::default(),
        }))))
    }

    fn build_comparison_expression(&mut self, addr: u32, instruction: &Ins) -> Result<NodeRef<'a>> {
        let left = self.build_slot(addr, instruction.a);
        let right = match instruction.def().c {
            Mode::Str => self.build_string_constant(instruction.cd)?,
            Mode::Num => self.build_numeric_constant(instruction.cd)?,
            Mode::Pri => self.build_primitive(u64::from(instruction.cd)),
            _ => self.build_slot(addr, instruction.cd),
        };

        // The comparison opcodes are inverted: the branch taken when the
        // condition holds leads to the *false* target.
        let kind = match instruction.op {
            Opcode::ISLT => BinaryOperatorKind::GreaterOrEqual,
            Opcode::ISGE => BinaryOperatorKind::LessThan,
            Opcode::ISLE => BinaryOperatorKind::GreaterThan,
            Opcode::ISGT => BinaryOperatorKind::LessOrEqual,
            Opcode::ISEQV | Opcode::ISEQS | Opcode::ISEQN | Opcode::ISEQP => {
                BinaryOperatorKind::NotEqual
            }
            Opcode::ISNEV | Opcode::ISNES | Opcode::ISNEN | Opcode::ISNEP => {
                BinaryOperatorKind::Equal
            }
            _ => return Err(self.malformed("not a comparison instruction")),
        };

        Ok(self.node(Node::BinaryOperator(self.payload(BinaryOperator {
            kind,
            left,
            right,
            meta: Meta::default(),
        }))))
    }

    fn build_unary_expression(&mut self, addr: u32, instruction: &Ins) -> Result<NodeRef<'a>> {
        let variable = self.build_slot(addr, instruction.cd);

        // Mind the inversion: these copy the value unchanged and only test it.
        if matches!(instruction.op, Opcode::ISFC | Opcode::ISF | Opcode::MOV) {
            return Ok(variable);
        }

        let kind = match instruction.op {
            Opcode::ISTC | Opcode::IST | Opcode::NOT => UnaryOperatorKind::Not,
            Opcode::UNM => UnaryOperatorKind::Minus,
            Opcode::ISTYPE => UnaryOperatorKind::ToString,
            Opcode::ISNUM => UnaryOperatorKind::ToNumber,
            Opcode::LEN => UnaryOperatorKind::Length,
            _ => return Err(self.malformed("not a unary instruction")),
        };

        Ok(self.node(Node::UnaryOperator(self.payload(UnaryOperator {
            kind,
            operand: variable,
            meta: Meta::default(),
        }))))
    }

    fn build_child(&mut self, index: u32) -> Result<NodeRef<'a>> {
        let child = match self.kgc(index)? {
            Const::Child(child) => *child,
            _ => return Err(self.malformed("FNEW does not refer to a prototype")),
        };
        build_function(self.alloc, self.chunk, child)
    }

    fn build_table_copy(&mut self, index: u32) -> Result<NodeRef<'a>> {
        let table = match self.kgc(index)? {
            Const::Table(table) => table,
            _ => return Err(self.malformed("TDUP does not refer to a table")),
        };

        let mut array = Vec::new();
        for value in &table.array {
            let value = self.build_table_record_item(value);
            array.push(self.node(Node::ArrayRecord(self.payload(ArrayRecord {
                value,
                meta: Meta::default(),
            }))));
        }

        let mut hash = Vec::new();
        for (key, value) in &table.hash {
            hash.push(self.node(Node::TableRecord(self.payload(TableRecord {
                key: self.build_table_record_item(key),
                value: self.build_table_record_item(value),
                meta: Meta::default(),
            }))));
        }

        Ok(
            self.node(Node::TableConstructor(self.payload(TableConstructor {
                array: records(self.alloc, array),
                records: records(self.alloc, hash),
                meta: Meta::default(),
            }))),
        )
    }

    fn build_table_record_item(&self, value: &ConstKey<'a>) -> NodeRef<'a> {
        match value {
            ConstKey::Nil | ConstKey::KeyMarker => primitive(self.alloc, PrimitiveKind::Nil),
            ConstKey::False => primitive(self.alloc, PrimitiveKind::False),
            ConstKey::True => primitive(self.alloc, PrimitiveKind::True),
            ConstKey::Int(number) => self.node(Node::Constant(self.payload(Constant {
                value: ConstantValue::Integer(*number),
                meta: Meta::default(),
            }))),
            ConstKey::Float(number) => self.node(Node::Constant(self.payload(Constant {
                value: ConstantValue::Float(*number),
                meta: Meta::default(),
            }))),
            // A template table borrows its strings from the prototype instead
            // of copying them into the arena.
            ConstKey::Str(bytes) => self.node(Node::Constant(self.payload(Constant {
                value: ConstantValue::String(bytes),
                meta: Meta::default(),
            }))),
        }
    }

    fn build_slot(&self, addr: u32, slot: u32) -> NodeRef<'a> {
        let identifier =
            Identifier::new(self.alloc, IdentifierKind::Slot, slot, Meta::new(addr, 0));
        self.node(Node::Identifier(self.payload(identifier)))
    }

    fn build_upvalue(&self, addr: u32, slot: u32) -> NodeRef<'a> {
        let name = self.prototype.debug.upvalue_name(slot);
        let mut identifier = Identifier::new(
            self.alloc,
            IdentifierKind::Upvalue,
            slot,
            Meta::new(addr, 0),
        );
        identifier.name = name;
        self.node(Node::Identifier(self.payload(identifier)))
    }

    fn build_global_variable(&self, index: u32) -> NodeRef<'a> {
        let mut identifier =
            Identifier::new(self.alloc, IdentifierKind::Builtin, 0, Meta::default());
        identifier.name = Some("_env");
        let table = self.node(Node::Identifier(self.payload(identifier)));
        let empty = self.node(Node::Constant(self.payload(Constant {
            value: ConstantValue::String(&[]),
            meta: Meta::default(),
        })));
        let key = self.build_string_constant(index).unwrap_or(empty);
        self.node(Node::TableElement(self.payload(TableElement {
            table,
            key,
            meta: Meta::default(),
        })))
    }

    fn build_string_constant(&self, index: u32) -> Result<NodeRef<'a>> {
        match self.kgc(index)? {
            // The string is borrowed from the prototype's constants rather than
            // copied into the arena.
            Const::Str(bytes) => Ok(self.node(Node::Constant(self.payload(Constant {
                value: ConstantValue::String(bytes),
                meta: Meta::default(),
            })))),
            _ => Err(self.malformed("expected a string constant")),
        }
    }

    fn build_cdata_constant(&self, index: u32) -> Result<NodeRef<'a>> {
        let value = match self.kgc(index)? {
            Const::Int64(number) => number.to_string(),
            Const::Uint64(number) => number.to_string(),
            Const::Complex(real, imaginary) => format!("{real}+{imaginary}i"),
            _ => return Err(self.malformed("expected a cdata constant")),
        };
        let bytes = self.alloc.alloc_slice_copy(value.as_bytes());
        Ok(self.node(Node::Constant(self.payload(Constant {
            value: ConstantValue::CData(bytes),
            meta: Meta::default(),
        }))))
    }

    fn build_numeric_constant(&self, index: u32) -> Result<NodeRef<'a>> {
        let value = match self.knum(index)? {
            NumConst::Int(number) => ConstantValue::Integer(number),
            NumConst::Float(number) => ConstantValue::Float(number),
        };
        Ok(self.node(Node::Constant(self.payload(Constant {
            value,
            meta: Meta::default(),
        }))))
    }

    fn build_literal(&self, value: i64) -> NodeRef<'a> {
        self.node(Node::Constant(self.payload(Constant {
            value: ConstantValue::Integer(value as i32),
            meta: Meta::default(),
        })))
    }

    fn build_primitive(&self, value: u64) -> NodeRef<'a> {
        let kind = match value {
            1 => PrimitiveKind::False,
            2 => PrimitiveKind::True,
            _ => PrimitiveKind::Nil,
        };
        primitive(self.alloc, kind)
    }

    // MARK: instruction repair passes

    /// LuaJIT sometimes emits `constant < variable`; rewrite it as
    /// `variable > constant`.
    fn fix_inverted_comparison_expressions(&mut self) {
        for index in 0..self.instructions.len() {
            let instruction = self.instructions[index];
            if !(Opcode::ISLT..=Opcode::ISGT).contains(&instruction.op) {
                continue;
            }

            let left_slot = instruction.a;
            let right_slot = instruction.cd;

            let inverted = index > 0 && {
                let preceding = self.instructions[index - 1];
                preceding.a == left_slot
                    && ((Opcode::UNM..=Opcode::POW).contains(&preceding.op)
                        || (Opcode::KSHORT..=Opcode::KNUM).contains(&preceding.op))
            };

            if !inverted {
                continue;
            }

            let mut fixed = instruction;
            fixed.a = right_slot;
            fixed.cd = left_slot;
            fixed.op = match instruction.op {
                Opcode::ISGT => Opcode::ISLT,
                Opcode::ISGE => Opcode::ISLE,
                Opcode::ISLT => Opcode::ISGT,
                Opcode::ISLE => Opcode::ISGE,
                other => other,
            };
            self.instructions[index] = fixed;
        }
    }

    /// A `repeat ... until true` loop has no conditional jump back to the loop
    /// head; synthesise the missing branch so the loop can be recognised.
    fn fix_broken_repeat_until_loops(&mut self) -> Result<()> {
        let mut index = 1usize;
        while index < self.instructions.len() {
            let instruction = self.instructions[index];
            if instruction.op != Opcode::LOOP {
                index += 1;
                continue;
            }

            let loop_exit_addr = self.jump_destination(index as u32, &instruction)? as usize;
            let loop_condition_addr = loop_exit_addr - 1;
            let condition = self.instructions[loop_condition_addr];

            if condition.op == Opcode::JMP {
                index += 1;
                continue;
            }

            let destination =
                self.jump_destination(loop_condition_addr as u32, &condition)? as usize;
            if destination <= index {
                index += 1;
                continue;
            }

            let fixed_condition = Ins::new_ad(Opcode::ISF, 0, SLOT_TRUE);

            let mut fixed_jump = Ins::new_ad(Opcode::JMP, 0, 0);
            fixed_jump.set_jump_offset(index as i32 - loop_condition_addr as i32 - 1);

            let insertion_index = loop_condition_addr + 1;
            self.insert_instruction(insertion_index, fixed_jump);
            self.insert_instruction(insertion_index, fixed_condition);

            let shift = 2i32;

            // `break` statements in such a loop point at the same exit as
            // ordinary jumps, so the jumps have to be matched up carefully.
            let mut leading_jump = false;
            for j in (index + 1)..insertion_index {
                let checked = self.instructions[j];
                if checked.op != Opcode::JMP {
                    leading_jump = false;
                    continue;
                }

                if !leading_jump {
                    let destination = self.jump_destination(j as u32, &checked)? as usize;
                    if checked.jump_offset() >= shift
                        && destination == insertion_index + shift as usize
                    {
                        let next_index = j + 1;
                        let following = self.instructions[next_index];
                        if following.op == Opcode::JMP {
                            let following_destination =
                                self.jump_destination(next_index as u32, &following)? as usize;
                            if following_destination < destination {
                                leading_jump = true;
                                continue;
                            } else if following_destination == destination {
                                leading_jump = false;
                                continue;
                            }
                        }

                        let mut found_else_break = false;
                        let mut previous_jump = false;
                        for k in next_index..insertion_index {
                            let following = self.instructions[k];
                            if following.op == Opcode::JMP {
                                if !previous_jump {
                                    previous_jump = true;
                                } else {
                                    let following_destination =
                                        self.jump_destination(k as u32, &following)? as usize;
                                    if following.jump_offset() >= shift
                                        && following_destination == destination
                                    {
                                        found_else_break = true;
                                        break;
                                    }
                                    previous_jump = false;
                                }
                            } else {
                                if previous_jump {
                                    let last = self.instructions[k - 1];
                                    let last_destination =
                                        self.jump_destination((k - 1) as u32, &last)? as usize;
                                    if last_destination < destination {
                                        break;
                                    }
                                }
                                previous_jump = false;
                            }
                        }

                        if !found_else_break {
                            let mut fixed = self.instructions[j];
                            fixed.set_jump_offset(fixed.jump_offset() - shift);
                            self.instructions[j] = fixed;
                        }
                    }
                }
                leading_jump = true;
            }

            index += 1;
        }
        Ok(())
    }

    /// Repairs the `var = var <cmp> const and (op) var or var` sequence that
    /// LuaJIT emits for `x = a < b and c or d` style expressions.
    fn fix_broken_unary_expressions(&mut self) -> Result<()> {
        let mut index = 3usize;
        while index < self.instructions.len() {
            if index + 2 >= self.instructions.len() {
                break;
            }

            let instruction = self.instructions[index];
            let previous = self.instructions[index - 1];
            if instruction.op != Opcode::ISTC
                || !(Opcode::ADDVN..=Opcode::CAT).contains(&previous.op)
            {
                index += 1;
                continue;
            }

            // Look for a jump that precedes the broken condition.
            let mut leading_jump_index = None;
            for offset in 1..index {
                let candidate = self.instructions[index - offset];
                if candidate.op == Opcode::JMP {
                    leading_jump_index = Some(index - offset);
                    break;
                } else if !(Opcode::ADDVN..=Opcode::CAT).contains(&candidate.op) {
                    break;
                }
            }

            let condition_jump = self.instructions[index + 1];
            let destination = self.jump_destination((index + 1) as u32, &condition_jump)?;

            let Some(leading_index) = leading_jump_index else {
                index += 1;
                continue;
            };

            if destination != index as u32 + 2 {
                index += 1;
                continue;
            }

            let leading = self.instructions[leading_index];
            let leading_destination = self.jump_destination(leading_index as u32, &leading)?;

            let remove = if destination == leading_destination {
                true
            } else {
                // The expression sits in an `else` body, so one more jump is
                // involved.
                let extra = self.instructions[index + 2];
                extra.op == Opcode::JMP
                    && self.jump_destination((index + 2) as u32, &extra)? == leading_destination
            };

            if remove {
                let mut fixed = self.instructions[index - 1];
                fixed.a = instruction.a;
                self.instructions[index - 1] = fixed;
                self.remove_instruction(index + 1);
                self.remove_instruction(index);
            }

            index += 1;
        }
        Ok(())
    }

    fn insert_instruction(&mut self, index: usize, instruction: Ins) {
        let line = self.line_map[index - 1];
        self.instructions.insert(index, instruction);
        self.line_map.insert(index, line);
        self.shift_warp_destinations(1, index);
        self.shift_variable_info(1, index);
    }

    fn remove_instruction(&mut self, index: usize) -> Ins {
        let removed = self.instructions.remove(index);
        self.line_map.remove(index);
        self.shift_warp_destinations(-1, index);
        self.shift_variable_info(-1, index);
        removed
    }

    fn shift_warp_destinations(&mut self, shift: i32, modified_index: usize) {
        for (current_index, instruction) in self.instructions.iter_mut().enumerate() {
            if !is_warp(instruction.op) {
                continue;
            }
            // A non branching `UCLO` always falls through.
            if instruction.op == Opcode::UCLO && instruction.jump_offset() == 0 {
                continue;
            }

            let offset = instruction.jump_offset();
            if current_index < modified_index && offset >= 0 {
                let destination = current_index as i64 + i64::from(offset) + 1;
                if destination > modified_index as i64
                    || (destination == modified_index as i64 && shift > 0)
                {
                    instruction.set_jump_offset(offset + shift);
                }
            } else if current_index >= modified_index && offset < 0 {
                let destination = current_index as i64 + i64::from(offset) - i64::from(shift) + 1;
                if destination < modified_index as i64
                    || (destination == modified_index as i64 && shift > 0)
                {
                    instruction.set_jump_offset(offset - shift);
                }
            }
        }
    }

    fn shift_variable_info(&mut self, shift: i32, modified_index: usize) {
        for info in &mut self.variables {
            if info.end_addr as usize > modified_index
                || (shift > 0 && info.end_addr as usize == modified_index)
            {
                info.end_addr = (i64::from(info.end_addr) + i64::from(shift)) as u32;
            }
            if info.start_addr as usize > modified_index
                || (shift > 0 && info.start_addr as usize == modified_index)
            {
                info.start_addr = (i64::from(info.start_addr) + i64::from(shift)) as u32;
            }
        }
    }
}

// MARK: operator lookup

fn binary_operator_kind(op: Opcode) -> Option<BinaryOperatorKind> {
    use BinaryOperatorKind::*;
    Some(match op {
        Opcode::ADDVN | Opcode::ADDNV | Opcode::ADDVV => Add,
        Opcode::SUBVN | Opcode::SUBNV | Opcode::SUBVV => Subtract,
        Opcode::MULVN | Opcode::MULNV | Opcode::MULVV => Multiply,
        Opcode::DIVVN | Opcode::DIVNV | Opcode::DIVVV => Division,
        Opcode::MODVN | Opcode::MODNV | Opcode::MODVV => Mod,
        Opcode::POW => Pow,
        _ => return None,
    })
}

// MARK: block index

fn block_index(block: NodeRef<'_>) -> u32 {
    match &*block.borrow() {
        Node::Block(block) => block.index,
        _ => 0,
    }
}
