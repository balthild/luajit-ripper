//! Removing the temporary registers that the compiler introduced.
//!
//! This is a port of ljd's `ast/slotworks.py`.
//!
//! LuaJIT keeps every value in a register, and most registers only ever hold an
//! intermediate value: `local c = a + b` is compiled as "compute into a scratch
//! register, then move it into the register of `c`". Undoing those moves is
//! what makes the output readable, and it is safe as long as a register is
//! written exactly once and read a handful of times.
//!
//! The pass is therefore built around a per-register record: it collects, for
//! each register, the assignment that defines it and every reference to it. What
//! happens next depends on the shape of those references:
//!
//! * a single reference in an ordinary expression is inlined into it,
//! * a reference that assigns into a table constructor becomes a record of that
//!   constructor,
//! * the results of a call or a vararg that are spread over several registers
//!   are folded back into a multiple assignment,
//! * the registers a generic `for` loop uses become the loop's variables,
//! * everything else is left alone, because inlining it could change the
//!   meaning of the program.
//!
//! The AST is a shared graph, so a reference is recorded as a *path* from the
//! root down to it. That path is what the pass uses to find the parent it has
//! to rewrite.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, ArenaHashSet, ArenaVec};

use super::helpers::insert_table_record;
use super::nodes::*;
use super::traverse::{self, Visitor};
use crate::error::{Error, Result};

/// What [`eliminate_temporary`] is allowed to do.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Skip references that could belong to more than one register.
    ///
    /// Ambiguity happens when the same register is written twice and the
    /// second write is only reached through a branch; a reference after the
    /// branch could have been written by either. Set to `false` to inline
    /// through it anyway, which the unwarper does inside blocks it has already
    /// proven safe.
    pub ignore_ambiguous: bool,
    /// Record the possible registers on every reference, so that a later run
    /// can tell which of them still apply.
    pub identify_slots: bool,
    /// Insert only where the value is known to still be valid.
    pub safe_mode: bool,
    /// The tree has already been unwarped, so it has no control flow graph.
    pub unwarped: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            ignore_ambiguous: true,
            identify_slots: false,
            safe_mode: true,
            unwarped: false,
        }
    }
}

/// Inlines every register that can be inlined without changing the program.
pub fn eliminate_temporary<'a>(
    alloc: &'a Allocator,
    root: NodeRef<'a>,
    options: Options,
) -> Result<()> {
    let invalidated = Invalidated::new_in(alloc);

    eliminate_multres(alloc, root, &invalidated)?;

    let mut slots = collect_slots(alloc, root, options.identify_slots, options.unwarped);
    sort_slots(&mut slots);
    eliminate_collected(alloc, root, slots, options, &invalidated)?;

    if !options.unwarped {
        cleanup_invalid_nodes(alloc, root, &invalidated);
    }

    Ok(())
}

/// Runs the simplifier over the tree, calling `callback` for the parts it changed.
pub fn simplify_ast<'a>(
    alloc: &'a Allocator,
    root: NodeRef<'a>,
    callback: &mut dyn FnMut(NodeRef<'a>),
) {
    traverse::traverse(&mut SimplifyVisitor::new(alloc, callback), root);
}

fn internal(message: &str) -> Error {
    Error::Internal(message.to_string())
}

// -- statements the pass deleted -------------------------------------------

/// Statements a pass has decided to remove.
///
/// ljd marks a node with an `_invalidated` attribute; since the flag only means
/// something for the run that set it, it is kept here instead of on the nodes.
#[derive(Clone, Copy)]
struct Invalidated<'a>(&'a RefCell<ArenaHashSet<'a, usize>>);

fn node_key<'a>(node: NodeRef<'a>) -> usize {
    traverse::node_key(node)
}

/// Whether two records describe the same thing.
///
/// The arena keeps its allocations where they are, so the address of a record
/// is its identity.
fn same_info<'a>(a: &Info<'a>, b: &Info<'a>) -> bool {
    std::ptr::eq(*a, *b)
}

impl<'a> Invalidated<'a> {
    fn new_in(alloc: &'a Allocator) -> Self {
        Invalidated(alloc.alloc(RefCell::new(ArenaHashSet::new_in(alloc))))
    }

    fn mark(&self, node: NodeRef<'a>) {
        self.0.borrow_mut().insert(node_key(node));
    }

    fn is_marked(&self, node: NodeRef<'a>) -> bool {
        self.0.borrow().contains(&node_key(node))
    }
}

/// Drops the statements the pass marked as deleted.
fn cleanup_invalid_nodes<'a>(
    alloc: &'a Allocator,
    root: NodeRef<'a>,
    invalidated: &Invalidated<'a>,
) {
    traverse::traverse(&mut TreeCleanup { alloc, invalidated }, root);
}

struct TreeCleanup<'a, 'b> {
    alloc: &'a Allocator,
    invalidated: &'b Invalidated<'a>,
}

impl<'a> Visitor<'a> for TreeCleanup<'a, '_> {
    fn visit(&mut self, node: NodeRef<'a>) -> bool {
        if !matches!(&*node.borrow(), Node::Block(_)) {
            return true;
        }

        let contents = traverse::block_contents(node);
        let kept = contents
            .into_iter()
            .filter(|statement| !self.invalidated.is_marked(statement));
        traverse::set_block_contents(self.alloc, node, kept);

        true
    }
}

// -- node helpers ----------------------------------------------------------

/// The register of an identifier that still stands for a register.
fn is_slot_identifier<'a>(node: NodeRef<'a>) -> Option<u32> {
    match &*node.borrow() {
        Node::Identifier(identifier) if identifier.kind == IdentifierKind::Slot => {
            Some(identifier.slot)
        }
        _ => None,
    }
}

fn identifier_slot<'a>(node: NodeRef<'a>) -> Option<u32> {
    match &*node.borrow() {
        Node::Identifier(identifier) => Some(identifier.slot),
        _ => None,
    }
}

fn identifier_id<'a>(node: NodeRef<'a>) -> Option<u32> {
    match &*node.borrow() {
        Node::Identifier(identifier) => identifier.id,
        _ => None,
    }
}

