//! Behavior of the decompiler over a corpus of real world bytecode dumps.
//!
//! The corpus is a local dataset that is never committed, so a checkout has
//! none and every test here does nothing. `LJR_CORPUS=<dir>` points at a
//! directory of `.ljbc` files to run them, `LJR_SAMPLE=<n>` decides how much of
//! it they look at and `LJR_SEED=<n>` which part of it; see
//! [`support::Corpus`] for all three.
//!
//! Nothing here knows anything about a particular corpus, so any collection of
//! dumps can be put behind `LJR_CORPUS` and checked just as well.

mod support;

use support::{Corpus, Unwarped};

// MARK: parsing

#[test]
fn every_dump_parses() {
    let Some(corpus) = Corpus::load() else {
        return;
    };
    let files = &corpus.files;

    let mut failures = Vec::new();
    let mut bytes = 0usize;
    let mut prototypes_seen = 0usize;

    for file in files {
        let data = std::fs::read(file).expect("dump should be readable");
        bytes += data.len();
        match support::try_parse_dump(&data) {
            Ok(chunk) => prototypes_seen += support::count_prototypes(chunk),
            Err(error) => failures.push(format!("{}: {error}", file.display())),
        }
    }

    let total = files.len();
    if !failures.is_empty() {
        for failure in failures.iter().take(20) {
            eprintln!("{failure}");
        }
        panic!("{total} dumps failed to parse: {}", failures.len());
    }

    eprintln!(
        "parsed {total} dumps ({bytes} bytes, {prototypes_seen} prototypes, {} bytes/dump avg)",
        bytes / total
    );
}

/// Cuts and corrupts dumps and expects the parser to survive all of them.
#[test]
fn truncated_dumps_are_rejected_without_panicking() {
    let Some(corpus) = Corpus::load() else {
        return;
    };

    for file in corpus.files.iter().take(64) {
        let data = std::fs::read(file).expect("dump should be readable");
        for cut in [0, 1, 3, 4, 5, 8, 16, 64, 256] {
            if cut >= data.len() {
                continue;
            }
            let _ = luajit_ripper::bytecode::parse(support::arena(), &data[..cut]);
        }
        // Flipping bits must never take the parser down either.
        let mut mutated = data.clone();
        for index in (0..mutated.len()).step_by(97) {
            mutated[index] ^= 0xa5;
            let _ = luajit_ripper::bytecode::parse(support::arena(), &mutated);
            mutated[index] ^= 0xa5;
        }
    }
}

// MARK: graph building

#[test]
fn every_dump_builds_an_ast() {
    let Some(corpus) = Corpus::load() else {
        return;
    };
    let files = &corpus.files;
    let mut failures = Vec::new();
    let mut blocks = 0usize;

    for file in files {
        let data = std::fs::read(file).expect("dump should be readable");
        let chunk = match support::try_parse_dump(&data) {
            Ok(chunk) => chunk,
            Err(error) => {
                failures.push(format!("{}: parse: {error}", file.display()));
                continue;
            }
        };

        // Build every prototype on its own, so that a failure can be pinned to
        // a single function.
        for prototype in support::prototypes(chunk) {
            match luajit_ripper::ast::builder::build_function(support::arena(), chunk, prototype) {
                Ok(root) => blocks += support::count_blocks(root),
                Err(error) => failures.push(format!(
                    "{}: first_line {}: {error}",
                    file.display(),
                    prototype.first_line
                )),
            }
        }
    }

    if !failures.is_empty() {
        for failure in failures.iter().take(20) {
            eprintln!("{failure}");
        }
        panic!(
            "{} functions failed to build an AST (out of {} dumps)",
            failures.len(),
            files.len()
        );
    }

    eprintln!("built {blocks} blocks from {} dumps", files.len());
}

// MARK: locals

