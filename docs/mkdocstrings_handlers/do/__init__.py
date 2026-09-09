"""mkdocstrings handler for the Do language."""

from __future__ import annotations

import copy
import json
import os
import re
from pathlib import Path
from typing import Any

# How many parameters a signature keeps once a parameter table repeats them.
MAX_SIGNATURE_PARAMS = 2

from mkdocstrings._internal.handlers.base import BaseHandler, CollectionError


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
            cached = json.loads(cache_file.read_text())
        except json.JSONDecodeError as e:
            raise CollectionError(
                f"doc cache file '{cache_file}' is invalid JSON: {e}"
            ) from e
        self._doc_cache[module] = cached
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


def _rewrite_doc_refs(entity: dict, aliases: dict[str, str]) -> None:
    """Retarget source-module links to names re-exported by this module."""
    doc = entity.get("doc", "") or ""
    for source, target in aliases.items():
        doc = doc.replace(f"]({source})", f"]({target})")
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


def _split_type(text: str) -> tuple[str, str]:
    """Split a leading parenthesised type off a parameter description.

    Until the language carries type annotations of its own, a description may
    open with its type in parentheses. A type is written as markdown and so
    holds parentheses of its own -- ``([`Str`](../std/str.md))`` -- so the
    group is matched by depth rather than to the first ``)``.
    """
    if not text.startswith("("):
        return "", text
    depth = 0
    for index, char in enumerate(text):
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return text[1:index].strip(), text[index + 1 :].lstrip()
    # Unbalanced, so there is no group to take and the text is all description.
    return "", text


def _slug(name: str) -> str:
    """An anchor-safe form of a parameter name.

    Parameters are written with punctuation that does not belong in a fragment
    identifier -- ``:host``, ``...args``.
    """
    return re.sub(r"[^0-9A-Za-z_]+", "-", name).strip("-")


def _signature(entity: dict) -> str:
    """The form of a declaration used as its heading.

    A parameter table repeats the whole list, so a declaration that renders one
    keeps only its required positional prefix in the heading; spelling out a
    keyword-heavy declaration produces a heading too long to scan or to use as
    a table-of-contents entry.
    """
    name = entity.get("name", "")
    params = entity.get("params") or []
    if not params:
        return f"{name}()"
    written = [p.get("name", "") + ("?" if p.get("optional") else "") for p in params]
    if not any(p.get("documented") for p in params):
        return " ".join([name, *written])
    kept: list[str] = []
    for param, text in zip(params, written):
        # Stop at the first optional parameter rather than skipping past it:
        # what identifies a call is the prefix that must be written out.
        if param.get("optional") or len(kept) == MAX_SIGNATURE_PARAMS:
            break
        kept.append(text)
    if len(kept) == len(written):
        return " ".join([name, *kept])
    return " ".join([name, *kept, "…"])


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
            type_, short = _split_type(short)
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
        if entity.get("kind") in ("function", "method"):
            entity["signature"] = _signature(entity)
            # The same leading-parenthesised-type convention parameter
            # descriptions use is also written on a function/method's own
            # doc comment, informally, to give its return type. Peel it off
            # before splitting the rest into intro/sections, the same way a
            # parameter's description is split in the loop above.
            return_type, doc = _split_type((entity.get("doc", "") or "").strip())
            entity["return_type"] = return_type
            entity["doc_intro"], entity["doc_sections"] = _split_intro(doc)
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