fn identifier_possible_ids<'a>(node: NodeRef<'a>) -> Vec<u32> {
    match &*node.borrow() {
        Node::Identifier(identifier) => identifier.possible_ids.iter().copied().collect(),
        _ => Vec::new(),
    }
}

fn identifier_name<'a>(node: NodeRef<'a>) -> Option<&'a str> {
    match &*node.borrow() {
        Node::Identifier(identifier) => identifier.name,
        _ => None,
    }
}

fn identifier_kind<'a>(node: NodeRef<'a>) -> Option<IdentifierKind> {
    match &*node.borrow() {
        Node::Identifier(identifier) => Some(identifier.kind),
        _ => None,
    }
}

fn set_identifier_id<'a>(node: NodeRef<'a>, id: u32) {
    if let Node::Identifier(identifier) = &mut *node.borrow_mut() {
        identifier.id = Some(id);
    }
}

fn set_identifier_possible_ids<'a>(alloc: &'a Allocator, node: NodeRef<'a>, ids: Vec<u32>) {
    if let Node::Identifier(identifier) = &mut *node.borrow_mut() {
        identifier.possible_ids = ArenaVec::from_iter_in(ids, &alloc);
    }
}

fn forget_possible_id<'a>(node: NodeRef<'a>, id: u32) {
    if let Node::Identifier(identifier) = &mut *node.borrow_mut() {
        identifier.possible_ids.retain(|candidate| *candidate != id);
    }
}

fn is_list<'a>(node: NodeRef<'a>) -> bool {
    matches!(
        &*node.borrow(),
        Node::Variables(_) | Node::Identifiers(_) | Node::Expressions(_) | Node::Statements(_)
    )
}

/// The nearest ancestor of the last node of `path` that is not a list.
///
/// An identifier always sits inside some list, so the list itself is never the
/// thing the pass has to rewrite.
fn get_holder<'a>(path: &[NodeRef<'a>]) -> Option<NodeRef<'a>> {
    path.iter()
        .rev()
        .skip(1)
        .find(|node| !is_list(node))
        .copied()
}

/// The value an assignment computes.
fn assignment_source<'a>(assignment: NodeRef<'a>) -> Result<NodeRef<'a>> {
    let borrowed = assignment.borrow();
    let Node::Assignment(inner) = &*borrowed else {
        return Err(internal(
            "a register was defined by something other than an assignment",
        ));
    };
    traverse::list_contents(inner.expressions)
        .into_iter()
        .next()
        .ok_or_else(|| internal("an assignment without a value"))
}

fn replace_in_list<'a>(
    alloc: &'a Allocator,
    list: NodeRef<'a>,
    original: NodeRef<'a>,
    replacement: NodeRef<'a>,
) -> bool {
    let contents = traverse::list_contents(list);
    match traverse::position(&contents, original) {
        Some(index) => {
            let mut contents = contents;
            contents[index] = replacement;
            set_list_contents(alloc, list, contents);
            true
        }
        None => false,
    }
}

/// Replaces `original` with `replacement` where the reference points.
fn replace_node<'a>(
    alloc: &'a Allocator,
    holder: NodeRef<'a>,
    original: NodeRef<'a>,
    replacement: NodeRef<'a>,
) -> bool {
    if is_list(holder) {
        return replace_in_list(alloc, holder, original, replacement);
    }
    traverse::replace_child(holder, original, replacement)
}

// -- slot bookkeeping ------------------------------------------------------

/// One place a register is read.
struct SlotReference<'a> {
    /// The nodes from the root down to the reference, the reference included.
    path: ArenaVec<'a, NodeRef<'a>>,
    identifier: NodeRef<'a>,
}

impl<'a> SlotReference<'a> {
    /// Copies the reference into the arena it belongs to.
    ///
    /// The arena types have no `Clone`, so a reference has to be rebuilt
    /// explicitly.
    fn clone_in(&self, alloc: &'a Allocator) -> SlotReference<'a> {
        SlotReference {
            path: ArenaVec::from_iter_in(self.path.iter().copied(), &alloc),
            identifier: self.identifier,
        }
    }
}

/// A register and everything that refers to it.
struct SlotInfo<'a> {
    slot: u32,
    /// The assignment that writes the register.
    assignment: NodeRef<'a>,
    references: ArenaVec<'a, SlotReference<'a>>,
    /// The node that ended this register's life. ljd records it for debugging;
    /// nothing in the pass reads it back.
    termination: Option<NodeRef<'a>>,
    /// Position of the write in the input, used to order the slots.
    slot_id: u32,
}

/// A record of one register definition.
///
/// Like a node, a record lives in the arena, so a reference to it is cheap to
/// copy and its address is its identity.
type Info<'a> = &'a RefCell<SlotInfo<'a>>;

/// Allocates a fresh register record in the arena the tree lives in.
fn new_info<'a>(
    alloc: &'a Allocator,
    slot: u32,
    assignment: NodeRef<'a>,
    slot_id: u32,
) -> Info<'a> {
    alloc.alloc(RefCell::new(SlotInfo {
        slot,
        assignment,
        references: ArenaVec::new_in(&alloc),
        termination: None,
        slot_id,
    }))
}

/// For each register, the live definitions by id. The entry under `-1` is the
/// most recent one, which is what a plain reference to the register means.
type KnownSlots<'a> = HashMap<u32, HashMap<i64, Info<'a>>>;

// -- collecting ------------------------------------------------------------

struct CollectorState<'a> {
    known_slots: KnownSlots<'a>,
    all_known_slots: KnownSlots<'a>,
    /// The state every finished block was left in, by block identity.
    block_slots: HashMap<usize, KnownSlots<'a>>,
    /// Which blocks the warp of a block can reach, by target identity.
    block_refs: HashMap<usize, Vec<usize>>,
    block: Option<NodeRef<'a>>,
}

impl<'a> CollectorState<'a> {
    fn new() -> Self {
        CollectorState {
            known_slots: KnownSlots::new(),
            all_known_slots: KnownSlots::new(),
            block_slots: HashMap::new(),
            block_refs: HashMap::new(),
            block: None,
        }
    }

