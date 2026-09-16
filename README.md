# luajit-ripper

A LuaJIT 2.1 bytecode decompiler, as a Rust library.

It takes a raw LuaJIT bytecode dump and gives back Lua source:

```rust
use luajit_ripper::{Options, decompile};

let dump = std::fs::read("chunk.ljbc").expect("the dump should be readable");
let source = decompile(&dump, &Options::default()).expect("the chunk should decompile");
print!("{source}");
```

The crate is a port of [LJD](https://github.com/Night-witch/ljd), the
LuaJIT raw bytecode decompiler. It is laid out as a pipeline of independent
stages, each of which can be used on its own:

| Module | What it does |
| --- | --- |
| `bytecode` | Parses the dump into prototypes, instructions and constants. |
| `listing` | Renders the parsed dump as a disassembly, byte for byte what `luajit -bl` writes. |
| `ast` | Builds a control flow graph and rewrites it back into structured statements. |
| `lua` | Renders the structured AST as source text. |

## Options

| Field | Default | Effect |
| --- | --- | --- |
| `indent` | `Indent::Tabs` | Tabs, or a chosen number of spaces. |
| `bitop_style` | `BitOpStyle::Operator` | `a & b`, or `bit.band(a, b)`. |
| `on_function_error` | `OnFunctionError::Fail` | Stop at the first region that cannot be structured, or recover it. |
| `show_slot_ids` | `false` | Let unnamed registers carry the ids of the definitions they may refer to. |
| `function_definition_sugar` | `false` | Write `t.f = function() end` as `function t.f() end`. |

## Limits

Rebuilding source from bytecode is not always possible, because a jump target
does not record which construct produced it. Two shapes of input fall outside
what the passes can recover:

* A branch whose two arms only meet again through a chain of empty jumps cannot
  be told from straight line code.
* A few graphs come back as statements Lua will not parse, such as a `return`
  that ends up in front of the statements that follow it. Nothing detects this,
  so the result has to be compiled to be sure of it.

Both are inherited from the original decompiler.

`OnFunctionError::Mark` turns the first case from a failure into a warning: the
region is written as the statements it holds and pointed out with a
`-- Decompilation error in this vicinity:` comment. The recovered code is
usually right, but a branch that could not be told apart loses the arm that was
not taken. A function that could not be finished at all is replaced by an
`error("Decompilation failed")` call instead, so the rest of the chunk stays
usable.

## Examples

```text
cargo run --release --example decompile_file -- chunk.ljbc [--spaces] [--slots]
cargo run --release --example decompile_dir  -- <input dir> <output dir> [--mark-errors]
cargo run --release --example listing       -- chunk.ljbc
```

## License

GPL-3.0-only. See `LICENSE` and `NOTICE.md`.
