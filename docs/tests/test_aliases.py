"""Alias layout and compiler round trips.

Run after building with dodo:
DOLANG_DOC_TEST_BINARY=target/x86_64-unknown-linux-gnu/debug/dolang \
    python3 -m unittest discover -s docs/tests
"""

import copy
import html
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from mkdocstrings_handlers.do import (
    DoHandler,
    _TypeScope,
    _alias_layout,
    _alias_type_lines,
    _declaration_name,
    _render_annotations,
    _render_type,
    _BINDS_COMPACT,
)

ROOT = Path(__file__).resolve().parents[2]


def name(text):
    return {"kind": "name", "name": text}


def union(count):
    return {"kind": "union", "members": [name(f"T{i}") for i in range(count)]}


def schema(count):
    return {"kind": "schema", "params": [
        {"kind": "key", "key": f"k{i}", "type": name("Int")} for i in range(count)
    ]}


def app(value, kind="pos"):
    return {"kind": "app", "base": name("Dict"), "args": [{"kind": kind, "type": value}]}


def text_content(markup):
    return html.unescape(re.sub(r"<[^>]*>", "", markup))


def normalize(value, scope=None):
    if isinstance(value, list):
        return [normalize(item, scope) for item in value]
    if isinstance(value, dict):
        result = {key: normalize(item, scope) for key, item in value.items()
                  if key not in ("target", "module", "item")}
        if scope and value.get("kind") == "name":
            result["name"] = scope.name(value)[0]
        if "key_type" in value:
            result.pop("key", None)
        return result
    return value


class LayoutTests(unittest.TestCase):
    def setUp(self):
        self.scope = _TypeScope("fixture", [])

    def test_visible_length_boundary(self):
        ty = union(2)
        length = len(_render_type(ty, self.scope, _BINDS_COMPACT, plain=True))
        for total in (89, 90, 91):
            with self.subTest(total=total):
                self.assertEqual(_alias_layout(ty, self.scope, total - length), total > 90)

    def test_structural_boundaries(self):
        self.assertFalse(_alias_layout(union(4), self.scope, 0))
        self.assertTrue(_alias_layout(union(5), self.scope, 0))
        self.assertFalse(_alias_layout(schema(3), self.scope, 0))
        self.assertTrue(_alias_layout(schema(4), self.scope, 0))
        nested = schema(2)
        nested["params"][0]["type"] = app(schema(1))
        self.assertTrue(_alias_layout(nested, self.scope, 0))

    def test_unsupported_layout_stays_compact(self):
        for ty in (name("VeryLongName" * 10), schema(0), app(schema(4), "key"),
                   {"kind": "func", "params": [], "ret": union(5)}):
            with self.subTest(ty=ty):
                self.assertFalse(_alias_layout(ty, self.scope, 100))

    def test_scope_links_and_escaping_do_not_change_layout(self):
        ty = union(5)
        ty["members"][0] = {"kind": "name", "name": "Renamed", "module": "other", "item": "Type"}
        ty["members"][1] = {"kind": "const", "text": '"<tag>&_*[x]"'}
        plain = "\n".join(_alias_type_lines(ty, self.scope, "@let A = ", 0, plain=True))
        linked = "\n".join(_alias_type_lines(ty, self.scope, "@let A = ", 0))
        self.assertEqual(text_content(linked), plain)
        self.assertIn('<autoref identifier="other.Type" optional>other.Type</autoref>', linked)
        self.assertIn('&lt;tag&gt;&amp;_*[x]', linked)
        self.assertNotIn('\\[', linked)

    def test_nested_html_and_plain_layout_agree(self):
        ty = schema(4)
        ty["params"][0]["key"] = '"<tag>"'
        ty["params"][0]["type"] = union(5)
        linked = "\n".join(_alias_type_lines(ty, self.scope, "@let A = ", 0))
        plain = "\n".join(_alias_type_lines(ty, self.scope, "@let A = ", 0, plain=True))
        self.assertEqual(text_content(linked), plain)
        self.assertIn('  "<tag>":\n    | T0', plain)

    def test_generic_prefix_counts_and_annotations_stay_compact(self):
        alias = {"kind": "alias", "name": "A" * 70, "type": union(2),
                 "binders": [{"kind": "pos", "name": "T", "bound": name("Int")}]}
        _render_annotations(alias, self.scope)
        self.assertTrue(alias["alias_vertical"])
        self.assertIn('[T @ Int]', alias["alias_declaration"])
        function = {"kind": "function", "returns": union(5),
                    "params": [{"type": schema(4)}]}
        _render_annotations(function, self.scope)
        self.assertNotIn("\n", function["return_annotation"])
        self.assertNotIn("\n", function["params"][0]["annotation"])
        opaque = {"kind": "alias", "name": "Opaque", "type": None}
        _render_annotations(opaque, self.scope)
        self.assertFalse(opaque["alias_vertical"])

    def test_reexport_uses_public_name_and_original_scope(self):
        handler = object.__new__(DoHandler)
        source_alias = {"kind": "alias", "name": "Original", "type": union(5)}
        handler._doc_cache = {"source": {"entities": [source_alias]}}
        handler._type_scopes = {"source": self.scope}
        result = handler._resolve_entity(Path("unused"), "public", {
            "kind": "import_item", "name": "Renamed", "module": "source", "item": "Original",
        })
        self.assertTrue(result["alias_declaration"].startswith("@let Renamed = $"))


class CompilerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = os.environ.get("DOLANG_DOC_TEST_BINARY") or shutil.which("dolang")
        if not cls.binary:
            raise unittest.SkipTest("Set DOLANG_DOC_TEST_BINARY to the dodo-built dolang")
        cls.binary = str(Path(cls.binary).resolve())

    def extract(self, source):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.dol"
            path.write_text(source)
            result = subprocess.run([
                self.binary, "-m", "compile", "extract", "--doc", "--all",
                "--module", "fixture", str(path),
            ], cwd=ROOT, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr + "\n" + source)
            return json.loads(result.stdout)

    def round_trip(self, source):
        original = self.extract(source)
        scope = _TypeScope("fixture", original["nodes"])
        aliases = [entity for entity in original["doc"]["entities"] if entity["kind"] == "alias"]
        rendered = []
        for entity in aliases:
            cooked = copy.deepcopy(entity)
            _render_annotations(cooked, scope)
            if cooked["alias_vertical"]:
                rendered.append(text_content(cooked["alias_declaration"]))
            else:
                rendered.append(f"@let {_declaration_name(cooked)} = " +
                                _render_type(entity["type"], scope, _BINDS_COMPACT, plain=True))
        rebuilt = self.extract("@import std\n" + "\n".join(rendered) + "\n")
        rebuilt_aliases = [entity for entity in rebuilt["doc"]["entities"] if entity["kind"] == "alias"]
        self.assertEqual([normalize(entity["type"]) for entity in aliases],
                         [normalize(entity["type"]) for entity in rebuilt_aliases])
        self.assertEqual([normalize(entity.get("binders")) for entity in aliases],
                         [normalize(entity.get("binders")) for entity in rebuilt_aliases])
        return "\n".join(rendered)

    def test_schemas_unions_and_expansion(self):
        rendered = self.round_trip('''
@let Base = {name: Str}
@let Large[T @ Int = Int] = {
  "x-custom": Str, (Str): Bool, ?port: Int, *Str, **Bool,
  *, **, ..., ...Base, *...Base, ...{Int, Str}
}
@let Applied = Dict[Int, {one: Str, two: Int, three: Bool, four: nil}]
@let Expanded = Tuple[...{Str, Int, Bool, nil}]
@let Alternatives = (Str | Int | Float | Bool | nil | Dict[{a: Int, b: Str, c: Bool, d: nil}])
@let Nested = Dict[{credentials: (Str | Int | Bool | Float | Dict[{user: Str, token: Str, scope: Str, expiry: Int}]), ?small: (Str | nil), ...Base}]
@let Positionals = {* (Str | Int | Float | Bool | nil), Dict[{a: Int, b: Str, c: Bool, d: nil}], Str, ...{Int, Str, Bool, nil}}
@let Function = ((Int, *Str, **Bool, <std.Iter[Int], >std.Sink[Str]) -> (Str | nil))
@let Keyword = Dict[schema: {a: Int, b: Str, c: Bool, d: nil}]
@let Empty = {}
''')
        self.assertIn("@let Applied = Dict[Int] $", rendered)
        self.assertIn("@let Expanded = Tuple ...$", rendered)
        self.assertIn("  credentials:\n    |", rendered)
        self.assertIn("- *|", rendered)
        self.assertIn("?small: (Str | nil)", rendered)
        self.assertIn("...$", rendered)

    def test_parenthesized_types(self):
        rendered = self.round_trip('''
@let Pair = (Int, Str)
@let Single = (Int,)
@let Empty = ()
@let Named = (name: Str, ?port: Int)
@let Grouped = Array[(Int)]
''')
        self.assertIn("@let Pair = (Int, Str)", rendered)
        self.assertIn("@let Single = (Int,)", rendered)
        self.assertIn("@let Empty = ()", rendered)
        self.assertIn("@let Named = (name: Str, ?port: Int)", rendered)
        self.assertIn("@let Grouped = Array[Int]", rendered)

    def test_array_types(self):
        rendered = self.round_trip('''
@let Ints = [Int]
@let Rows = [[Str | nil]]
''')
        self.assertIn("@let Ints = [Int]", rendered)
        self.assertIn("@let Rows = [[Str | nil]]", rendered)

    def test_source_layout_does_not_control_output(self):
        compact = self.round_trip("@let A = (Str | Int | Float | Bool | nil)\n")
        vertical = self.round_trip("@let A = $\n  | Str\n  | Int\n  | Float\n  | Bool\n  | nil\n")
        self.assertEqual(compact, vertical)
        self.assertEqual(self.round_trip("@let Small = $\n  | Str\n  | nil\n"),
                         "@let Small = (Str | nil)")

    def test_existing_extraction_fixture(self):
        fixture = json.loads((ROOT / "dolang-shell/tests/doc/fixture.json").read_text())
        scope = _TypeScope("fixture", fixture["nodes"])
        for entity in fixture["doc"]["entities"]:
            if entity["kind"] == "alias":
                with self.subTest(alias=entity["name"]):
                    ty = entity.get("type")
                    if ty:
                        plain = "\n".join(_alias_type_lines(ty, scope, "@let A = ", 0, plain=True))
                        linked = "\n".join(_alias_type_lines(ty, scope, "@let A = ", 0))
                        self.assertEqual(plain, text_content(linked))

    def test_representative_aliases_round_trip(self):
        cache = Path(self.binary).parent / "mkdocs-doc"
        if not cache.is_dir():
            self.skipTest("Run dodo mkdocs to produce the extraction cache")
        for module in ("args", "json", "progress", "security.windows", "security.unix"):
            with self.subTest(module=module):
                original = json.loads((cache / f"{module}.json").read_text())
                scope = _TypeScope(module, original["nodes"])
                aliases = [entity for entity in original["doc"]["entities"]
                           if entity["kind"] == "alias" and entity.get("type")]
                rendered = []
                modules = {"std"}

                def imported_names(value):
                    if isinstance(value, dict):
                        if value.get("kind") == "name" and "module" in value:
                            modules.add(value["module"])
                        for item in value.values():
                            imported_names(item)
                    elif isinstance(value, list):
                        for item in value:
                            imported_names(item)

                for entity in aliases:
                    imported_names(entity)
                    cooked = copy.deepcopy(entity)
                    _render_annotations(cooked, scope)
                    if cooked["alias_vertical"]:
                        rendered.append(text_content(cooked["alias_declaration"]))
                    else:
                        rendered.append(f"@let {_declaration_name(cooked)} = " +
                                        _render_type(entity["type"], scope, _BINDS_COMPACT, plain=True))
                declared = {entity["name"] for entity in aliases}
                source_bytes = (ROOT / original["source"]["path"]).read_bytes()
                placeholders = []
                for node in scope.targets.values():
                    if node["kind"] in ("Class", "Alias"):
                        span = node["name"]
                        local_name = source_bytes[span["start"]["byte_offset"]:
                                                  span["end"]["byte_offset"]].decode()
                        if local_name not in declared:
                            placeholders.append(f"@let {local_name} = ...")
                source = "\n".join([*(f"@import {m}" for m in sorted(modules)),
                                    *placeholders, *rendered]) + "\n"
                rebuilt = self.extract(source)
                rebuilt_scope = _TypeScope(module, rebuilt["nodes"])
                rebuilt_types = {entity["name"]: entity["type"] for entity in rebuilt["doc"]["entities"]
                                 if entity["kind"] == "alias" and entity.get("type")}
                for entity in aliases:
                    self.assertEqual(normalize(entity["type"], scope),
                                     normalize(rebuilt_types[entity["name"]], rebuilt_scope),
                                     entity["name"] + "\n" + source)

    def test_built_alias_html(self):
        cache = Path(self.binary).parent / "mkdocs-doc"
        if not cache.is_dir() or not (ROOT / "site/api/args/index.html").is_file():
            self.skipTest("Run dodo mkdocs to produce the rendered site")
        checked = 0
        for module in ("args", "json", "progress", "security.windows", "security.unix"):
            page = (ROOT / "site/api" / module / "index.html").read_text()
            original = json.loads((cache / f"{module}.json").read_text())
            scope = _TypeScope(module, original["nodes"])
            for entity in original["doc"]["entities"]:
                if entity["kind"] != "alias" or not entity.get("doc"):
                    continue
                cooked = copy.deepcopy(entity)
                _render_annotations(cooked, scope)
                if not cooked["alias_vertical"]:
                    continue
                with self.subTest(module=module, alias=entity["name"]):
                    identifier = f"{module}.{entity['name']}"
                    pattern = (rf'<h3 id="{re.escape(identifier)}">(.*?)</h3>\s*'
                               r'<pre class="do-alias"><code>(.*?)</code></pre>')
                    match = re.search(pattern, page, re.DOTALL)
                    self.assertIsNotNone(match)
                    self.assertEqual(text_content(match[1]), _declaration_name(cooked))
                    self.assertEqual(text_content(match[2]), text_content(cooked["alias_declaration"]))
                    self.assertNotIn("<autoref", match[2])
                    if "<autoref" in cooked["alias_declaration"]:
                        self.assertIn('<a class="autorefs autorefs-internal"', match[2])
                    self.assertIn(f'href="#{identifier}"', page)
                    self.assertEqual(page.count(f'id="{identifier}"'), 1)
                    checked += 1
        self.assertGreater(checked, 10)


if __name__ == "__main__":
    unittest.main()
