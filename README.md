# luajit-ripper

A LuaJIT 2.1 bytecode decompiler.

The crate is a port of [LJD](https://github.com/Aussiemon/ljd), a LuaJIT bytecode decompiler written in Python.

## Usage

### As a CLI Tool

```shell
# install from prebuilt binary
cargo binstall luajit-ripper

# or compile from source
cargo install luajit-ripper --features cli
```

```shell
luajit-ripper --input <dump.ljbc> [--output <file.lua>]
luajit-ripper --input <dir of dumps> --output <dir> [--module-structure]
```

#### Options

| Option | Effect |
| --- | --- |
| `-i`, `--input <PATH>` | Dump, or directory of dumps, to read. |
| `-o`, `--output <PATH>` | Where to write. Left out, a single dump goes to stdout. |
| `--module-structure` | Name every output after the chunk name in its dump. |
| `-j`, `--threads <N>` | How many dumps to decompile at once; `0` picks a number. |
| `--indent`, `--indent-width` | Tabs, or a chosen number of spaces per level. |
| `--slots` | Let unnamed registers carry the ids of the definitions they may refer to. |
| `--syntactic-sugar` | Write `t.f = function() end` as `function t.f() end`. |
| `--bit-library` | Write `bit.band(a, b)` instead of `a & b`. |
| `--mark-errors` | Write the regions that cannot be structured as code instead of failing their chunk. |

#### Outputs

A single dump without `--output` is written to stdout. A directory of dumps is decompiled into an output directory with one worker per core (`--threads` changes that), and every worker keeps an allocator of its own, which is reset after each dump instead of unwinding what the passes built.

A dump keeps the name of the file it was compiled from, such as `@modules/logic/rouge/map/Foo.lua`. With `--module-structure` that name becomes the path below the output directory, leading `@` and all, which turns a flat collection of hashed dumps back into the tree it was built from. A dump whose name is missing (a stripped dump) or unusable keeps the path of its input file instead, and the run says so.

Without it, the layout of the input is mirrored: `sub/a.ljbc` becomes `sub/a.lua`.

### As a Library

```shell
cargo add luajit-ripper
```

`decompile` runs the whole pipeline: it parses the dump, builds a control flow graph, rewrites the graph back into statements and writes the source. A chunk is decompiled in one call, and the result is a complete chunk, so it can be compiled again as it is:

```rust
use luajit_ripper::{Options, decompile};

let dump = std::fs::read("chunk.ljbc").expect("the dump should be readable");
let source = decompile(&dump, &Options::default()).expect("the chunk should decompile");
print!("{source}");
```

The stages are public as well, so a caller that wants a disassembly, or wants to work on the graph itself, can stop halfway. Every stage reads and writes the same arena, which the caller owns and releases in one go:

```rust
use oxc_allocator::Allocator;
use luajit_ripper::{bytecode, listing};

let alloc = Allocator::new();
let chunk = bytecode::parse(&alloc, &dump).expect("the dump should parse");
print!("{}", listing::dump(&chunk));
```

| Module | What it does |
| --- | --- |
| `bytecode` | Parses the dump into prototypes, instructions and constants. |
| `listing` | Renders the parsed dump as a disassembly, byte for byte what `luajit -bl` writes. |
| `ast` | Builds a control flow graph and rewrites it back into structured statements. |
| `lua` | Renders the structured AST as source text. |

#### Options

`Options` is what changes the output; `decompile_ast` takes the same options when the graph is being driven by hand.

| Field | Default | Effect |
| --- | --- | --- |
| `indent` | `Indent::Tabs` | Tabs, or a chosen number of spaces. |
| `bitop_style` | `BitOpStyle::Operator` | `a & b`, or `bit.band(a, b)`. |
| `on_function_error` | `OnFunctionError::Fail` | Stop at the first region that cannot be structured, or recover it. |
| `show_slot_ids` | `false` | Let unnamed registers carry the ids of the definitions they may refer to. |
| `function_definition_sugar` | `false` | Write `t.f = function() end` as `function t.f() end`. |

## Testing

```text
cargo test # library and compiler round trips, seconds
cargo test --features cli # the same, plus the command line tool
```

The round trips compile the decompiled source again, so they need a LuaJIT: `LUAJIT=<path>` picks one, and `luajit` on `PATH` is the default.

## Limitations

Rebuilding source from bytecode is not always possible, because a jump target does not record which construct produced it. Two shapes of input fall outside what the passes can recover:

* A branch whose two arms only meet again through a chain of empty jumps cannot be told from straight line code.
* A few graphs come back as statements Lua will not parse, such as a `return` that ends up in front of the statements that follow it. Nothing detects this, so the result has to be compiled to be sure of it.

Both are inherited from the original decompiler.

`OnFunctionError::Mark` turns the first case from a failure into a warning: the region is written as the statements it holds and pointed out with a `-- Decompilation error in this vicinity:` comment. The recovered code is usually right, but a branch that could not be told apart loses the arm that was not taken. A function that could not be finished at all is replaced by an `error("Decompilation failed")` call instead, so the rest of the chunk stays usable.

## License

The original LJD project is authored by Andrian Nord and licensed under the MIT license. The fork this port is based on is licensed under the GNU General Public License version 3. Therefore, this crate is licensed under the GPLv3 as well.

The bytecode dump format implemented here is the one documented in LuaJIT's `lj_bcdump.h`. LuaJIT itself is released under the MIT license.

## AI Usage Disclosure

This project is my first fully vibe-coded project. I do not understand what the program does in detail and I let LLM agents to do almost all the work. However, I did my best effort to guide them and review the parts I can understand. I also required them to do cross-validation with the original LJD decompiler, which is human-coded.

I did this primarily for my own needs. I have been using it in reverse-engineering work over a real-world LuaJIT application containing ~20000 files. It works reasonably well.

Anyway, use it at your own risk.
