//! Recovers local variables from the debug information.
//!
//! This is a port of ljd's `ast/locals.py`. The bytecode addresses of the
//! statements are used to look up which source variable lives in which
//! register at that point, so a slot identifier can be given the name the
//! author used.
//!
//! LuaJIT numbers registers, not variables: everything the compiler keeps on
//! the stack shares one register file, and a register holds different variables
//! over time. `mark_locals` therefore walks the AST in order, remembers every
//! register reference it has seen, and resolves them once the variable that
//! lives there becomes known. `mark_local_definitions` then decides which of
//! the resulting locals are introduced by an assignment.

use std::collections::{BTreeMap, HashMap};

use oxc_allocator::{Allocator, ArenaBox};

use super::ext::*;
use super::nodes::*;
use super::traverse::Visitor;
use crate::bytecode::{DebugInfo, VarKind};

// MARK: entry points

/// Gives every register reference its source level name, where one is known.
pub fn mark_locals<'a>(root: ArenaNode<'a>, alt_mode: bool) {
    root.traverse(&mut LocalsMarker::new(alt_mode));
}

/// Turns the assignments that introduce a variable into `local` definitions.
pub fn mark_local_definitions<'a>(alloc: &'a Allocator, root: ArenaNode<'a>) {
    root.traverse(&mut LocalDefinitionsMarker::new(alloc));
}

// MARK: naming registers

struct LocalsState<'a> {
    /// Register references that have not been resolved to a variable yet.
    pending_slots: BTreeMap<u32, Vec<ArenaNode<'a>>>,
    debug: Option<&'a DebugInfo<'a>>,
    /// Address of the node that is currently being visited, `-1` while unknown.
    addr: i64,
}

impl LocalsState<'_> {
    fn new() -> Self {
        LocalsState {
            pending_slots: BTreeMap::new(),
            debug: None,
            addr: -1,
        }
    }
}

struct LocalsMarker<'a> {
    states: Vec<LocalsState<'a>>,
    alt_mode: bool,
}

impl<'a> LocalsMarker<'a> {
    fn new(alt_mode: bool) -> Self {
        LocalsMarker {
            states: Vec::new(),
            alt_mode,
        }
    }

    fn state(&mut self) -> &mut LocalsState<'a> {
        self.states
            .last_mut()
            .expect("a function is always entered")
    }

    /// Names the pending register references using the variables that are alive
    /// at `addr`, and forgets the ones that have been dealt with.
    fn process_slots(&mut self, addr: i64) {
        let Ok(addr) = u32::try_from(addr) else {
            return;
        };
        let alt_mode = self.alt_mode;
        let state = self.state();
        let Some(debug) = state.debug else {
            return;
        };

        let mut resolved = Vec::new();
        let mut named = Vec::new();
        for (slot, nodes) in &state.pending_slots {
            let Some(variable) = debug.local_name(addr, *slot, alt_mode) else {
                continue;
            };
            resolved.push(*slot);
            if variable.kind == VarKind::Internal {
                continue;
            }
            named.push((variable.name, variable.end_addr, nodes.clone()));
        }

        for slot in resolved {
            state.pending_slots.remove(&slot);
        }

        for (name, end_addr, nodes) in named {
            for node in nodes {
                if let Node::Identifier(identifier) = &mut *node.borrow_mut() {
                    identifier.name = Some(name);
                    identifier.kind = IdentifierKind::Local;
                    identifier.local_end = Some(end_addr);
                }
            }
        }
    }

    fn reset_slot(&mut self, slot: u32) {
        self.state().pending_slots.remove(&slot);
    }

    fn reset_slots(&mut self, slots: &[ArenaNode<'a>]) {
        let slots: Vec<u32> = slots
            .iter()
            .filter_map(|node| node.identifier_kind_and_slot())
            .map(|(_, slot)| slot)
            .collect();
        for slot in slots {
            self.reset_slot(slot);
        }
    }

    /// Registers the address of a node, and resolves the pending references
    /// when it starts a new statement.
    ///
    /// Statements are visited on entry and on exit, so that both the address
    /// before and the one after it are taken into account.
    fn process_worthy_node(&mut self, node: ArenaNode<'a>) {
        let Some(addr) = node.borrow().addr() else {
            return;
        };
        if node.is_identifier() {
            return;
        }

        let addr = i64::from(addr);
        if self.state().addr < addr {
            self.state().addr = addr;
        }
        if !self.alt_mode {
            self.process_slots(addr);
        }
    }
}