    /// The definitions of every register that are live right now, with the
    /// duplicate entries under `-1` left out.
    fn definitions(&self) -> Vec<Info<'a>> {
        let mut infos: Vec<Info<'a>> = Vec::new();
        for slot_states in self.known_slots.values() {
            for (id, info) in slot_states {
                if i64::from(info.borrow().slot_id) == *id
                    && !infos.iter().any(|existing| same_info(existing, info))
                {
                    infos.push(info);
                }
            }
        }
        infos
    }
}

/// Walks the tree and records, for every register, where it is written and read.
struct SlotsCollector<'a> {
    alloc: &'a Allocator,
    states: Vec<CollectorState<'a>>,
    path: Vec<NodeRef<'a>>,
    /// The expression list of the assignment being visited, skipped because it
    /// was already walked before the destinations were registered.
    skip: Option<NodeRef<'a>>,
    next_slot_id: u32,
    identify: bool,
    unwarped: bool,
    slots: Vec<Info<'a>>,
    unused: Vec<Info<'a>>,
}

impl<'a> SlotsCollector<'a> {
    fn run(
        alloc: &'a Allocator,
        root: NodeRef<'a>,
        identify: bool,
        unwarped: bool,
    ) -> (Vec<Info<'a>>, Vec<Info<'a>>) {
        let mut collector = SlotsCollector {
            alloc,
            states: vec![CollectorState::new()],
            path: Vec::new(),
            skip: None,
            next_slot_id: 0,
            identify,
            unwarped,
            slots: Vec::new(),
            unused: Vec::new(),
        };
        traverse::traverse(&mut collector, root);

        (collector.slots, collector.unused)
    }

    fn state(&mut self) -> &mut CollectorState<'a> {
        self.states
            .last_mut()
            .expect("a function is always entered")
    }

    fn read_state(&self) -> &CollectorState<'a> {
        self.states.last().expect("a function is always entered")
    }

    /// The definition of `slot` the current block knows about.
    fn get_slot(&self, slot: u32, id: Option<u32>, exact: bool) -> Option<Info<'a>> {
        let slot_states = self.read_state().known_slots.get(&slot)?;
        let info = slot_states.get(&id.map(i64::from).unwrap_or(-1))?;

        if exact && info.borrow().slot_id != id.unwrap_or(u32::MAX) {
            return None;
        }
        Some(info)
    }

    fn set_slot(&mut self, slot: u32, id: Option<u32>, info: Info<'a>) {
        let state = self.state();
        for target in [&mut state.known_slots, &mut state.all_known_slots] {
            let slot_states = target.entry(slot).or_default();
            slot_states.insert(id.map(i64::from).unwrap_or(-1), info);
            // `-1` stands for the most recent definition, so it is only
            // replaced when a real id is registered.
            if id.is_some() {
                slot_states.insert(-1, info);
            }
        }
    }

    fn remove_slot(&mut self, slot: u32, id: Option<u32>) {
        let state = self.state();
        for target in [&mut state.known_slots, &mut state.all_known_slots] {
            let Some(slot_states) = target.get_mut(&slot) else {
                continue;
            };

            match id {
                Some(id) => {
                    if let (Some(current), Some(most_recent)) =
                        (slot_states.get(&i64::from(id)), slot_states.get(&-1))
                        && same_info(current, most_recent)
                    {
                        slot_states.remove(&-1);
                    }
                    slot_states.remove(&i64::from(id));
                }
                None => {
                    slot_states.remove(&-1);
                }
            }

            if slot_states.is_empty() {
                target.remove(&slot);
            }
        }
    }

    /// Looks for a definition of `slot` the given block knows about, following
    /// the graph backwards when it does not.
    fn find_slot_assignments(
        &self,
        slot: u32,
        block: Option<usize>,
        visited: &mut HashSet<usize>,
    ) -> Option<Vec<Info<'a>>> {
        let state = self.read_state();
        let current = state.block.map(node_key);
        let block = block.or(current)?;

        // While a block is being walked its state lives in the collector; once
        // it is finished, the recorded copy is used instead.
        let known = if Some(block) == current {
            Some(&state.known_slots)
        } else {
            state.block_slots.get(&block)
        };

        if let Some(info) = known
            .and_then(|known| known.get(&slot))
            .and_then(|slots| slots.get(&-1))
        {
            return Some(vec![*info]);
        }

        // Keep track of the blocks visited so that a loop cannot turn this into
        // an endless walk.
        let predecessors = state.block_refs.get(&block)?;
        visited.insert(block);

        let mut found: Vec<Info<'a>> = Vec::new();
        for predecessor in predecessors {
            if visited.contains(predecessor) {
                continue;
            }
            if let Some(infos) = self.find_slot_assignments(slot, Some(*predecessor), visited) {
                for info in infos {
                    if !found.iter().any(|existing| same_info(existing, &info)) {
                        found.push(info);
                    }
                }
            }
        }

        (!found.is_empty()).then_some(found)
    }

    fn commit_info(&mut self, info: Info<'a>) {
        match info.borrow().references.len() {
            0 => {}
            1 => self.unused.push(info),
            _ => self.slots.push(info),
        }
    }

    /// Ends the life of the definition `id` refers to.
    fn commit_slot(&mut self, slot: u32, id: Option<u32>, node: NodeRef<'a>) {
        let Some(info) = self.get_slot(slot, id, true) else {
            return;
        };

        info.borrow_mut().termination = Some(node);
        self.remove_slot(slot, id);
        self.commit_info(info);
    }

    /// Starts a new definition of `slot`.
    fn register_slot(&mut self, node: NodeRef<'a>, slot: NodeRef<'a>, assignment: NodeRef<'a>) {
        let Some(slot_number) = identifier_slot(slot) else {
            return;
        };
        let id = identifier_id(slot);
        self.commit_slot(slot_number, id, node);

        // A register can be registered again by a later run over the same tree;
        // reusing the id keeps references from the previous run valid.
        let slot_id = match id {
            Some(id) => id,
            None => {
                let id = self.next_slot_id;
                self.next_slot_id += 1;
                set_identifier_id(slot, id);
                id
            }
        };

        let info = new_info(self.alloc, slot_number, assignment, slot_id);
        self.set_slot(slot_number, Some(slot_id), info);
    }

    fn register_all_slots(&mut self, node: NodeRef<'a>, slots: &[NodeRef<'a>]) {
        for slot in slots {
            if is_slot_identifier(slot).is_some() {
                self.register_slot(node, slot, node);
            }
        }
    }

    /// Records that `node` reads the register `info` describes.
    fn register_slot_reference(&mut self, info: Info<'a>, node: NodeRef<'a>, update_id: bool) {
        let slot_id = info.borrow().slot_id;
        let id = identifier_id(node);
        let possible_ids = identifier_possible_ids(node);

        if id.is_none() && !possible_ids.contains(&slot_id) {
            if update_id {
                if !possible_ids.is_empty() {
                    // The register matches, but not the definition: this
                    // reference belongs to a different one.
                    return;
                }
                set_identifier_id(node, slot_id);
            } else if self.identify {
                let mut possible_ids = possible_ids;
                possible_ids.push(slot_id);
                possible_ids.sort_unstable();
                set_identifier_possible_ids(self.alloc, node, possible_ids);
            }
        }

        let path = ArenaVec::from_iter_in(self.path.iter().copied(), &self.alloc);
        info.borrow_mut().references.push(SlotReference {
            path,
            identifier: node,
        });
    }

    fn visit_assignment(
        &mut self,
        node: NodeRef<'a>,
        expressions: NodeRef<'a>,
        destinations: Vec<NodeRef<'a>>,
    ) {
        // The expressions have to be collected first: registering the
        // destinations makes them definitions, and a definition must not be
        // mistaken for a read of the previous value.
        traverse::visit_subtree(self, expressions);
        self.skip = Some(expressions);
        self.register_all_slots(node, &destinations);
    }

    fn visit_identifier(&mut self, node: NodeRef<'a>) {
        let Some(slot) = is_slot_identifier(node) else {
            return;
        };

        // A register may have been identified in a previous block; the most
        // recent definition is the one meant here.
        if let Some(info) = self.get_slot(slot, identifier_id(node), false) {
            self.register_slot_reference(info, node, true);
            return;
        }

        let mut visited = HashSet::new();
        let Some(assignments) = self.find_slot_assignments(slot, None, &mut visited) else {
            return;
        };
        // With a single candidate the reference is unambiguous, so it can be
        // pinned to it.
        let update_ids = self.identify && assignments.len() == 1;
        for info in assignments {
            self.register_slot_reference(info, node, update_ids);
        }
    }

    fn leave_block(&mut self, node: NodeRef<'a>) {
        let targets = match traverse::block_warp(node) {
            Some(warp) => traverse::block_targets(&warp.borrow()),
            None => Vec::new(),
        };

        let state = self.state();
        for target in targets {
            let refs = state.block_refs.entry(node_key(target)).or_default();
            let key = node_key(node);
            if !refs.contains(&key) {
                refs.push(key);
            }
        }

        // The state a block ends in is what a later block sees when it looks
        // backwards through the graph.
        let infos = state.definitions();
        state
            .block_slots
            .insert(node_key(node), state.known_slots.clone());
        state.known_slots = KnownSlots::new();

        for info in infos {
            self.commit_info(info);
        }
    }
}

