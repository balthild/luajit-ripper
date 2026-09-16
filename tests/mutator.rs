//! Tests for the cleanups that run after the control flow is structured.
//!
//! `primary_pass` turns `if` statements nested in an `else` into `elseif`
//! chains, and folds assignments that only fill in fields of a table back into
//! its constructor. Both are visible in the text the writer produces, which is
//! what these tests look at.

mod support;

use support::*;

/// Decompiles a snippet into Lua source.
fn decompile(luajit: &str, name: &str, source: &str) -> String {
    let dump = compile_source(luajit, name, source, true);
    luajit_ripper::decompile(&dump, &Default::default())
        .unwrap_or_else(|error| panic!("{name} must decompile: {error}"))
}

#[test]
fn an_if_in_an_else_becomes_an_elseif() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = concat!(
        "local function f(a)\n",
        "\tif a == 1 then\n",
        "\t\tg(1)\n",
        "\telseif a == 2 then\n",
        "\t\tg(2)\n",
        "\telse\n",
        "\t\tg(3)\n",
        "\tend\n",
        "end\n",
        "return f\n",
    );
    let text = decompile(&luajit, "elseif_chain", source);

    assert!(text.contains("elseif"), "the chain must be merged: {text}");
    assert!(
        !text.contains("else\n\t\tif"),
        "no if should be nested in an else: {text}"
    );
}

#[test]
fn table_field_assignments_are_folded_into_the_constructor() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = concat!(
        "local t = {}\n",
        "t.x = 1\n",
        "t.y = 2\n",
        "t[1] = 3\n",
        "return t\n",
    );
    let text = decompile(&luajit, "table_fold", source);

    assert!(text.contains("x = 1"), "{text}");
    assert!(text.contains("y = 2"), "{text}");
    assert!(text.contains("3,"), "{text}");
    assert_eq!(
        text.matches("t.x").count() + text.matches("t.y").count() + text.matches("t[1]").count(),
        0,
        "the fields should be inside the constructor: {text}"
    );
}

#[test]
fn a_read_of_the_table_keeps_it_out_of_the_constructor() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    // `t.y = t.x` reads the table while it is being built, so it has to stay a
    // statement of its own.
    let source = concat!("local t = {}\n", "t.x = 1\n", "t.y = t.x\n", "return t\n",);
    let text = decompile(&luajit, "table_read", source);

    assert!(
        text.contains("t.y = t.x"),
        "the read has to stay outside the constructor: {text}"
    );
}

#[test]
fn a_field_constructor_is_compared_safely() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    // The constructor is assigned to a computed field of `t`, and the following
    // statement writes to the very same field. Checking whether the field
    // assignments belong to the constructor means comparing the two keys, which
    // here are both `i - 1`. Two operators are not something `is_equal` can
    // compare, so it has to answer `false` instead of tripping its own check.
    let source = concat!(
        "local t = {}\n",
        "local i = 1\n",
        "t[i - 1] = {}\n",
        "t[i - 1][1] = 1\n",
        "return t\n",
    );
    let text = decompile(&luajit, "computed_field_constructor", source);

    assert!(
        text.contains("t[i - 1][1] = 1"),
        "the nested write has to survive: {text}"
    );
}