impl<'a> Visitor<'a> for LocalsMarker<'a> {
    fn visit(&mut self, node: ArenaNode<'a>) -> bool {
        self.process_worthy_node(node);

        enum Action<'b> {
            None,
            Enter(&'b DebugInfo<'b>),
            Leave(Vec<ArenaNode<'b>>),
            ResetSlot(u32),
            Queue { slot: u32 },
        }

        let action = {
            let borrowed = node.borrow();
            match &*borrowed {
                Node::FunctionDefinition(inner) => Action::Enter(inner.debug),
                Node::Variables(_) | Node::Identifiers(_) => Action::Leave(node.list_contents()),
                Node::NumericLoopWarp(inner) => {
                    let index = inner.index;
                    match index.identifier_kind_and_slot() {
                        Some((_, slot)) => Action::ResetSlot(slot),
                        None => Action::None,
                    }
                }
                Node::Identifier(identifier) => {
                    if identifier.kind == IdentifierKind::Slot {
                        Action::Queue {
                            slot: identifier.slot,
                        }
                    } else {
                        Action::None
                    }
                }
                _ => Action::None,
            }
        };

        match action {
            Action::None => {}
            Action::Enter(debug) => {
                self.states.push(LocalsState::new());
                self.state().debug = Some(debug);
            }
            Action::Leave(contents) => {
                // Last chance for a `local a = a + 1` style assignment.
                let addr = self.state().addr;
                self.process_slots(addr);
                self.reset_slots(&contents);
            }
            Action::ResetSlot(slot) => self.reset_slot(slot),
            Action::Queue { slot } => {
                self.state()
                    .pending_slots
                    .entry(slot)
                    .or_default()
                    .push(node);
            }
        }

        true
    }

    fn leave(&mut self, node: ArenaNode<'a>) {
        enum Action {
            None,
            Exit(i64),
            Process(i64),
            Assignment(usize),
        }

        let action = {
            let borrowed = node.borrow();
            match &*borrowed {
                Node::FunctionDefinition(inner) => {
                    let mut addr = inner.instruction_count as i64;
                    if self.alt_mode {
                        addr -= 1;
                    }
                    Action::Exit(addr)
                }
                Node::NumericFor(_) | Node::IteratorFor(_) => {
                    if self.alt_mode {
                        match node.find_addr() {
                            Some(addr) => Action::Process(i64::from(addr)),
                            None => Action::None,
                        }
                    } else {
                        Action::None
                    }
                }
                Node::Assignment(inner) => {
                    if self.alt_mode {
                        let count = inner.destinations.list_contents().len();
                        Action::Assignment(count)
                    } else {
                        Action::None
                    }
                }
                _ => Action::None,
            }
        };

        match action {
            Action::None => {}
            Action::Exit(addr) => {
                self.process_slots(addr);
                self.states.pop();
            }
            Action::Process(addr) => self.process_slots(addr),
            Action::Assignment(count) => {
                for _ in 0..count {
                    let addr = self.state().addr;
                    self.process_slots(addr + 1);
                    self.process_slots(addr + 2);
                }
            }
        }

        self.process_worthy_node(node);
    }
}

// MARK: marking definitions

struct DefinitionsState {
    /// For every register, the variable it holds, if one is known.
    known_locals: HashMap<u32, Option<u32>>,
    addr: u32,
}

impl DefinitionsState {
    fn new() -> Self {
        DefinitionsState {
            known_locals: HashMap::new(),
            addr: 0,
        }
    }
}

struct LocalDefinitionsMarker<'a> {
    alloc: &'a Allocator,
    states: Vec<DefinitionsState>,
    path: Vec<ArenaNode<'a>>,
}

impl<'a> LocalDefinitionsMarker<'a> {
    fn new(alloc: &'a Allocator) -> Self {
        LocalDefinitionsMarker {
            alloc,
            states: Vec::new(),
            path: Vec::new(),
        }
    }

    fn state(&mut self) -> &mut DefinitionsState {
        self.states
            .last_mut()
            .expect("a function is always entered")
    }

    /// Records that `local` now lives in its register.
    ///
    /// Returns whether the same variable was already living there, which means
    /// the assignment writes to a local instead of declaring one.
    fn update_known_locals(&mut self, local: ArenaNode<'a>, addr: u32) -> bool {
        let (slot, end_addr) = {
            let borrowed = local.borrow();
            let Node::Identifier(identifier) = &*borrowed else {
                return false;
            };
            (identifier.slot, identifier.local_end)
        };

        let previous = self.state().known_locals.insert(slot, end_addr);

        match previous.flatten() {
            Some(end) => end > addr,
            None => false,
        }
    }