impl<'a> Visitor<'a> for SlotsCollector<'a> {
    fn visit(&mut self, node: NodeRef<'a>) -> bool {
        if let Some(skip) = self.skip
            && traverse::same_node(skip, node)
        {
            return false;
        }

        self.path.push(node);

        enum Action<'b> {
            None,
            Enter,
            Assignment(NodeRef<'b>, Vec<NodeRef<'b>>),
            Identifier,
            Block,
        }

        let action = {
            let borrowed = node.borrow();
            match &*borrowed {
                Node::FunctionDefinition(_) => Action::Enter,
                Node::Assignment(inner) => Action::Assignment(
                    inner.expressions,
                    traverse::list_contents(inner.destinations),
                ),
                Node::Identifier(_) => Action::Identifier,
                Node::Block(_) => Action::Block,
                _ => Action::None,
            }
        };

        match action {
            Action::None => {}
            Action::Enter => self.states.push(CollectorState::new()),
            Action::Assignment(expressions, destinations) => {
                self.visit_assignment(node, expressions, destinations)
            }
            Action::Identifier => self.visit_identifier(node),
            Action::Block => self.state().block = Some(node),
        }

        true
    }

    fn leave(&mut self, node: NodeRef<'a>) {
        self.path.pop();

        enum Action {
            None,
            Assignment,
            Block,
            Function,
        }

        let action = {
            let borrowed = node.borrow();
            match &*borrowed {
                Node::Assignment(_) => Action::Assignment,
                Node::Block(_) => Action::Block,
                Node::FunctionDefinition(_) => Action::Function,
                _ => Action::None,
            }
        };

        match action {
            Action::None => {}
            Action::Assignment => self.skip = None,
            Action::Block => self.leave_block(node),
            Action::Function => {
                // An unwarped tree has no blocks to end a register's life, so
                // anything still live is committed when its function ends.
                if self.unwarped && self.states.len() == 1 {
                    let infos = self.state().definitions();
                    for info in infos {
                        self.commit_info(info);
                    }
                }
                self.states.pop();
            }
        }
    }
}

fn collect_slots<'a>(
    alloc: &'a Allocator,
    root: NodeRef<'a>,
    identify: bool,
    unwarped: bool,
) -> Vec<Info<'a>> {
    SlotsCollector::run(alloc, root, identify, unwarped).0
}

/// Orders the slots by where they were written, so that a value is inlined into
/// the statement that uses it rather than into one that comes earlier.
fn sort_slots<'a>(slots: &mut [Info<'a>]) {
    slots.sort_by_key(|info| info.borrow().slot_id);
}

// -- eliminating -----------------------------------------------------------

/// One reference that can be replaced by the value it reads.
struct SimpleReference<'a> {
    info: Info<'a>,
    reference: SlotReference<'a>,
}

