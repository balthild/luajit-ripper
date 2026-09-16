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
| `on_function_error` | `OnFunctionError::Fail` | Stop at the first function that cannot be decompiled, or leave an `error("Decompilation failed")` call in its place and carry on. |
| `show_slot_ids` | `false` | Let unnamed registers carry the ids of the definitions they may refer to. |
| `function_definition_sugar` | `false` | Write `t.f = function() end` as `function t.f() end`. |

`OnFunctionError::Mark` is what makes a whole chunk decompile even when one
function cannot be; the rest of the chunk stays usable.

## Limits

Rebuilding source from bytecode is not always possible, because a jump target
does not record which construct produced it. Two shapes of input fall outside
what the passes can recover:

* A branch whose two arms only meet again through a chain of empty jumps cannot
  be turned back into an `if`. `OnFunctionError` decides whether the chunk fails
  or only that function is marked.
* A few graphs come back as statements Lua will not parse, such as a `return`
  that ends up in front of the statements that follow it. Nothing detects this,
  so the result has to be compiled to be sure of it.

Both are inherited from the original decompiler.

## Examples

```text
cargo run --release --example decompile_file -- chunk.ljbc [--spaces] [--slots]
cargo run --release --example decompile_dir  -- <input dir> <output dir> [--mark-errors]
cargo run --release --example listing       -- chunk.ljbc
```

## License

GPL-3.0-only. See `LICENSE` and `NOTICE.md`.
