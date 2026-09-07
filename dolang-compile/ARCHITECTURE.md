# Compiler Frontend Architecture (dolang-compile)

The `dolang-compile` crate transforms Do source code into bytecode through a
multi-phase pipeline: lex → parse → elaborate → lower → emit. Lexical analysis
converts indentation into explicit indent/dedent tokens. Parsing uses recursive
descent with Pratt expression parsing to build an AST. Elaboration performs name
resolution and validates control flow. Lowering converts the AST to a control
flow graph. Emission translates the CFG into bytecode. The `Compiler` type
orchestrates the process and maintains shared tables (source lines, symbols,
interned strings, constant pool, diagnostics).

Elaboration stores binding provenance directly in variables. Resolutions use
lexical scope depth and variable index; compilation needs no document table.
With `Config::document(true)`, a separate pass annotates the elaborated AST with
document node IDs and builds a table retained by `Unit`. Borrowed scope frames
follow the recursive traversal and annotate the AST's variable storage directly;
lexical scope depth is independent of document parentage. Token visits read the
annotations; without indexing they emit no node IDs. Lowering does not consume
document metadata.
