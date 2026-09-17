"""Escapes the link titles mkdocs-autorefs builds from stripped headings.

With `strip_title_tags` in effect (its default under Material), autorefs strips
the tags from a heading and writes the remaining text into a link's `title`
attribute without escaping it. A heading whose text contains `"`, such as a type
alias over string constants, then ends the attribute early and the rest of the
title spills into the page.

The stripper is a module global that autorefs looks up on each use, so replacing
it with one that escapes its output fixes every link.
"""

import html

from mkdocs_autorefs._internal import references


class _EscapingTagStripper(references._HTMLTagStripper):
    def strip(self, html_text: str) -> str:
        return html.escape(super().strip(html_text))


def on_startup(**kwargs):
    references._html_tag_stripper = _EscapingTagStripper()
