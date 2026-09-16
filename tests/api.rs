//! End to end tests for the public API.
//!
//! These compile a snippet, decompile it and compile the result again, so that
//! what is checked is the whole pipeline rather than one pass.

mod support;

use support::*;

#[test]
fn a_chunk_decompiles_to_source_that_compiles_again() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = concat!(
        "local M = {}\n",
        "\n",
        "function M.add(a, b)\n",
        "\treturn a + b\n",
        "end\n",
        "\n",
        "function M.describe(t)\n",
        "\tlocal parts = {}\n",
        "\tfor key, value in pairs(t) do\n",
        "\t\tif type(value) == \"string\" then\n",
        "\t\t\tparts[#parts + 1] = key .. \"=\" .. value\n",
        "\t\tend\n",
        "\tend\n",
        "\treturn table.concat(parts, \", \")\n",
        "end\n",
        "\n",
        "return M\n",
    );

    let dump = compile_source(&luajit, "chunk", source, true);
    let text =
        luajit_ripper::decompile(&dump, &Default::default()).expect("the chunk must decompile");

    eprintln!("{text}");
    assert!(text.contains("local M = {"), "{text}");
    assert!(text.contains("add = function (a, b)"), "{text}");
    assert!(text.contains("for key, value in pairs(t) do"), "{text}");
    assert!(text.contains("if type(value) == \"string\" then"), "{text}");
    assert!(
        text.contains("parts[#parts + 1] = key .. \"=\" .. value"),
        "{text}"
    );

    // The result has to be Lua the same compiler accepts.
    let recompiled = compile_source(&luajit, "chunk_again", &text, false);
    luajit_ripper::bytecode::parse(&recompiled)
        .expect("the decompiled source must compile back to a dump");
}

#[test]
fn the_indentation_of_the_output_can_be_chosen() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local function f(a)\n\tif a then\n\t\tg()\n\tend\nend\nreturn f\n";
    let dump = compile_source(&luajit, "indent", source, true);

    let options = luajit_ripper::Options {
        indent: luajit_ripper::Indent::Spaces(2),
        ..Default::default()
    };
    let text = luajit_ripper::decompile(&dump, &options).expect("the snippet must decompile");
    assert!(text.contains("\n  if a then"), "{text}");
    assert!(!text.contains('\t'), "{text}");
}

#[test]
fn bit_operations_can_be_written_as_library_calls() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    let source = "local function f(a, b)\n\treturn a & b\nend\nreturn f\n";
    let dump = compile_source(&luajit, "bitop", source, true);

    let operators =
        luajit_ripper::decompile(&dump, &Default::default()).expect("the snippet must decompile");
    assert!(operators.contains("a & b"), "{operators}");

    let options = luajit_ripper::Options {
        bitop_style: luajit_ripper::BitOpStyle::BitLibrary,
        ..Default::default()
    };
    let calls = luajit_ripper::decompile(&dump, &options).expect("the snippet must decompile");
    assert!(calls.contains("bit.band(a, b)"), "{calls}");
}

#[test]
fn a_malformed_dump_is_reported_as_an_error() {
    let error = luajit_ripper::decompile(b"not a dump", &Default::default())
        .expect_err("garbage must not decompile");
    assert!(
        matches!(error, luajit_ripper::Error::BadMagic),
        "unexpected error: {error}"
    );
}

#[test]
fn a_called_function_literal_is_wrapped_in_parentheses() {
    let Some(luajit) = luajit() else {
        eprintln!("luajit not found, skipping");
        return;
    };

    // `a or function () end()` is not valid Lua: a function literal can only be
    // called when it is wrapped, as in `a or (function () end)()`. The written
    // source has to carry the parentheses, otherwise the result does not
    // compile.
    let source = concat!(
        "local function fallback()\n",
        "\treturn 1\n",
        "end\n",
        "local function pick(a)\n",
        "\treturn a or (function ()\n",
        "\t\treturn 2\n",
        "\tend)()\n",
        "end\n",
        "return pick\n",
    );
    let dump = compile_source(&luajit, "called_literal", source, true);
    let text =
        luajit_ripper::decompile(&dump, &Default::default()).expect("the snippet must decompile");

    assert!(
        text.contains("(function ()"),
        "the call target has to be wrapped: {text}"
    );
    assert!(
        !text.contains("or function ()"),
        "an unwrapped function literal cannot be called: {text}"
    );

    compile_source(&luajit, "called_literal_again", &text, false);
}
