"""mkdocstrings handler for the Do language."""

from __future__ import annotations

import copy
import html
import json
import os
import re
from pathlib import Path
from typing import Any

# How many parameters a signature keeps once a parameter table repeats them.
MAX_SIGNATURE_PARAMS = 3

from mkdocstrings._internal.handlers.base import BaseHandler, CollectionError

# Characters Markdown reads as syntax, escaped in the text of a type
_MARKDOWN_SPECIAL = re.compile(r"([\\`*_\[\]])")


def get_handler(
    *,
    theme: str,
    custom_templates: str | None,
    mdx: Any,
    mdx_config: Any,
    handler_config: dict,
    tool_config: Any,
) -> "DoHandler":
    return DoHandler(
        theme=theme,
        custom_templates=custom_templates,
        mdx=mdx,
        mdx_config=mdx_config,
        handler_config=handler_config,
    )


class DoHandler(BaseHandler):
    name = "do"
    domain = "do"
    fallback_theme = "material"

    def __init__(self, *, handler_config: dict, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._global_config = handler_config.get("options", {})
        # Keyed by module name: raw (unannotated) doc_data read from the
        # pre-extracted JSON cache. Every entity/member under a module is
        # documented via its own `::: module.Foo.bar` directive, each
        # triggering a separate `collect()` call -- without this cache, every
        # one of them would re-read and re-parse the same cache file.
        self._doc_cache: dict[str, dict] = {}
        # Keyed by module name, like `_doc_cache`: what the names in the module's
        # types refer to, from the raw nodes alongside its documentation.
        self._type_scopes: dict[str, _TypeScope] = {}

    def get_templates_dir(self, handler: str | None = None) -> Path:
        return Path(__file__).parent / "templates"

    def get_options(self, local_options: dict) -> dict:
        merged = dict(self._global_config)
        merged.update(local_options)
        return merged

    def _load_module(self, cache_dir: Path, module: str) -> dict:
        cached = self._doc_cache.get(module)
        if cached is not None:
            return cached
        cache_file = cache_dir / f"{module}.json"
        if not cache_file.is_file():
            raise CollectionError(f"Doc reference names missing module '{module}'.")
        try:
            # `dolang -m compile extract --doc` nests the cooked documentation
            # projection under "doc" alongside the raw nodes/tokens/diagnostics
            # dump. Types in the former refer to the raw nodes by index.
            extracted = json.loads(cache_file.read_text())
        except json.JSONDecodeError as e:
            raise CollectionError(
                f"doc cache file '{cache_file}' is invalid JSON: {e}"
            ) from e
        cached = extracted["doc"]
        self._doc_cache[module] = cached
        self._type_scopes[module] = _TypeScope(module, extracted.get("nodes", []))
        return cached

    def _resolve_entity(
        self,
        cache_dir: Path,
        module: str,
        entity: dict,
        chain: tuple[tuple[str, str], ...] = (),
    ) -> dict:
        kind = entity.get("kind")
        if kind == "import_module":
            result = copy.deepcopy(entity)
            result["kind"] = "value"
            return result
        if kind != "import_item":
            result = copy.deepcopy(entity)
            result["_doc_source"] = f"{module}.{entity.get('name', '')}"
            # Types are rendered in the module declaring them, which a
            # re-export does not change
            self._load_module(cache_dir, module)
            _render_annotations(result, self._type_scopes[module])
            return result

        key = (module, entity.get("name", ""))
        if key in chain:
            path = " -> ".join(f"{m}.{n}" for m, n in (*chain, key))
            raise CollectionError(f"Cyclic public doc re-export: {path}")
        source_module = entity.get("module", "")
        source_item = entity.get("item", "")
        source = self._load_module(cache_dir, source_module)
        target = _find_entity(source.get("entities", []), [source_item])
        if target is None:
            path = " -> ".join(
                [
                    *(f"{m}.{n}" for m, n in chain),
                    f"{module}.{key[1]}",
                    f"{source_module}.{source_item}",
                ]
            )
            raise CollectionError(f"Unresolved public doc re-export: {path}")
        result = self._resolve_entity(cache_dir, source_module, target, (*chain, key))
        result["name"] = entity.get("name", source_item)
        result["pub"] = entity.get("pub", True)
        return result

    def _resolve_entities(
        self, cache_dir: Path, module: str, entities: list[dict]
    ) -> list[dict]:
        resolved = [
            self._resolve_entity(cache_dir, module, entity) for entity in entities
        ]
        aliases = {
            result["_doc_source"]: f"{module}.{source.get('name')}"
            for source, result in zip(entities, resolved)
            if source.get("kind") == "import_item" and "_doc_source" in result
        }
        for entity in resolved:
            _rewrite_doc_refs(entity, aliases)
        return resolved

    def collect(self, identifier: str, options: dict) -> dict:
        """Collect documentation for an identifier.

        Identifier formats:
          - ``module``                      → entire module (all public entities)
          - ``module.sub``                  → sub-module or entity named ``sub``
          - ``module.ClassName``            → a class
          - ``module.function_name``        → a top-level function or value
          - ``module.ClassName.member``     → a class member (method or field)

        Resolution tries the longest module prefix first so that dotted module
        names (e.g. ``_container.dockman``) are preferred over treating the
        last component as an entity name. A prefix is a module if a file for
        it exists in the pre-extracted JSON cache (``DOLANG_DOC_CACHE``) --
        one file per module, named ``<module name>.json``, produced ahead of
        the mkdocs build by the ``dodo mkdocs`` extraction step (see
        ``extract_docs`` in ``dodo.dol``). This handler never runs the
        ``dolang`` extractor itself.
        """
        cache_dir = os.environ.get("DOLANG_DOC_CACHE")
        if not cache_dir:
            raise CollectionError(
                "DOLANG_DOC_CACHE is not set. It must point at the "
                "pre-extracted doc JSON directory produced by the "
                "`dodo mkdocs` build (see extract_docs/with_mkdocs in dodo.dol)."
            )

        # Find the longest dotted prefix of the identifier that names a module.
        parts = identifier.split(".")
        module_name = None
        entity_parts: list[str] = []
        cached = None

        for split in range(len(parts), 0, -1):
            candidate = ".".join(parts[:split])
            cached = self._doc_cache.get(candidate)
            if cached is None:
                cache_file = Path(cache_dir) / f"{candidate}.json"
                if cache_file.is_file():
                    cached = self._load_module(Path(cache_dir), candidate)
            if cached is not None:
                module_name = candidate
                entity_parts = parts[split:]
                break

        if cached is None:
            raise CollectionError(
                f"Could not resolve module for identifier '{identifier}' in "
                f"doc cache '{cache_dir}'."
            )

        doc_data = copy.deepcopy(cached)
        entities = self._resolve_entities(Path(cache_dir), module_name, doc_data.get("entities", []))
        _sort_entities(entities)

        show_undocumented = options.get("show_undocumented", False)
        if not show_undocumented:
            _strip_undocumented(entities)

        _annotate_params(entities)

        if not entity_parts:
            # Module-level: return all public entities as a synthetic module object.
            _annotate_entities(entities, module_name)
            return {
                "kind": "module",
                "module": module_name,
                "doc": doc_data.get("doc") or "",
                "entities": entities,
                "_identifier": identifier,
                "_module": module_name,
            }

        entity = _find_entity(entities, entity_parts)
        if entity is None:
            raise CollectionError(
                f"Entity '{'.'.join(entity_parts)}' not found in module '{module_name}'"
            )

        entity["_identifier"] = identifier
        entity["_module"] = module_name
        for member in entity.get("members", []):
            member["_identifier"] = f"{identifier}.{member['name']}"
            member["_module"] = module_name
        _assign_anchors(entity.get("members", []))
        entity["_anchor"] = identifier
        return entity

    def render(self, data: dict, options: dict, *, locale: str | None = None) -> str:
        kind = data.get("kind", "function")
        template = self.env.get_template(f"{kind}.html.jinja2")
        if kind == "module":
            return template.render(
                entity=data,
                entities=data.get("entities", []),
                options=options,
            )
        return template.render(entity=data, options=options)


def _annotate_entities(entities: list[dict], module_name: str) -> None:
    """Recursively set ``_identifier`` and ``_module`` on every entity and member."""
    for entity in entities:
        name = entity.get("name", "")
        entity["_identifier"] = f"{module_name}.{name}"
        entity["_module"] = module_name
        for member in entity.get("members", []):
            member["_identifier"] = f"{module_name}.{name}.{member['name']}"
            member["_module"] = module_name
        _assign_anchors(entity.get("members", []))
    _assign_anchors(entities)


def _assign_anchors(entities: list[dict]) -> None:
    """Set ``_anchor``, the heading id, on each entity from its ``_identifier``.

    Overloads share a name, and so an identifier, which links to the first of
    them; sorting keeps them together in source order. Each later one gets an
    anchor of its own so that no heading id is repeated.
    """
    counts: dict[str, int] = {}
    for entity in entities:
        identifier = entity["_identifier"]
        count = counts.get(identifier, 0) + 1
        counts[identifier] = count
        entity["_anchor"] = identifier if count == 1 else f"{identifier}--overload-{count}"


def _sort_entities(entities: list[dict]) -> None:
    """Alphabetize entities and their members in place.

    Declaration order in the source has no documentation value and produces
    pages whose ordering varies file to file; every list a template iterates
    -- a module's types/functions/values, a class's fields/methods -- is
    built by filtering this list while preserving relative order, so sorting
    it once here alphabetizes each of those downstream lists too.
    """
    entities.sort(key=lambda e: (e.get("name") or "").lower())
    for entity in entities:
        members = entity.get("members")
        if members:
            _sort_entities(members)


def _rewrite_doc_refs(entity: dict, aliases: dict[str, str]) -> None:
    """Retarget source-module links to names re-exported by this module."""
    doc = entity.get("doc", "") or ""
    for source, target in aliases.items():
        doc = doc.replace(f"]({source})", f"]({target})")
        # A member of a re-exported class moves with it
        doc = doc.replace(f"]({source}.", f"]({target}.")
    entity["doc"] = doc
    for member in entity.get("members", []):
        _rewrite_doc_refs(member, aliases)


def _split_doc(doc: str) -> tuple[str, str]:
    """Split a doc comment into its first paragraph and whatever follows it.

    The first paragraph is what a parameter table can hold; anything past it
    needs room of its own.
    """
    text = (doc or "").strip()
    if not text:
        return "", ""
    head, _, rest = text.partition("\n\n")
    return head.strip(), rest.strip()


def _split_intro(doc: str) -> tuple[str, str]:
    """Split prose before the first Markdown section from those sections."""
    text = (doc or "").strip()
    fence = None
    for match in re.finditer(r"(?m)^.*(?:\n|$)", text):
        line = match.group().rstrip("\r\n")
        marker = re.match(r"^\s*(`{3,}|~{3,})", line)
        if marker:
            run = marker.group(1)
            if fence is None:
                fence = run[0]
            elif run[0] == fence:
                fence = None
        elif fence is None and re.match(r"^#{1,6}(?:\s+|$)", line):
            return text[: match.start()].rstrip(), text[match.start() :].strip()
    return text, ""


class _TypeScope:
    """What the names in one module's types refer to.

    A name in a type gives the index of the raw node it refers to. Only the
    nodes that decide how a name renders are kept: the prelude bindings, and
    the classes the module declares at top level.
    """

    def __init__(self, module: str, nodes: list[dict]) -> None:
        self.module = module
        root = next(
            (index for index, node in enumerate(nodes) if node.get("kind") == "Root"),
            None,
        )
        self.targets = {
            index: node
            for index, node in enumerate(nodes)
            if node.get("kind") in ("PreludeItem", "PreludeModule")
            or (node.get("kind") in ("Class", "Alias") and node.get("parent") == root)
        }

    def name(self, ty: dict) -> tuple[str, str | None]:
        """How to show a possibly dotted name, and what to link it to, if anything.

        A name the module has in scope without importing it -- a prelude binding
        or its own class -- is shown as written. An imported name is shown with
        its module, since the page it appears on does not show the import.
        """
        name = ty.get("name", "")
        head, dot, fields = name.partition(".")
        rest = dot + fields
        node = self.targets.get(ty.get("target"))
        kind = node.get("kind") if node else None
        if kind == "PreludeItem":
            return name, f"{node['module']}.{node['item']}{rest}"
        if kind == "PreludeModule":
            return name, f"{node['module']}{rest}"
        if kind in ("Class", "Alias"):
            return name, f"{self.module}.{name}"
        if "item" in ty:
            path = f"{ty['module']}.{ty['item']}{rest}"
            return path, path
        if "module" in ty:
            module = ty["module"]
            # `import time` binds the first name of the path, which the type
            # then spells out in full
            path = name if name.startswith(f"{module}.") else f"{module}{rest}"
            return path, path
        # A binder, or a declaration with no page of its own
        return name, None


# How tightly each type form binds, so that it is parenthesized where needed
_BINDS_FUNC, _BINDS_UNION, _BINDS_COMPACT = range(3)


def _escape_type_text(text: str) -> str:
    """Escape text for HTML that Markdown will still read.

    Quotes are escaped too: a string constant can reach a heading, whose text
    autorefs copies into the `title` attribute of links to it.
    """
    return _MARKDOWN_SPECIAL.sub(r"\\\1", html.escape(text))


def _render_type(ty: dict, scope: _TypeScope, context: int, plain: bool = False) -> str:
    """Render a type tree in a position binding as tightly as `context`.

    A `plain` rendering is bare text with no links, for a template to escape.
    """
    escape = (lambda text: text) if plain else _escape_type_text
    kind = ty.get("kind")
    if kind == "name":
        name, link = scope.name(ty)
        text = escape(name)
        if link and not plain:
            return f'<autoref identifier="{html.escape(link)}" optional>{text}</autoref>'
        return text
    if kind == "const":
        return escape(ty.get("text", ""))
    if kind == "app":
        base = _render_type(ty["base"], scope, _BINDS_COMPACT, plain)
        args = _render_type_args(ty["args"], scope, plain)
        rendered = f"{base}[{args}]" if plain else f"{base}\\[{args}\\]"
        binding = _BINDS_COMPACT
    elif kind == "schema":
        rendered = f"{{{_render_type_args(ty['args'], scope, plain)}}}"
        binding = _BINDS_COMPACT
    elif kind == "union":
        rendered = " | ".join(
            _render_type(member, scope, _BINDS_COMPACT, plain) for member in ty["members"]
        )
        binding = _BINDS_UNION
    elif kind == "func":
        params = _render_type_args(ty["params"], scope, plain)
        ret = _render_type(ty["ret"], scope, _BINDS_FUNC, plain)
        arrow = "->" if plain else "-&gt;"
        rendered = f"({params}) {arrow} {ret}"
        binding = _BINDS_FUNC
    else:
        return ""
    return f"({rendered})" if binding < context else rendered


def _render_type_args(args: list[dict], scope: _TypeScope, plain: bool = False) -> str:
    rendered = []
    for arg in args:
        text = "?" if arg.get("optional") else ""
        if arg.get("kind") == "rest":
            text += "..."
        elif arg.get("kind") == "open_rest":
            rendered.append(text + "...")
            continue
        elif arg.get("kind") == "key_rest":
            key = _render_type(arg["key_type"], scope, _BINDS_FUNC, plain)
            text += f"...{key}: "
        elif arg.get("kind") == "key" and "key_type" in arg:
            # A name must be parenthesized to not be taken as a symbol key
            key_type = arg["key_type"]
            key = _render_type(key_type, scope, _BINDS_COMPACT, plain)
            text += f"({key}): " if key_type.get("kind") == "name" else f"{key}: "
        elif arg.get("kind") == "key":
            key = arg.get("key", "")
            text += f"{key if plain else _escape_type_text(key)}: "
        rendered.append(text + _render_type(arg["type"], scope, _BINDS_FUNC, plain))
    return ", ".join(rendered)


def _binder_text(binder: dict, scope: _TypeScope) -> str:
    """A binder as its declaration writes it, as plain text."""
    text = binder.get("name", "")
    if binder.get("bound"):
        text += " @ " + _render_type(binder["bound"], scope, _BINDS_COMPACT, plain=True)
    if binder.get("default"):
        text += " = " + _render_type(binder["default"], scope, _BINDS_COMPACT, plain=True)
    return text


def _type_html(ty: dict | None, scope: _TypeScope) -> str:
    """Render an annotation, linking the names documented elsewhere.

    The result passes through Markdown, which leaves the tags alone but still
    reads the text between them, so that text is escaped for both. Links are
    optional: a name with no documentation renders as plain text. Like an
    annotation in source, it is a compact type, so a union or function type is
    parenthesized.
    """
    if not ty:
        return ""
    return f"<code>{_render_type(ty, scope, _BINDS_COMPACT)}</code>"


def _render_annotations(entity: dict, scope: _TypeScope) -> None:
    """Render the annotations of an entity and its members, in its module's scope."""
    entity["binder_text"] = [
        _binder_text(binder, scope) for binder in entity.get("binders") or []
    ]
    for param in entity.get("params") or []:
        param["annotation"] = _type_html(param.get("type"), scope)
        if param.get("type_spread") and param["annotation"]:
            param["annotation"] = param["annotation"].replace("<code>", "<code>...", 1)
    if entity.get("kind") in ("function", "method"):
        entity["return_annotation"] = _type_html(entity.get("returns"), scope)
        # A method may narrow its receiver, as `self @ Iter[U]`
        entity["self_annotation"] = _type_html(entity.get("self_type"), scope)
    elif entity.get("kind") == "field":
        entity["annotation"] = _type_html(entity.get("type"), scope)
    elif entity.get("kind") == "alias":
        entity["annotation"] = _type_html(entity.get("type"), scope)
    for member in entity.get("members", []):
        _render_annotations(member, scope)


def _slug(name: str) -> str:
    """An anchor-safe form of a parameter name.

    Parameters are written with punctuation that does not belong in a fragment
    identifier -- ``:host``, ``...args``.
    """
    return re.sub(r"[^0-9A-Za-z_]+", "-", name).strip("-")


def _signature(entity: dict) -> str:
    """The form of a declaration used as its heading.

    A parameter table repeats the whole list, so a declaration that renders one
    keeps only its first few required parameters in the heading; spelling out
    a keyword-heavy declaration produces a heading too long to scan or to use
    as a table-of-contents entry.
    """
    name = _declaration_name(entity)
    params = entity.get("params") or []
    if not params:
        return f"{name}()"
    written = [p.get("name", "") + ("?" if p.get("optional") else "") for p in params]
    if not any(p.get("documented") for p in params):
        return " ".join([name, *written])
    kept: list[str] = []
    for param, text in zip(params, written):
        if len(kept) == MAX_SIGNATURE_PARAMS:
            break
        if not param.get("optional"):
            kept.append(text)
    if len(kept) == len(written):
        return " ".join([name, *kept])
    return " ".join([name, *kept, "…"])


def _declaration_name(entity: dict) -> str:
    """A declaration's name followed by its type binders, as plain text.

    A protocol's name keeps the `@` it is declared with.
    """
    name = entity.get("name", "")
    if entity.get("protocol"):
        name = f"@{name}"
    binders = entity.get("binder_text") or []
    return f"{name}[{', '.join(binders)}]" if binders else name


def _annotate_params(entities: list[dict]) -> None:
    """Recursively prepare parameters and signatures for rendering."""
    for entity in entities:
        # The extraction format distinguishes an absent comment (`null`) from
        # a present string. Templates operate on Markdown text, so normalize
        # that absence at the rendering boundary.
        entity["doc"] = entity.get("doc") or ""
        # Punctuation is what distinguishes `:args` from `...args`, and it is
        # exactly what a slug drops, so collisions are broken by position.
        seen: set[str] = set()
        for index, param in enumerate(entity.get("params") or []):
            param["doc"] = param.get("doc") or ""
            short, rest = _split_doc(param["doc"])
            type_ = param.get("annotation", "")
            param["type"] = type_
            param["doc_short"] = short
            param["doc_rest"] = rest
            # A type alone documents a parameter, so it is enough to earn the
            # table -- and the abbreviated signature that comes with it.
            param["documented"] = bool(type_ or short or rest)
            slug = _slug(param.get("name", "")) or f"param{index}"
            if slug in seen:
                slug = f"{slug}-{index}"
            seen.add(slug)
            param["slug"] = slug
        # What a one-line table cell can hold, wherever an entity is listed
        # rather than rendered: the same first paragraph a parameter table takes.
        entity["doc_summary"], _ = _split_doc(entity.get("doc", ""))
        entity["declaration_name"] = _declaration_name(entity)
        if entity.get("kind") in ("function", "method"):
            entity["signature"] = _signature(entity)
            entity["return_type"] = entity.get("return_annotation", "")
            entity["doc_intro"], entity["doc_sections"] = _split_intro(entity["doc"])
        elif entity.get("kind") == "field":
            entity["type"] = entity.get("annotation", "")
            entity["doc"] = entity["doc"].strip()
        _annotate_params(entity.get("members", []))


def _strip_undocumented(entities: list[dict]) -> None:
    """Remove undocumented members from classes and undocumented top-level entities."""
    for entity in entities:
        if "members" in entity:
            entity["members"] = [
                m for m in entity["members"] if m.get("doc")
            ]
    entities[:] = [e for e in entities if e.get("doc") or e.get("members")]


def _find_entity(entities: list[dict], parts: list[str]) -> dict | None:
    """Find an entity by name parts, e.g. ``['MyClass']`` or ``['MyClass', 'method']``."""
    if not parts:
        return None
    top_name = parts[0]
    for entity in entities:
        if entity.get("name") == top_name:
            if len(parts) == 1:
                return entity
            return _find_entity(entity.get("members", []), parts[1:])
    return None