/// Runs the local variable and register elimination passes over the whole
/// corpus.
///
/// The real world corpus is not stripped, so every register that holds a source
/// level variable has to come back with a name. After the temporaries have been
/// inlined almost nothing should be left as a plain register.
#[test]
fn locals_are_recovered_for_the_whole_corpus() {
    use luajit_ripper::ast::nodes::{AssignmentKind, Node};

    let Some(corpus) = Corpus::load() else {
        return;
    };

    let files = &corpus.files;
    let mut named = 0usize;
    let mut unnamed = 0usize;
    let mut definitions = 0usize;
    let mut unnamed_definitions = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for file in files {
        let data = std::fs::read(file).expect("dump should be readable");
        let chunk = match support::try_parse_dump(&data) {
            Ok(chunk) => chunk,
            Err(_) => continue,
        };

        for prototype in support::prototypes(chunk) {
            let Ok(root) =
                luajit_ripper::ast::builder::build_function(support::arena(), chunk, prototype)
            else {
                continue;
            };

            luajit_ripper::ast::mutator::pre_pass(support::arena(), root);
            luajit_ripper::ast::locals::mark_locals(root, false);
            luajit_ripper::ast::locals::mark_local_definitions(support::arena(), root);

            if let Err(error) = luajit_ripper::ast::slotworks::eliminate_temporary(
                support::arena(),
                root,
                luajit_ripper::ast::slotworks::Options {
                    identify_slots: true,
                    ..Default::default()
                },
            ) {
                failures.push(format!("{}: {error}", file.display()));
            }

            for node in luajit_ripper::ast::traverse::walk(root) {
                let borrowed = node.borrow();
                match &*borrowed {
                    Node::Identifier(identifier) => {
                        if identifier.name.is_some() {
                            named += 1;
                        } else if identifier.kind == luajit_ripper::ast::nodes::IdentifierKind::Slot
                        {
                            unnamed += 1;
                        }
                    }
                    Node::Assignment(assignment)
                        if assignment.kind == AssignmentKind::LocalDefinition =>
                    {
                        for destination in
                            luajit_ripper::ast::traverse::list_contents(assignment.destinations)
                        {
                            if let Node::Identifier(identifier) = &*destination.borrow() {
                                definitions += 1;
                                unnamed_definitions += usize::from(identifier.name.is_none());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let total = named + unnamed;
    eprintln!(
        "named {named} of {total} register references ({}%), \
         {definitions} local definitions, {unnamed_definitions} of them unnamed, \
         {} slot elimination failures",
        named * 100 / total.max(1),
        failures.len()
    );

    for failure in failures.iter().take(20) {
        eprintln!("{failure}");
    }

    assert!(
        named * 5 > total * 3,
        "only {named} of {total} register references were named"
    );
    assert_eq!(
        unnamed_definitions, 0,
        "a local declaration has to know the name of the variable it declares"
    );
    assert!(
        failures.is_empty(),
        "{} functions could not have their registers eliminated",
        failures.len()
    );
}

// MARK: unwarping

/// Structures every function of the corpus, and expects recovery to pick up
/// whatever the strict pass gives up on.
#[test]
fn every_function_unwarps() {
    let Some(corpus) = Corpus::load() else {
        return;
    };

    let mut functions = 0usize;
    let mut recovered = 0usize;
    let mut unrecovered: Vec<String> = Vec::new();

    for file in &corpus.files {
        let data = std::fs::read(file).expect("dump should be readable");
        let chunk = match support::try_parse_dump(&data) {
            Ok(chunk) => chunk,
            Err(_) => continue,
        };

        for prototype in support::prototypes(chunk) {
            functions += 1;

            match support::unwarp_function(chunk, prototype) {
                Some(Unwarped::Strict) => {}
                Some(Unwarped::Recovered) => recovered += 1,
                Some(Unwarped::Failed(error)) => unrecovered.push(format!(
                    "{} first_line {}: {error}",
                    file.display(),
                    prototype.first_line
                )),
                None => unrecovered.push(format!(
                    "{} first_line {}: the registers could not be eliminated",
                    file.display(),
                    prototype.first_line
                )),
            }
        }
    }

    eprintln!("unwarped {functions} functions, recovery saved {recovered} of them");
    for failure in unrecovered.iter().take(20) {
        eprintln!("{failure}");
    }
    assert!(
        unrecovered.is_empty(),
        "recovery left {} function(s) behind",
        unrecovered.len()
    );
}
