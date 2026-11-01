# dolang-lsp Architecture

LSP server for Do using `tower-lsp-server` and `tokio`. Document state
recompiles on every change to provide diagnostics and semantic tokens.

Documents live in workspaces (`workspace.rs`). A workspace is a root: the
nearest directory above a document with a `.git`, directory or file, or else
the document's own directory, with its settings. A `.dolang-lsp.toml` at the
root sets them; whatever it leaves out comes from the convention, `lib/` as the
module path and the shell's prelude. A configuration below the root sets the
prelude of its subtree only, and is read the first time a document there is
compiled. Its members are the
documents in it open for any reason: in the client, or as a module a check
imports. A member is one revision of its document, an `Arc` that new text
replaces in the workspace's table; whoever holds an older one keeps it, and can
tell it is no longer current. The workspace's lock guards only reading and
changing that table: files are read, and members compiled and checked, outside
it. The backend's only state is a handle for each open document, which keeps
its workspace alive, and the table of live workspaces.

Opening, editing, saving and closing a document update the handle and the
workspace before the handler's first `await`, so they apply in the order the
client sent them. Edits can still arrive out of order, so a member the client
has open replaces another only if its version isn't older. A reopen starts
over, whatever versions came before. A save is a revision at the document's
current version.

Compilation runs on a dedicated worker thread, so handlers never block the event
loop on it. Handlers send the worker each new member, along with the sender of a
shared future of its `Document`. A request awaits the current member's future,
since its positions refer to the text the client last sent; ordering requests
after edits is left to the client. If the worker passes over a member,
superseded or failing to compile it, the request answers from the last published
`Document`. A member with the same text as the one it replaces keeps its
`Unit<'static>`, and its check if the client had it open too. The worker
projects each unit into a `Document` holding the unit (whose source is the
document's text), semantic tokens, references, declarations, symbols and quick
fixes. An applier task publishes a `Document`'s diagnostics, and keeps it on the
handle, only if its member is still the current one.

The worker also type checks each workspace once its table has settled. A
document's projection publishes its compile diagnostics at once, along with a
check its unit kept. Every change to a workspace's table, from a handler or a
file on disk, puts its check off for a short delay; the worker waits on its
request channel with `recv_timeout` until the earliest check is due. A check
is of the members whose checks aren't valid, not of the open ones: opening a
document only decides whether its check is published. Their units, compiled as
modules if they lie under the configured module path and as scripts otherwise,
are checked together with the closure of their imports, in one checker run, so
documents that share modules share a check. Each member gets the checker's
diagnostics located in it and the run's analysis. An open one's check
publishes the compile diagnostics again, merged with the checker's, if its
member is still current, and the applier attaches the analysis to that stored
document. Hover asks the analysis what the checker concluded at the cursor,
and for a variable shows its name and type above the declaration's doc
comment, without its `let` label.

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

A check looks modules up in its workspace's table. A member the client has open
as the module comes first, so importers see the client's buffer, then one loaded
from disk; of several, the one at the least path. Open documents are matched to
modules by the workspace's module path, not by URI. A buffer under a nested
root, such as a submodule, is a member of that root's workspace only, so an
enclosing workspace whose module path reaches it reads its file. A module the
table lacks is read from the module path and becomes a loaded member, unless the
client opened the module meanwhile, whose buffer stands in for it. Next is a
bundled typelib unless the configuration turns them off. An import found nowhere
is reported as not found. A check gathers the modules its members import,
directly or transitively, plus `proc` for the pipe types, as `compile check`
does, loading modules until the set stops growing. Each member stands for its
own module, so it replaces its bundled copy. A buffer of a module that another
buffer of the same module stands in for is checked on its own.

A member's check records, for each module in its closure, what it looked up to:
a member, held weakly, a bundled typelib, or nothing. The check is valid while
each of them still looks up the same. Validity is derived, so a change to the
table needs no bookkeeping beyond the change: replacing a member invalidates
whatever was checked against it, and nothing else. A check is stored only on
members still current when it finishes, and one checked against a module
replaced meanwhile is invalid on arrival and checked again. Before gathering, a
check compares each loaded member's file with the modification time and size it
was read at, and looks again for the modules checks found missing or bundled. A
change replaces, loads or drops loaded members, and puts the check off until the
table settles. A check whose gathered members were replaced meanwhile is
abandoned. First, though, a check drops the loaded members that no open
document's latest check reached, looking each module that check found up again
by name, so what replaced it stays. A revision not checked yet holds what its
earlier revision's check reached. A module only an edit imports may be dropped
and loaded again, which costs only the read. A workspace is dropped once no open
document is in it.

When the client allows it, the server registers watchers for `.dol` sources
and configuration files. A source event puts every live workspace's check off,
and the check's look at the disk finds what changed. A module held by an open
document ignores its file. A configuration event in a
directory retires the workspaces whose roots contain it, and moves their open
documents to the workspaces they find now, where they are compiled and checked
again. Wildcard roots expand only when a configuration loads, so a new
directory they match waits for the next configuration event. The convention's
`lib/` is resolved at each lookup instead, so it may appear later.

Configuration files are TOML files (`.dolang-lsp.toml`) that provide static
prelude imports and, at the root only, the module path. For a document's
prelude, the nearest configuration that sets one wins. A configuration is
untrusted, so `module_path` confines what it reads to the workspace's root.
Roots are canonicalized and contained when the configuration loads. Each module
file is canonicalized, contained in its root's canonical path (rechecked at
open), opened non-blocking, verified as a regular file on the open handle and
read up to a size limit. A check compiles at most `MAX_MODULES` sources.

The custom request `dolang/modulePath` reports a document's or directory's
root and the existing directories of its module path, so an editor can run
scripts with the module path the server checks them with.

`dodo install` uses the newly built `dolang` binary to regenerate typelibs
from bundled-library sources, checking them all together in one
`compile typelib` run, then sets `DOLANG_LSP_TYPELIB_DIR` for the LSP
build. The build script validates and embeds the directory's `.dolt` files.
`typelib_index::lookup` returns a module's bytes for decoding with
`dolang_compile::typeck::Typelib::read` and adding to the check builder.
Ordinary builds leave the variable unset and embed an empty index, as with
`DOLANG_LSP_DOC_JSON_DIR` for documentation, so checks know no bundled module.
`dodo lsp-test` generates both directories as `install` does and runs the
crate's tests against them; tests that need the bundle return early without
it. dolang-lsp is not a default workspace member, so `dodo cargo-test` doesn't
build it a second time without the bundle.