struct RefsProcessData<'a> {
    slots: Vec<Info<'a>>,
    simple: Vec<SimpleReference<'a>>,
    massive: Vec<(NodeRef<'a>, Info<'a>, NodeRef<'a>, NodeRef<'a>)>,
    tables: Vec<(Info<'a>, SlotReference<'a>)>,
    iterators: Vec<(Info<'a>, NodeRef<'a>, NodeRef<'a>)>,
    unsafe_slots: Vec<Info<'a>>,
}

impl<'a> RefsProcessData<'a> {
    fn new(slots: Vec<Info<'a>>) -> Self {
        RefsProcessData {
            slots,
            simple: Vec::new(),
            massive: Vec::new(),
            tables: Vec::new(),
            iterators: Vec::new(),
            unsafe_slots: Vec::new(),
        }
    }
}

fn eliminate_collected<'a>(
    alloc: &'a Allocator,
    root: NodeRef<'a>,
    slots: Vec<Info<'a>>,
    options: Options,
    invalidated: &Invalidated<'a>,
) -> Result<()> {
    let mut data = RefsProcessData::new(slots);

    fill_refs(
        alloc,
        &mut data,
        options.ignore_ambiguous && options.safe_mode,
        options.safe_mode,
    )?;

    eliminate_simple_cases(alloc, &data.simple, invalidated)?;
    recheck_unsafe_cases(alloc, root, &data.unsafe_slots, options, invalidated)?;
    eliminate_into_table_constructors(alloc, &data.tables, invalidated)?;
    eliminate_mass_assignments(alloc, &data.massive, invalidated);
    eliminate_iterators(alloc, &data.iterators, invalidated)?;

    Ok(())
}

/// Decides what to do with every collected register.
fn fill_refs<'a>(
    alloc: &'a Allocator,
    data: &mut RefsProcessData<'a>,
    ignore_ambiguous: bool,
    safe_mode: bool,
) -> Result<()> {
    for info in data.slots.clone() {
        let assignment = info.borrow().assignment;
        let destination_count = match &*assignment.borrow() {
            Node::Assignment(inner) => traverse::list_contents(inner.destinations).len(),
            _ => {
                return Err(internal(
                    "a register was defined by something other than an assignment",
                ));
            }
        };

        if destination_count > 1 {
            fill_massive_refs(alloc, info, data, ignore_ambiguous, safe_mode)?;
        } else {
            fill_simple_refs(alloc, info, data, ignore_ambiguous, safe_mode)?;
        }
    }

    Ok(())
}

fn fill_simple_refs<'a>(
    alloc: &'a Allocator,
    info: Info<'a>,
    data: &mut RefsProcessData<'a>,
    ignore_ambiguous: bool,
    safe_mode: bool,
) -> Result<()> {
    let assignment = info.borrow().assignment;
    let source = assignment_source(assignment)?;
    let source_is_table = matches!(&*source.borrow(), Node::TableConstructor(_));

    let mut holders: Vec<NodeRef<'a>> = Vec::new();
    let mut new_simple: Vec<SimpleReference<'a>> = Vec::new();

    // Once a reference that cannot be part of the constructor has been seen,
    // the ones after it cannot be folded into the constructor either.
    let mut all_ctor_refs = true;

    let references = info.borrow();

    for reference in references.references.iter().skip(1) {
        // A reference that could not be pinned to a single definition is
        // skipped: inlining it could pick the wrong value.
        if ignore_ambiguous && identifier_id(reference.identifier).is_none() {
            continue;
        }

        let Some(holder) = get_holder(&reference.path) else {
            continue;
        };

        let is_element = matches!(&*holder.borrow(), Node::TableElement(_));
        if is_element {
            // The compiler can evaluate part of an index expression once and
            // move it to another register, which gives two references with the
            // same holder; only the first one counts.
            if traverse::contains(&holders, holder) {
                continue;
            }
            holders.push(holder);
        }

        let Some(path_index) = traverse::position(&reference.path, holder) else {
            continue;
        };

        // The statement the reference belongs to, if it reached the holder
        // through an assignment destination.
        let statement = get_holder(&reference.path[..path_index]);
        let is_destination = statement.is_some_and(|statement| {
            matches!(&*statement.borrow(), Node::Assignment(inner)
                if traverse::list_contents(inner.destinations)
                    .first()
                    .is_some_and(|destination| traverse::same_node(destination, holder)))
        });

        if source_is_table && is_element && is_destination && all_ctor_refs {
            data.tables.push((info, reference.clone_in(alloc)));
        } else {
            new_simple.push(SimpleReference {
                info,
                reference: reference.clone_in(alloc),
            });
            all_ctor_refs = false;
        }
    }

    // Inlining is only safe while there is a single use: with two, the value
    // would be computed twice, and computing it may have side effects.
    let count = new_simple.len();
    if count == 1 || (!safe_mode && count == 2) {
        data.simple.extend(new_simple);
    } else if count > 1 {
        data.unsafe_slots.push(info);
    }

    Ok(())
}

