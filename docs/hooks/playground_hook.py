"""Adds "Open in playground" links to code fences tagged `playground`.

A fence tagged `playground` renders like an untagged Do fence, followed by a
link that opens its source in the playground. Lines starting with `#>` are
omitted from the rendered example but included in the linked source, so
examples can keep their `import` lines out of the docs.

The link carries the source in the `src` query parameter, encoded as the
playground's `decodeSource` expects: gzip, then unpadded base64url.
"""

import base64
import gzip
import html

from mkdocs.plugins import event_priority
from mkdocs.utils import get_relative_url
from pymdownx.superfences import highlight_validator

HIDDEN = "#>"

# The formatter runs without knowing which page it renders into, so it writes
# this placeholder and `on_page_content` replaces it with the page-relative
# playground URL.
BASE_PLACEHOLDER = "@@PLAYGROUND_BASE@@"


def _split(src):
    shown = []
    full = []
    for line in src.splitlines():
        if line.startswith(HIDDEN):
            full.append(line[len(HIDDEN):].removeprefix(" "))
        else:
            shown.append(line)
            full.append(line)
    return "\n".join(shown), "\n".join(full)


def _encode(source):
    # mtime=0 keeps the output identical across builds.
    data = gzip.compress(source.encode(), mtime=0)
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def _format(src, language, class_name, options, md, **kwargs):
    shown, full = _split(src)
    fences = md.preprocessors["fenced_code_block"]
    code = fences.highlight(shown, "dolang", options, md, **kwargs)
    href = html.escape(f"{BASE_PLACEHOLDER}?src={_encode(full)}")
    return (
        f'<div class="{class_name}">{code}'
        f'<a class="playground-open" href="{href}">Open in playground</a>'
        "</div>"
    )


# Runs before plugins so mkdocstrings, which renders doc comments with its own
# Markdown instance, sees the custom fence too.
@event_priority(100)
def on_config(config, **kwargs):
    superfences = config.mdx_configs.setdefault("pymdownx.superfences", {})
    superfences.setdefault("custom_fences", []).append(
        {
            "name": "playground",
            "class": "playground-example",
            "format": _format,
            "validator": highlight_validator,
        }
    )
    return config


def on_page_content(html_text, page, **kwargs):
    base = get_relative_url("playground/", page.url)
    return html_text.replace(BASE_PLACEHOLDER, base)