    /// Splits an assignment that declares a local and writes to an existing one
    /// at the same time, which Lua cannot express in a single statement.
    fn split_assignment(
        &mut self,
        statement: ArenaNode<'a>,
        destinations: &[ArenaNode<'a>],
        slot_index: usize,
    ) {
        let new_statement = {
            let borrowed = statement.borrow();
            let Node::Assignment(inner) = &*borrowed else {
                return;
            };
            let new_destinations = ArenaNode::new_variables(
                self.alloc,
                destinations[slot_index + 1..].iter().copied(),
            );
            ArenaNode::new(
                self.alloc,
                Node::Assignment(ArenaBox::new_in(
                    Assignment {
                        expressions: inner.expressions,
                        destinations: new_destinations,
                        kind: inner.kind,
                        meta: inner.meta,
                    },
                    &self.alloc,
                )),
            )
        };

        let old_destinations =
            ArenaNode::new_variables(self.alloc, destinations[..=slot_index].iter().copied());
        if let Node::Assignment(inner) = &mut *statement.borrow_mut() {
            inner.destinations = old_destinations;
        }

        // The statement has to stay where it is, which is inside the list the
        // path points at.
        for index in (1..self.path.len()).rev() {
            let holder = self.path[index];
            let contents = holder.list_contents();
            if let Some(position) = contents.iter().position(|item| *item == statement) {
                let mut contents = contents;
                contents.insert(position + 1, new_statement);
                holder.borrow_mut().list_set_contents(self.alloc, contents);
                return;
            }
        }
    }

    /// Handles an assignment statement: decides whether it declares a local and
    /// splits it when it mixes both cases.
    fn visit_assignment(
        &mut self,
        node: ArenaNode<'a>,
        destinations: Vec<ArenaNode<'a>>,
        addr: Option<u32>,
    ) {
        let Some(first) = destinations.first().copied() else {
            return;
        };

        // The address of the destination wins, if it has one: the debug
        // information may have moved on since the statement started.
        if let Some(dst_addr) = first.borrow().addr()
            && dst_addr != self.state().addr
        {
            self.state().addr = dst_addr;
        }
        let addr = addr.unwrap_or(self.state().addr);

        if first.identifier_kind() != Some(IdentifierKind::Local) {
            return;
        }

        let mut known_slot = self.update_known_locals(first, addr);
        let mut split_at = None;
        for (slot_index, slot) in destinations[1..].iter().enumerate() {
            let slot_is_local = slot.identifier_kind() == Some(IdentifierKind::Local);
            let also_known = slot_is_local && self.update_known_locals(*slot, addr);

            if !known_slot && (!slot_is_local || also_known) {
                // The variable is unknown, so it cannot be declared together
                // with the ones before it.
                split_at = Some(slot_index);
                break;
            }
            if !slot_is_local {
                return;
            }
            debug_assert_eq!(known_slot, also_known);
            known_slot = also_known;
        }

        if let Some(slot_index) = split_at {
            self.split_assignment(node, &destinations, slot_index);
            return;
        }

        if !known_slot && let Node::Assignment(inner) = &mut *node.borrow_mut() {
            inner.kind = AssignmentKind::LocalDefinition;
        }
    }

    /// Registers the variables a loop introduces.
    fn register_loop_variables(&mut self, variables: Vec<ArenaNode<'a>>, addr: Option<u32>) {
        let Some(addr) = addr else {
            return;
        };
        for variable in variables {
            if variable.identifier_kind() == Some(IdentifierKind::Local) {
                self.update_known_locals(variable, addr);
            }
        }
    }
}

impl<'a> Visitor<'a> for LocalDefinitionsMarker<'a> {
    fn visit(&mut self, node: ArenaNode<'a>) -> bool {
        enum Action<'b> {
            None,
            Enter(Vec<ArenaNode<'b>>),
            Assignment(Vec<ArenaNode<'b>>, Option<u32>),
            Loop(Vec<ArenaNode<'b>>, Option<u32>),
        }

        if let Some(addr) = node.borrow().addr() {
            self.state().addr = addr;
        }
        self.path.push(node);

        let action = {
            let borrowed = node.borrow();
            match &*borrowed {
                Node::FunctionDefinition(inner) => Action::Enter(inner.arguments.list_contents()),
                Node::IteratorFor(inner) => {
                    Action::Loop(inner.identifiers.list_contents(), borrowed.addr())
                }
                Node::NumericFor(inner) => Action::Loop(vec![inner.variable], borrowed.addr()),
                Node::Assignment(inner) => {
                    Action::Assignment(inner.destinations.list_contents(), borrowed.addr())
                }
                _ => Action::None,
            }
        };

        match action {
            Action::None => {}
            Action::Enter(arguments) => {
                self.states.push(DefinitionsState::new());
                for argument in arguments {
                    // Arguments are live from the first instruction on.
                    if argument.is_identifier() {
                        self.update_known_locals(argument, 1);
                    }
                }
            }
            Action::Loop(variables, addr) => self.register_loop_variables(variables, addr),
            Action::Assignment(destinations, addr) => {
                self.visit_assignment(node, destinations, addr)
            }
        }

        true
    }

    fn leave(&mut self, node: ArenaNode<'a>) {
        self.path.pop();

        if node.is_function_definition() {
            self.states.pop();
        }
    }
}