fn fill_massive_refs<'a>(
    alloc: &'a Allocator,
    info: Info<'a>,
    data: &mut RefsProcessData<'a>,
    ignore_ambiguous: bool,
    safe_mode: bool,
) -> Result<()> {
    let assignment = info.borrow().assignment;
    let source = assignment_source(assignment)?;

    // `local a = nil; local b = nil` compiles into a single `KNIL`, which has
    // nothing to do with multiple results: every value is nil on its own, so
    // the ordinary inlining rules apply.
    if matches!(&*source.borrow(), Node::Primitive(primitive)
        if primitive.kind == PrimitiveKind::Nil)
    {
        return fill_simple_refs(alloc, info, data, ignore_ambiguous, safe_mode);
    }

    if !matches!(&*source.borrow(), Node::FunctionCall(_) | Node::Vararg) {
        return Err(internal("a multiple assignment did not come from a call"));
    }

    let Some(first_reference) = info
        .borrow()
        .references
        .get(1)
        .map(|entry| entry.clone_in(alloc))
    else {
        return Ok(());
    };
    let Some(holder) = get_holder(&first_reference.path) else {
        return Ok(());
    };

    if safe_mode {
        // A reference that could have been written by an earlier definition
        // cannot be folded into this assignment.
        loop {
            let slot_id = info.borrow().slot_id;
            if info.borrow().references.len() <= 2 {
                break;
            }
            let Some(last) = info
                .borrow()
                .references
                .last()
                .map(|entry| entry.clone_in(alloc))
            else {
                break;
            };
            if identifier_id(last.identifier).is_some() {
                break;
            }
            forget_possible_id(last.identifier, slot_id);
            info.borrow_mut().references.pop();
        }
    }

    // More than two references means the values cannot be moved into the call
    // sites; leave the registers as ordinary locals.
    if info.borrow().references.len() != 2 {
        data.unsafe_slots.push(info);
        return Ok(());
    }

    match &*holder.borrow() {
        Node::Assignment(inner) => {
            let Some(destination) = traverse::list_contents(inner.destinations).first().copied()
            else {
                return Ok(());
            };
            let origin = info.borrow().references[0].identifier;

            // The statement that copies the results out of the call.
            let base = first_reference
                .path
                .len()
                .checked_sub(3)
                .and_then(|index| first_reference.path.get(index))
                .copied();
            let Some(base) = base else {
                return Ok(());
            };
            if !matches!(&*base.borrow(), Node::Assignment(_)) {
                return Ok(());
            }

            data.massive.push((origin, info, base, destination));
        }
        Node::IteratorWarp(_) => {
            data.iterators.push((info, source, holder));
        }
        _ => {}
    }

    Ok(())
}

/// Inlines the value a register holds into the place that reads it.
fn eliminate_simple_cases<'a>(
    alloc: &'a Allocator,
    simple: &[SimpleReference<'a>],
    invalidated: &Invalidated<'a>,
) -> Result<()> {
    for entry in simple {
        let info = entry.info;
        let reference = &entry.reference;

        let Some(holder) = reference
            .path
            .len()
            .checked_sub(2)
            .and_then(|index| reference.path.get(index))
            .copied()
        else {
            return Err(internal("a register reference has no parent"));
        };
        let destination = reference.identifier;

        let assignment = info.borrow().assignment;
        let source = assignment_source(assignment)?;

        // Inlining a constant into a place that cannot hold one produces code
        // Lua will not parse, as in `local a = nil; a()`.
        if matches!(&*source.borrow(), Node::Primitive(_) | Node::Constant(_)) {
            let is_string = matches!(&*source.borrow(), Node::Constant(inner)
                if matches!(inner.value, ConstantValue::String(_)));
            if matches!(&*holder.borrow(), Node::FunctionCall(call)
                if traverse::same_node(call.function, destination))
            {
                continue;
            }
            if matches!(&*holder.borrow(), Node::TableElement(element)
                if traverse::same_node(element.table, destination) && !is_string)
            {
                continue;
            }
        }

        if matches!(&*source.borrow(), Node::FunctionDefinition(_))
            && info.borrow().references.len() >= 3
        {
            // A function that is called more than once has to stay a variable.
            let count = info
                .borrow()
                .references
                .iter()
                .filter(|reference| {
                    identifier_id(reference.identifier).is_some()
                        || identifier_possible_ids(reference.identifier).len() == 1
                })
                .count();
            if count >= 3 {
                let first = info.borrow().references[0].identifier;
                let slot = match &*first.borrow() {
                    Node::Identifier(identifier) => identifier.slot,
                    _ => continue,
                };
                let name = alloc.alloc_str(&format!("slot{slot}"));
                if let Node::Identifier(identifier) = &mut *first.borrow_mut() {
                    identifier.kind = IdentifierKind::Local;
                    if identifier.name.is_none() {
                        identifier.name = Some(name);
                    }
                }
                continue;
            }
        } else if matches!(
            &*source.borrow(),
            Node::BinaryOperator(_) | Node::UnaryOperator(_)
        ) && is_index_key(entry)
        {
            // An operator that is inlined into an index can turn the call it
            // was part of into a method call; undo that if the key is not a
            // plain name.
            let function = reference
                .path
                .len()
                .checked_sub(3)
                .and_then(|index| reference.path.get(index))
                .copied();
            if let Some(function) = function
                && let Node::FunctionCall(call) = &mut *function.borrow_mut()
                && call.is_method
            {
                let key_is_name = matches!(&*call.function.borrow(), Node::TableElement(element)
                    if matches!(&*element.key.borrow(), Node::Constant(key)
                        if matches!(key.value, ConstantValue::String(_))));
                let table = match &*holder.borrow() {
                    Node::TableElement(element) => Some(element.table),
                    _ => None,
                };
                if !key_is_name && let Some(table) = table {
                    let mut arguments = traverse::list_contents(call.arguments);
                    arguments.insert(0, table);
                    set_list_contents(alloc, call.arguments, arguments);
                    call.is_method = false;
                }
            }
        }

        invalidated.mark(assignment);

        if !replace_node(alloc, holder, destination, source) {
            return Err(internal("a register reference could not be replaced"));
        }
    }

    Ok(())
}

/// Whether the reference is the key of a table element.
fn is_index_key<'a>(entry: &SimpleReference<'a>) -> bool {
    let Some(holder) = entry
        .reference
        .path
        .len()
        .checked_sub(2)
        .and_then(|index| entry.reference.path.get(index))
    else {
        return false;
    };

    matches!(&*holder.borrow(), Node::TableElement(element)
        if traverse::same_node(element.key, entry.reference.identifier))
}

/// Folds a register that only assigns into a table constructor into it.
fn eliminate_into_table_constructors<'a>(
    alloc: &'a Allocator,
    tables: &[(Info<'a>, SlotReference<'a>)],
    invalidated: &Invalidated<'a>,
) -> Result<()> {
    for (info, reference) in tables {
        let assignment = info.borrow().assignment;
        let constructor = assignment_source(assignment)?;

        // The reference sits inside the assignment that puts it in the table,
        // so the path is at least as long as that statement.
        if reference.path.len() < 4 {
            continue;
        }
        let element = reference.path[reference.path.len() - 2];
        let base = reference.path[reference.path.len() - 4];

        if !matches!(&*base.borrow(), Node::Assignment(_)) {
            continue;
        }

        let key = match &*element.borrow() {
            Node::TableElement(inner) => inner.key,
            _ => continue,
        };
        let value = assignment_source(base)?;

        if insert_table_record(alloc, constructor, key, value, false) {
            invalidated.mark(base);
        }
    }

    Ok(())
}

