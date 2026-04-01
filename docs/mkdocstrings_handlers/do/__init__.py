"""mkdocstrings handler for the Do language."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path
from typing import Any

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
    """Find the .dol source file for a module name using the same resolution logic as dolang-shell.

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

    def get_templates_dir(self, handler: str | None = None) -> Path:
        return Path(__file__).parent / "templates"

    def get_options(self, local_options: dict) -> dict:
        merged = dict(self._global_config)
        merged.update(local_options)
        return merged

    def collect(self, identifier: str, options: dict) -> dict:
        """Collect documentation for an identifier.

        ``paths`` may be a list of search directories (resolved like dolang-shell)
        or a dict mapping module names to explicit file paths — or both together.

        Identifier formats:
          - ``module``                      → entire module (all public entities)
          - ``module.sub``                  → sub-module or entity named ``sub``
          - ``module.ClassName``            → a class
          - ``module.function_name``        → a top-level function or value
          - ``module.ClassName.member``     → a class member (method or field)

        Resolution tries the longest module prefix first so that dotted module
        names (e.g. ``container.docker``) are preferred over treating the last
        component as an entity name.
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

        # Find dolang-doc binary: prefer DOLANG_DOC env var, then PATH.
        dolang_doc = os.environ.get("DOLANG_DOC") or shutil.which("dolang-doc")
        if dolang_doc is None:
            raise CollectionError(
                "'dolang-doc' binary not found. Set DOLANG_DOC env var or add it to PATH."
            )

        try:
            result = subprocess.run(
                [dolang_doc, "--module", module_name, source_path],
                capture_output=True,
                text=True,
                check=True,
            )
        except subprocess.CalledProcessError as e:
            raise CollectionError(
                f"dolang-doc failed for '{source_path}': {e.stderr}"
            ) from e

        try:
            doc_data = json.loads(result.stdout)
        except json.JSONDecodeError as e:
            raise CollectionError(
                f"dolang-doc produced invalid JSON: {e}"
            ) from e

        entities = doc_data.get("entities", [])

        show_undocumented = options.get("show_undocumented", False)
        if not show_undocumented:
            _strip_undocumented(entities)

        if not entity_parts:
            # Module-level: return all public entities as a synthetic module object.
            _annotate_entities(entities, module_name)
            return {
                "kind": "module",
                "module": module_name,
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
