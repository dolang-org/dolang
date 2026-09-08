"""mkdocstrings handler for the Do language."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
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


def _resolve_module(module_name: str, search_dirs: list[str]) -> str | None:
    """Find the .dol source file for a module name using the same resolution logic as dolang.

    For a module name like ``foo.bar``, tries in each search directory:
      1. ``<dir>/foo/bar.dol``
      2. ``<dir>/foo/bar/mod.dol``

    Returns the path as a string, or None if not found.
    """
    parts = module_name.split(".")
    rel_file = Path(*parts).with_suffix(".dol")
    rel_mod = Path(*parts, "mod.dol")
    for base in search_dirs:
        base_path = Path(base)
        for candidate in (base_path / rel_file, base_path / rel_mod):
            if candidate.is_file():
                return str(candidate)
    return None


class DoHandler(BaseHandler):
    name = "do"
    domain = "do"
    fallback_theme = "material"

    def __init__(self, *, handler_config: dict, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._global_config = handler_config.get("options", {})
        self._aliases: dict[str, str] = {}

    def get_aliases(self, identifier: str) -> tuple[str, ...]:
        """Expose qualified Do names without requiring them in HTML anchors."""
        alias = self._aliases.get(identifier)
        return (alias,) if alias is not None else ()

    def get_templates_dir(self, handler: str | None = None) -> Path:
        return Path(__file__).parent / "templates"

    def get_options(self, local_options: dict) -> dict:
        merged = dict(self._global_config)
        merged.update(local_options)
        return merged

    def collect(self, identifier: str, options: dict) -> dict:
        """Collect documentation for an identifier.

        ``paths`` may be a list of search directories (resolved like dolang)
        or a dict mapping module names to explicit file paths — or both together.

        Identifier formats:
          - ``module``                      → entire module (all public entities)
          - ``module.sub``                  → sub-module or entity named ``sub``
          - ``module.ClassName``            → a class
          - ``module.function_name``        → a top-level function or value
          - ``module.ClassName.member``     → a class member (method or field)

        Resolution tries the longest module prefix first so that dotted module
        names (e.g. ``_container.dockman``) are preferred over treating the
        last component as an entity name.
        """
        paths_opt = options.get("paths", [])

        # Normalise: paths can be a list of search dirs, a dict of explicit
        # mappings, or a mixed list containing both strings and dicts.
        search_dirs: list[str] = []
        explicit: dict[str, str] = {}
        if isinstance(paths_opt, dict):
            explicit = paths_opt
        elif isinstance(paths_opt, list):
            for item in paths_opt:
                if isinstance(item, str):
                    search_dirs.append(item)
                elif isinstance(item, dict):
                    explicit.update(item)
        elif isinstance(paths_opt, str):
            search_dirs.append(paths_opt)

        # Find the longest dotted prefix of the identifier that names a module.
        parts = identifier.split(".")
        source_path = None
        module_name = None
        entity_parts: list[str] = []

        for split in range(len(parts), 0, -1):
            candidate = ".".join(parts[:split])
            # Check explicit mapping first, then search dirs.
            if candidate in explicit:
                source_path = explicit[candidate]
                module_name = candidate
                entity_parts = parts[split:]
                break
            found = _resolve_module(candidate, search_dirs)
            if found is not None:
                source_path = found
                module_name = candidate
                entity_parts = parts[split:]
                break

        if source_path is None:
            raise CollectionError(
                f"Could not resolve module for identifier '{identifier}'. "
                f"Check the 'paths' option in the handler configuration."
            )

        # Find the extractor: prefer the DOLANG_DOC env var, then the `doc`
        # entrypoint of a `dolang` on PATH.
        dolang_doc = os.environ.get("DOLANG_DOC")
        dolang_doc_cmd = dolang_doc.split() if dolang_doc else None
        if dolang_doc_cmd is None:
            found = shutil.which("dolang")
            dolang_doc_cmd = [found, "-m", "doc"] if found else None
        if dolang_doc_cmd is None:
            raise CollectionError(
                "'dolang' not found. Set DOLANG_DOC env var or add it to PATH."
            )

        try:
            result = subprocess.run(
                [*dolang_doc_cmd, "--module", module_name, source_path],
                capture_output=True,
                text=True,
                check=True,
            )
        except subprocess.CalledProcessError as e:
            raise CollectionError(
                f"documentation extraction failed for '{source_path}': {e.stderr}"
            ) from e

        try:
            doc_data = json.loads(result.stdout)
        except json.JSONDecodeError as e:
            raise CollectionError(
                f"documentation extraction produced invalid JSON: {e}"
            ) from e

        entities = doc_data.get("entities", [])

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
                "doc": doc_data.get("doc", ""),
                "entities": entities,
                "_identifier": identifier,
                "_module": module_name,
            }

        entity = _find_entity(entities, entity_parts)
        if entity is None:
            raise CollectionError(
                f"Entity '{'.'.join(entity_parts)}' not found in '{source_path}'"
            )

        entity["_identifier"] = identifier
        entity["_module"] = module_name
        for member in entity.get("members", []):
            member["_identifier"] = f"{identifier}.{member['name']}"
            member["_module"] = module_name
        return entity

    def render(self, data: dict, options: dict, *, locale: str | None = None) -> str:
        kind = data.get("kind", "function")
        self._aliases = {}
        if kind == "module":
            for entity in data.get("entities", []):
                self._aliases[entity.get("name", "")] = entity.get("_identifier", "")
        else:
            self._aliases[data.get("name", "")] = data.get("_identifier", "")
            for member in data.get("members", []):
                self._aliases[member.get("name", "")] = member.get("_identifier", "")
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
        # Punctuation is what distinguishes `:args` from `...args`, and it is
        # exactly what a slug drops, so collisions are broken by position.
        seen: set[str] = set()
        for index, param in enumerate(entity.get("params") or []):
            short, rest = _split_doc(param.get("doc", ""))
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
            entity["doc_intro"], entity["doc_sections"] = _split_intro(
                entity.get("doc", "")
            )
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