/// Moves the results of a call or vararg back into a multiple assignment.
fn eliminate_mass_assignments<'a>(
    alloc: &'a Allocator,
    massive: &[(NodeRef<'a>, Info<'a>, NodeRef<'a>, NodeRef<'a>)],
    invalidated: &Invalidated<'a>,
) {
    for (identifier, info, base_assignment, destination) in massive {
        // The simple case elimination may have removed this assignment
        // already, in which case there is nothing left to do.
        if invalidated.is_marked(base_assignment) {
            continue;
        }

        let assignment = info.borrow().assignment;
        let destinations = match &*assignment.borrow() {
            Node::Assignment(inner) => inner.destinations,
            _ => continue,
        };

        if replace_in_list(alloc, destinations, identifier, destination) {
            invalidated.mark(base_assignment);
        }
    }
}

/// Turns the registers of a generic `for` loop into its variables.
fn eliminate_iterators<'a>(
    alloc: &'a Allocator,
    iterators: &[(Info<'a>, NodeRef<'a>, NodeRef<'a>)],
    invalidated: &Invalidated<'a>,
) -> Result<()> {
    let mut processed: Vec<NodeRef<'a>> = Vec::new();

    for (info, source, warp) in iterators {
        if traverse::contains(&processed, warp) {
            continue;
        }
        let assignment = info.borrow().assignment;

        let (controls, destinations) = {
            let borrowed = warp.borrow();
            let Node::IteratorWarp(inner) = &*borrowed else {
                continue;
            };
            let destinations = match &*assignment.borrow() {
                Node::Assignment(inner) => traverse::list_contents(inner.destinations),
                _ => Vec::new(),
            };
            (inner.controls, destinations)
        };

        let mut control_contents = traverse::list_contents(controls);

        // `for a in b` iterates over `b` itself when `b` is not a call, in
        // which case the first control is kept.
        let mut prefix = None;
        if destinations.len() == 2 && control_contents.len() == 3 {
            prefix = Some(control_contents.remove(0));
        }

        for (index, slot) in destinations.iter().enumerate() {
            let Some(slot_number) = identifier_slot(slot) else {
                continue;
            };
            let Some(control) = control_contents.get(index) else {
                continue;
            };
            let Some(control_number) = identifier_slot(control) else {
                continue;
            };
            if control_number != slot_number {
                return Err(internal("a loop variable does not match its register"));
            }
        }

        let replacement = match prefix {
            Some(prefix) => vec![prefix],
            None => vec![*source],
        };
        if let Node::IteratorWarp(inner) = &mut *warp.borrow_mut() {
            inner.controls = expressions(alloc, replacement);
        }

        processed.push(*warp);
        invalidated.mark(assignment);
    }

    Ok(())
}

// -- rechecking -------------------------------------------------------------

/// Runs the pass again over the parts of the tree that hold references which
/// could not be eliminated the first time round.
///
/// A reference that was ambiguous before may be unambiguous now that the
/// surrounding registers have been inlined.
fn recheck_unsafe_cases<'a>(
    alloc: &'a Allocator,
    root: NodeRef<'a>,
    unsafe_slots: &[Info<'a>],
    options: Options,
    invalidated: &Invalidated<'a>,
) -> Result<()> {
    if unsafe_slots.is_empty() {
        return Ok(());
    }

    let mut blocks: Vec<NodeRef<'a>> = Vec::new();
    let mut slots: Vec<(u32, u32)> = Vec::new();
    for info in unsafe_slots {
        let borrowed = info.borrow();
        slots.push((borrowed.slot, borrowed.slot_id));
        if options.unwarped {
            continue;
        }
        for reference in borrowed.references.iter().skip(1) {
            if let Some(block) = reference
                .path
                .iter()
                .rev()
                .find(|node| matches!(&*node.borrow(), Node::Block(_)))
                .copied()
                && !traverse::contains(&blocks, block)
            {
                blocks.push(block);
            }
        }
    }

    if blocks.is_empty() && !options.unwarped {
        return Ok(());
    }

    let invalidated = *invalidated;
    let mut callback = |node: NodeRef<'a>| {
        if !options.unwarped && !matches!(&*node.borrow(), Node::Block(_)) {
            return;
        }

        cleanup_invalid_nodes(alloc, node, &invalidated);

        let mut new_slots = collect_slots(alloc, node, false, options.unwarped);
        if options.safe_mode {
            new_slots.retain(|info| {
                let borrowed = info.borrow();
                slots.contains(&(borrowed.slot, borrowed.slot_id))
            });
        }

        sort_slots(&mut new_slots);

        let mut data = RefsProcessData::new(new_slots);
        if fill_refs(
            alloc,
            &mut data,
            options.ignore_ambiguous && options.safe_mode,
            options.safe_mode,
        )
        .is_ok()
        {
            let _ = eliminate_simple_cases(alloc, &data.simple, &invalidated);
        }
    };

    if options.unwarped {
        simplify_ast(alloc, root, &mut callback);
    } else {
        for block in blocks {
            simplify_ast(alloc, block, &mut callback);
        }
    }

    Ok(())
}

// -- simplification ---------------------------------------------------------

/// Marks the calls that are really method calls.
///
/// The compiler passes the receiver as the first argument; recognising it lets
/// the slot handling drop that argument and inline a register that would
/// otherwise have too many references.
struct SimplifyVisitor<'a, 'c> {
    alloc: &'a Allocator,
    callback: &'c mut dyn FnMut(NodeRef<'a>),
    dirty: bool,
    root: Option<NodeRef<'a>>,
}

