# dolang-lsp Architecture

LSP server for Do using `tower-lsp-server` and `tokio`. Document state
recompiles on every change to provide diagnostics and semantic tokens.

Compilation runs on a dedicated worker thread, so handlers never block the
event loop. Handlers send the worker each document's text. Each open or edit
starts a revision, a shared future of its `Document`, which replaces the
document's newest revision unless a newer one arrived first. A request awaits
the newest revision, since its positions refer to the text the client last
sent; ordering requests after edits is left to the client. If the worker passes
over a revision, superseded or failing to compile it, the request answers from
the last stored `Document`. The worker keeps the
`Unit<'static>` it last compiled for each open document, reusing it when the
text and settings are unchanged. It projects each unit into a `Document`
holding the unit (whose source is the document's text), semantic tokens,
references, declarations, symbols and quick fixes. An applier task stores each
`Document`, replacing the old one whole, then publishes its diagnostics.

tower-lsp-server runs handlers concurrently, so edits can reach the worker out
of order. Each piece of work carries a stamp: an epoch, which each open of the
document advances, and the client's version. The worker and the applier drop
anything older than what they already hold. A close also goes through the
worker, so the applier drops the document only after any projection still in
flight.

The worker also type checks each document once its edits have settled. A
document's projection publishes its compile diagnostics at once, and schedules a
check for a short delay later; each newer edit pushes the check back. The worker
waits on its request channel with `recv_timeout` until the earliest check is
due. A check has the document's unit, compiled as its module if it lies under
the configured module path and as a script otherwise. The unchanged unit of a
save keeps its check. The check publishes the compile diagnostics again,
merged with the checker's, but only while the stored document still has the
stamp it checked. The check also keeps the checker's analysis, which the applier
attaches to that stored document; a saved unit's kept check comes with its
analysis. Hover asks the analysis what the checker concluded at the cursor, and
for a variable shows its name and type above the declaration's doc comment,
without its `let` label.

The check also keeps the units of the modules it read from source, keyed by the
checker's name for each, with the document's own. Go to definition and hover on
an item of another module look it up there (`lookup.rs`): an item imported by
name, from its import's node, or a path such as `a.f`, from the analysis's
`Item` judgment. A `pub import` re-export is followed to the module declaring
the item. A member use's `Member` judgments name the class declaring each
member it reached, one per class of a union receiver. A call's `Callee`
judgment, on the member for a method call and on the whole call for a function
call, gives the `@def` it chose, which the lookup finds among the
implementation's siblings in source order. A module the check read from a
bundled typelib has no source, so hover falls back to the static doc index and
go to definition to the import. Hover on a module read from source shows the
comment opening it. When every declaration found is a function,
method or class, here or in another module, hover shows only their docs, whose
signatures stand in for the checked type. A declaration's docs note where it is
declared: its class, qualified by its module outside the document's own, or the
other module whose top level declares it. One signature form serves source and
the doc index (`signature.rs`).
Being the check's own units, they agree with its analysis, and an edit to a
module reaches its importers with their next check.

A check resolves imports through the workspace (`workspace.rs`), which holds a
store for each module path, plus one shared by configurations without one. A
store answers what a module is now. An open document that is the module comes
first, so importers see the client's buffer. Open documents are matched to
modules by each store's module path, not by URI. Next is a source on the
module path, then a bundled typelib unless the configuration turns them off.
An import found nowhere is reported as not found. A check resolves the modules
the document imports, directly or transitively, plus `proc` for the pipe
types, as `compile check` does. The document's own module is never looked up,
so it replaces its bundled copy. Each module's unit is kept until its text
changes.

A store records each module's imports, and those of the open documents checked
through it, by name, whether or not the import resolved. Opening, editing or
closing a document that is a module, a watched file changing on disk, or a check
finding a module changed on disk, invalidates the module. Every open document
that imports it, directly or transitively, is then checked again once the change
settles. Only checks need redoing, since a unit doesn't depend on what it
imports. A store is dropped once no open document checks through it.

When the client allows it, the server registers watchers for `.dol` sources
and configuration files. A source event invalidates the module the file names
in each store, by where it is, so a deleted file is named too. A module held
by an open document ignores its file. A configuration event drops that
directory's cached settings and compiles the open documents under it again.
Each one then finds its configuration anew, nearer ones included, and a store
whose module path changed is rebuilt. Wildcard roots expand only when a
configuration loads, so a new directory they match waits for the next
configuration event.

Configuration files are TOML files (`.dolang-lsp.toml`, the nearest ancestor
wins) that provide static prelude imports and the module path. A
configuration is untrusted, so `module_path` confines what it reads to the
configuration's directory. Roots are canonicalized and contained when the
configuration loads. Each module file is canonicalized, contained in its
root's canonical path (rechecked at open), opened non-blocking, verified as a
regular file on the open handle and read up to a size limit. A check compiles
at most `MAX_MODULES` sources.

`dodo install` uses the newly built `dolang` binary to regenerate typelibs
from bundled-library sources, then sets `DOLANG_LSP_TYPELIB_DIR` for the LSP
build. The build script validates and embeds the directory's `.dolt` files.
`typelib_index::lookup` returns a module's bytes for decoding with
`dolang_compile::typeck::Typelib::read` and adding to the check builder.
Ordinary builds leave the variable unset and embed an empty index, as with
`DOLANG_LSP_DOC_JSON_DIR` for documentation, so checks know no bundled module.
`dodo lsp-test` generates both directories as `install` does and runs the
crate's tests against them; tests that need the bundle return early without
it. dolang-lsp is not a default workspace member, so `dodo cargo-test` doesn't
build it a second time without the bundle.
