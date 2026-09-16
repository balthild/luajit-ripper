//! Small shared operations on the AST.
//!
//! This is a port of ljd's `ast/helpers.py`: comparing two nodes for equality
//! without comparing identity, folding a value into a table constructor, and
//! checking whether a subtree reads a given table.

use super::nodes::*;
use super::traverse::{self, Visitor};

/// Whether two nodes describe the same thing.
///
/// Only identifiers, constants and table elements are compared; anything else
/// counts as unequal. `strict` says the caller expects one of those three
/// kinds, which ljd turns into an assertion.
pub fn is_equal(a: &NodeRef, b: &NodeRef, strict: bool) -> bool {
    let nested = {
        let (borrowed_a, borrowed_b) = (a.borrow(), b.borrow());
        match (&*borrowed_a, &*borrowed_b) {
            (Node::Identifier(left), Node::Identifier(right)) => {
                return left.kind == right.kind && left.slot == right.slot;
            }
            (Node::Constant(left), Node::Constant(right)) => {
                return left.value == right.value;
            }
            (Node::TableElement(left), Node::TableElement(right)) => (
                left.table.clone(),
                right.table.clone(),
                left.key.clone(),
                right.key.clone(),
            ),
            _ => {
                // Two nodes of different kinds are never equal. Two nodes of
                // the same kind that are not one of the three comparable kinds
                // are a comparison the caller asked for by mistake. ljd turns
                // that into an assertion, but only for a strict comparison;
                // callers that pass `false` mean "if in doubt, not equal".
                let same_kind =
                    std::mem::discriminant(&*borrowed_a) == std::mem::discriminant(&*borrowed_b);
                debug_assert!(
                    !(strict && same_kind),
                    "a strict comparison cannot compare these two nodes"
                );
                return false;
            }
        }
    };

    let (table_a, table_b, key_a, key_b) = nested;
    is_equal(&table_a, &table_b, strict) && is_equal(&key_a, &key_b, strict)
}

/// Whether any part of `node` reads `table`.
///
/// The check is used to keep a table constructor from being folded into itself:
/// if the constructor ends up containing an expression that reads the table,
/// the reads would refer to the wrong value.
pub fn has_same_table(node: &NodeRef, table: &NodeRef) -> bool {
    let mut checker = TableChecker {
        table: table.clone(),
        found: false,
        function_depth: 0,
    };
    traverse::traverse(&mut checker, node);
    checker.found
}

struct TableChecker {
    table: NodeRef,
    found: bool,
    function_depth: usize,
}

impl Visitor for TableChecker {
    fn visit(&mut self, node: &NodeRef) -> bool {
        if self.found {
            return false;
        }

        match &*node.borrow() {
            Node::FunctionDefinition(_) => self.function_depth += 1,
            Node::TableElement(inner) => {
                if is_equal(&self.table, &inner.table, true) {
                    self.found = true;
                }
            }
            Node::Identifier(inner) => {
                // A closure can capture the table as an upvalue, which reads it
                // just as much as an index does.
                let captures_table = self.function_depth > 0
                    && inner.kind == IdentifierKind::Upvalue
                    && inner.name.is_some()
                    && self
                        .table
                        .borrow()
                        .identifier()
                        .and_then(|id| id.name.as_ref())
                        == inner.name.as_ref();
                if captures_table {
                    self.found = true;
                }
            }
            _ => {}
        }

        true
    }

    fn leave(&mut self, node: &NodeRef) {
        if matches!(&*node.borrow(), Node::FunctionDefinition(_)) {
            self.function_depth -= 1;
        }
    }
}

/// Adds `key = value` to a table constructor.
///
/// A positive integer key becomes part of the array part, anything else a
/// record. Returns whether the record could be added: an array entry that is
/// already taken by a different value is left alone.
pub fn insert_table_record(
    constructor: &NodeRef,
    key: &NodeRef,
    value: &NodeRef,
    replace: bool,
) -> bool {
    let (array, records) = match &*constructor.borrow() {
        Node::TableConstructor(inner) => (inner.array.clone(), inner.records.clone()),
        _ => return false,
    };

    // `...` spreads the results of a call or a vararg across the rest of the
    // constructor, so the value itself is the record.
    if matches!(&*key.borrow(), Node::MulTres) {
        push(&records, value.clone());
        return true;
    }

    let index = match &*key.borrow() {
        Node::Constant(inner) => match &inner.value {
            ConstantValue::Integer(value) if *value >= 0 => Some(*value as usize),
            _ => None,
        },
        _ => None,
    };

    if let Some(position) = index {
        let mut array_contents = list_contents(&array);

        // `t = {nil, x}` uses an explicit nil for the hole at index one.
        if position == 1 && array_contents.is_empty() {
            array_contents.push(node(Node::ArrayRecord(Box::new(ArrayRecord {
                value: primitive(PrimitiveKind::Nil),
                meta: Meta::default(),
            }))));
        }

        if position > array_contents.len() {
            set_list_contents(&array, array_contents);
        } else {
            let record = node(Node::ArrayRecord(Box::new(ArrayRecord {
                value: value.clone(),
                meta: Meta::default(),
            })));

            if array_contents.is_empty() || position == array_contents.len() {
                array_contents.push(record);
                set_list_contents(&array, array_contents);
                return true;
            }
            if replace {
                array_contents[position] = record;
                set_list_contents(&array, array_contents);
                return true;
            }

            let current = array_contents[position].clone();
            // The entry is a record; whether the slot is free is decided by the
            // value inside it.
            let current_value = match &*current.borrow() {
                Node::ArrayRecord(record) => record.value.clone(),
                _ => current.clone(),
            };
            let is_hole = matches!(&*current_value.borrow(), Node::Primitive(inner)
                if inner.kind == PrimitiveKind::Nil);
            if !is_hole {
                set_list_contents(&array, array_contents);
                return false;
            }

            array_contents[position] = record;
            set_list_contents(&array, array_contents);
            return true;
        }
    }

    // Records are not as critical as array entries: whatever the order, both
    // values end up in the table.
    let record = node(Node::TableRecord(Box::new(TableRecord {
        key: key.clone(),
        value: value.clone(),
        meta: Meta::default(),
    })));

    let mut records_contents = list_contents(&records);
    if records_contents.is_empty() {
        records_contents.push(record);
        set_list_contents(&records, records_contents);
        return true;
    }

    let last = records_contents.last().cloned().expect("checked above");
    let is_multi = matches!(&*last.borrow(), Node::FunctionCall(_) | Node::Vararg);
    if is_multi {
        let position = records_contents.len() - 1;
        records_contents.insert(position, record);
    } else {
        records_contents.push(record);
    }
    set_list_contents(&records, records_contents);

    true
}