impl<'a, 'c> SimplifyVisitor<'a, 'c> {
    fn new(alloc: &'a Allocator, callback: &'c mut dyn FnMut(NodeRef<'a>)) -> Self {
        SimplifyVisitor {
            alloc,
            callback,
            dirty: false,
            root: None,
        }
    }

    fn flush(&mut self, node: NodeRef<'a>) {
        if self.dirty {
            (self.callback)(node);
            self.dirty = false;
        }
    }

    fn try_make_method_call(&mut self, node: NodeRef<'a>, function: NodeRef<'a>) {
        let (arguments, is_method) = match &*node.borrow() {
            Node::FunctionCall(inner) => (inner.arguments, inner.is_method),
            _ => return,
        };
        if is_method {
            return;
        }

        let mut argument_contents = traverse::list_contents(arguments);
        let Some(first) = argument_contents.first().copied() else {
            return;
        };

        let Node::TableElement(element) = &*function.borrow() else {
            return;
        };
        let (table, key) = (element.table, element.key);

        // The receiver is the callee's table, passed a second time as a hidden
        // argument.
        let matches_table = identifier_slot(first) == identifier_slot(table)
            && identifier_name(first) == identifier_name(table)
            && identifier_kind(first) == identifier_kind(table)
            && matches!(&*first.borrow(), Node::Identifier(_))
            && matches!(&*table.borrow(), Node::Identifier(_));
        if !matches_table {
            return;
        }

        // A string key can be written as a method call; anything else cannot,
        // so the receiver has to stay an ordinary argument.
        let key_is_valid = match &*key.borrow() {
            Node::Identifier(identifier) => identifier.kind == IdentifierKind::Slot,
            Node::Constant(constant) => matches!(constant.value, ConstantValue::String(_)),
            _ => false,
        };
        if !key_is_valid {
            return;
        }

        argument_contents.remove(0);
        set_list_contents(self.alloc, arguments, argument_contents);
        if let Node::FunctionCall(inner) = &mut *node.borrow_mut() {
            inner.is_method = true;
        }
        self.dirty = true;
    }
}

impl<'a> Visitor<'a> for SimplifyVisitor<'a, '_> {
    fn visit(&mut self, node: NodeRef<'a>) -> bool {
        if self.root.is_none() {
            self.root = Some(node);
        }

        let call = match &*node.borrow() {
            Node::FunctionCall(inner) if !inner.is_method => Some(inner.function),
            _ => None,
        };
        if let Some(function) = call {
            self.try_make_method_call(node, function);
        }

        true
    }

    fn leave(&mut self, node: NodeRef<'a>) {
        // The outermost node is the last one to be left, so it is where a
        // change that no block reported gets reported.
        if self
            .root
            .is_some_and(|root| traverse::same_node(root, node))
        {
            self.root = None;
            self.flush(node);
            return;
        }

        if matches!(&*node.borrow(), Node::Block(_)) {
            self.flush(node);
        }
    }
}

// -- multiple results -------------------------------------------------------

/// Moves the results of a call back to the statement that consumed them.
///
/// A call that produces several values is compiled into a `MULTRES` marker at
/// every place that reads them, with the call itself in a temporary assignment.
/// The marker is what this rewrites.
struct MultresEliminator<'a, 'c> {
    alloc: &'a Allocator,
    last_value: Option<NodeRef<'a>>,
    invalidated: &'c Invalidated<'a>,
    failure: Option<Error>,
}

impl<'a> Visitor<'a> for MultresEliminator<'a, '_> {
    fn visit(&mut self, node: NodeRef<'a>) -> bool {
        let list = {
            let borrowed = node.borrow();
            match &*borrowed {
                Node::FunctionCall(inner) => Some(inner.arguments),
                Node::Return(inner) => Some(inner.returns),
                _ => None,
            }
        };

        if let Some(list) = list
            && let Some(index) = mul_res_index(list)
        {
            match self.last_value.take() {
                Some(value) => replace_at(self.alloc, list, index, value),
                None => self.failure = Some(internal("a multiple result without a call")),
            }
        }

        true
    }

    fn leave(&mut self, node: NodeRef<'a>) {
        if self.failure.is_some() {
            return;
        }

        let (destination, source) = {
            let borrowed = node.borrow();
            let Node::Assignment(inner) = &*borrowed else {
                return;
            };
            (
                traverse::list_contents(inner.destinations).first().copied(),
                traverse::list_contents(inner.expressions).first().copied(),
            )
        };

        let (Some(destination), Some(source)) = (destination, source) else {
            return;
        };

        if matches!(&*destination.borrow(), Node::MulTres) {
            if !matches!(&*source.borrow(), Node::FunctionCall(_) | Node::Vararg) {
                return;
            }
            // The assignment only existed to hold the results for the marker,
            // so it goes away once they have been used.
            self.last_value = Some(source);
            self.invalidated.mark(node);
            return;
        }

        let expressions = match &*node.borrow() {
            Node::Assignment(inner) => inner.expressions,
            _ => return,
        };
        let Some(index) = mul_res_index(expressions) else {
            return;
        };

        match self.last_value.take() {
            Some(value) => replace_at(self.alloc, expressions, index, value),
            None => self.failure = Some(internal("a multiple result without a call")),
        }
    }
}

/// The position of the `MULTRES` marker in a list, if it has one.
fn mul_res_index<'a>(list: NodeRef<'a>) -> Option<usize> {
    traverse::list_contents(list)
        .iter()
        .position(|node| matches!(&*node.borrow(), Node::MulTres))
}

/// Puts `value` at `index` of a list.
fn replace_at<'a>(alloc: &'a Allocator, list: NodeRef<'a>, index: usize, value: NodeRef<'a>) {
    let mut contents = traverse::list_contents(list);
    contents[index] = value;
    set_list_contents(alloc, list, contents);
}

/// Rewrites `MULTRES` back into the call it stands for.
fn eliminate_multres<'a>(
    alloc: &'a Allocator,
    root: NodeRef<'a>,
    invalidated: &Invalidated<'a>,
) -> Result<()> {
    let mut visitor = MultresEliminator {
        alloc,
        last_value: None,
        invalidated,
        failure: None,
    };
    traverse::traverse(&mut visitor, root);

    if let Some(error) = visitor.failure {
        return Err(error);
    }

    cleanup_invalid_nodes(alloc, root, invalidated);
    Ok(())
}
