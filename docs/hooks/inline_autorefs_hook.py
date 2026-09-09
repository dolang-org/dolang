"""Recognizes standard CommonMark inline links `[text](identifier)` whose
target isn't a URL, relative/absolute path, or in-page anchor, and treats
them as mkdocs-autorefs identifiers instead of literal hrefs.

mkdocs-autorefs itself only recognizes `[text][identifier]` reference-style
syntax -- its `AutorefsInlineProcessor` subclasses Python-Markdown's
`ReferenceInlineProcessor`, which requires a `[identifier]: url` definition
to parse as a link at all; without one, CommonMark renders it as literal
brackets, not a link. Editors and tools that check Markdown against the
CommonMark rendering (not mkdocs-autorefs' own later HTML rewrite) see that
as broken syntax rather than an unresolved reference.

Standard inline link syntax renders identically either way, so switching to
it sidesteps the problem without anything in mkdocs-autorefs needing to
change: this module produces the exact `<autoref identifier="...">` element
its own inline processor produces, and mkdocs-autorefs' existing HTML
post-processing pass (which resolves that element against the registered
identifier map) does the rest unmodified.

This lives under docs/hooks/ (loaded by file path via mkdocs.yml's `hooks:`
key) rather than as a plain `markdown_extensions:` entry, because that
config option eagerly imports every extension by name while the config
itself is still loading -- well before docs/'s own modules are ever put on
sys.path (see lexer_hook.py's on_startup for the mechanism that does that,
which fires too late for this). Registering the extension object directly
in on_config sidesteps needing this module to be importable by name at all.
"""

import re
from xml.etree.ElementTree import Element

from markdown.extensions import Extension
from markdown.inlinepatterns import LINK_RE, LinkInlineProcessor

_SCHEME_RE = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")


def _looks_like_identifier(target):
    """True if `target` looks like a bare mkdocs-autorefs identifier rather
    than a URL, relative/absolute path, or in-page anchor.

    Every real link in this project's docs is written with a leading `.`,
    `/`, or `#`, an explicit URL scheme, or a `.md`/`.html` suffix -- an
    identifier is whatever's left.
    """
    if not target:
        return False
    if target[0] in "./#":
        return False
    if _SCHEME_RE.match(target):
        return False
    # Strip a trailing in-page anchor before checking the extension: a real
    # link to another page's heading (`commands.md#literal-strings`) ends
    # with the anchor, not `.md`/`.html`.
    path = target.split("#", 1)[0]
    if path.endswith((".md", ".html")):
        return False
    return True


class InlineAutorefsLinkProcessor(LinkInlineProcessor):
    """Reinterprets `[text](identifier)` as an autoref when the target
    doesn't look link-shaped, deferring to the normal `<a href>` otherwise.
    """

    def handleMatch(self, m, data):
        el, start, end = super().handleMatch(m, data)
        if el is None or el.tag != "a" or not _looks_like_identifier(el.get("href", "")):
            return el, start, end
        autoref = Element("autoref")
        autoref.set("identifier", el.get("href"))
        autoref.text = el.text
        return autoref, start, end


class InlineAutorefsExtension(Extension):
    def extendMarkdown(self, md):
        # One above the built-in 'link' processor (160) so ours is tried
        # first for the same `[...](...)` syntax; it always produces a valid
        # element (falling back to the same `<a>` LinkInlineProcessor would
        # produce), so 'link' never actually runs for text matching this
        # regex -- it's just left registered and unreachable, not replaced.
        md.inlinePatterns.register(InlineAutorefsLinkProcessor(LINK_RE, md), "inline-autorefs", 161)


def on_config(config, **kwargs):
    config.markdown_extensions.append(InlineAutorefsExtension())
    return config
