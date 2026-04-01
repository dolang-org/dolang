import sys
from pathlib import Path

from pygments.lexers._mapping import LEXERS


def on_startup(**kwargs):
    # Add the docs directory to sys.path so dolang_lexer can be imported
    docs_dir = str(Path(__file__).resolve().parent.parent)
    if docs_dir not in sys.path:
        sys.path.insert(0, docs_dir)

    # Register the Do lexer with Pygments
    LEXERS["DoLexer"] = (
        "dolang_lexer",  # module name
        "Do",            # display name
        ("dolang", "dol"),  # aliases
        ("*.dol",),      # filenames
        ("text/x-dolang",),  # mimetypes
    )
