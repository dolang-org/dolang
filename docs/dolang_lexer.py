import json
import os
import subprocess
from bisect import bisect_right
from typing import Any, Dict, Iterator, Tuple

from pygments.lexer import Lexer
from pygments.token import (
    Comment,
    Keyword,
    Name,
    Number,
    Operator,
    Punctuation,
    String,
    Text,
    Token,
)


class DoLexer(Lexer):
    name = "Do"
    aliases = ["dolang", "dol"]
    filenames = ["*.dol"]
    mimetypes = ["text/x-dolang"]

    def __init__(self, **options):
        """
        Initialize the lexer.

        Options:
            json_file: Path to JSON file containing dolang-highlight output
            highlighter_command: Command to run for highlighting (list of strings).
                Defaults to DOLANG_HIGHLIGHT env var if set, otherwise dolang-highlight
        """
        super().__init__(**options)
        self.json_file = options.get("json_file")
        self.highlighter_command = options.get("highlighter_command")
        self._tokens = None

    def get_tokens_unprocessed(self, text: str) -> Iterator[Tuple[int, Token, str]]:
        # Load JSON token data (will execute highlighter if needed)
        json_tokens = self._load_json_tokens(text)

        if not json_tokens or not text:
            # Fallback to text if no JSON data or source text available
            yield 0, Text, text
            return

        # Process and sort tokens by start position
        processed_tokens = self._process_tokens(json_tokens, text)

        # Generate complete token stream with gaps filled as Text
        current_pos = 0

        for start_offset, token_type, token_text in processed_tokens:
            # Fill gap before this token with Text
            if start_offset > current_pos:
                gap_text = (text)[current_pos:start_offset]
                if gap_text:
                    yield current_pos, Text, gap_text

            # Yield the actual token
            yield start_offset, token_type, token_text
            current_pos = start_offset + len(token_text)

        # Fill remaining text as Text
        if current_pos < len((text)):
            remaining_text = (text)[current_pos:]
            if remaining_text:
                yield current_pos, Text, remaining_text

    # Token kind to priority table
    # Lower number = higher priority (selected when multiple tokens at same offset)
    _TOKEN_PRIORITY_TABLE = {
        # Regular token kinds (priority 0 = highest)
        "comment": 0,
        "constant": 0,
        "delim": 0,
        "escape": 0,
        "field": 0,
        "key": 1,
        "module_name": 0,
        "module_item": 1,
        "keyword": 0,
        "literal": 0,
        "number": 0,
        "operand": 0,
        "string_delim": 0,
        "variable": 0,
        "sigil": 0,
        # Diagnostic kinds
        "error": 100,  # Low priority - overshadowed by regular tokens
        "warning": 100,  # Low priority - filtered out anyway, but explicit here
    }

    def _get_token_priority(self, kind: str) -> int:
        """Get priority for token kind. Lower number = higher priority."""
        return self._TOKEN_PRIORITY_TABLE.get(kind, 0)

    def _process_tokens(self, json_tokens: list, source_text: str) -> list:
        offset_map = self._build_offset_map(source_text)

        # First, collect all valid tokens with their metadata
        token_candidates = []

        for token_info in json_tokens:
            span = token_info.get("span", {})
            start_pos = span.get("start", {})
            end_pos = span.get("end", {})

            start_offset = start_pos.get("offset", 0)
            end_offset = end_pos.get("offset", start_offset)

            # Skip invalid spans
            if start_offset >= end_offset or start_offset < 0:
                continue

            # Skip tokens beyond source text
            if start_offset >= offset_map["byte_length"]:
                continue

            start_index = self._byte_to_char_offset(offset_map, start_offset)
            end_index = self._byte_to_char_offset(offset_map, end_offset)

            if start_index >= end_index:
                continue

            # Extract the actual text from source
            token_text = source_text[start_index:end_index]

            kind = token_info.get("kind", "text")

            # Get priority for this token kind
            priority = self._get_token_priority(kind)

            # Map Do token kind to Pygments token type
            token_type = self._map_token_type(token_info)

            # Apply origin/context modifiers for richer highlighting
            token_type = self._apply_modifiers(token_type, token_info)

            token_candidates.append((start_index, priority, token_type, token_text))

        # Sort by start offset, then by priority (lower priority number = higher priority)
        token_candidates.sort(key=lambda x: (x[0], x[1]))

        # Keep only the first (highest priority) token at each start offset
        processed = []
        seen_starts = {}

        for start_offset, priority, token_type, token_text in token_candidates:
            if start_offset in seen_starts:
                continue  # Skip - a higher priority token already exists at this position
            if token_type == Token.Error:
                print(f"WARNING: error in block\n{source_text}")
            processed.append((start_offset, token_type, token_text))
            seen_starts[start_offset] = True

        return processed

    def _build_offset_map(self, text: str) -> Dict[str, Any]:
        encoded = text.encode("utf-8")
        byte_offsets = [0]
        char_offsets = [0]
        byte_offset = 0

        for char_index, ch in enumerate(text, start=1):
            byte_offset += len(ch.encode("utf-8"))
            byte_offsets.append(byte_offset)
            char_offsets.append(char_index)

        return {
            "byte_length": len(encoded),
            "byte_offsets": byte_offsets,
            "char_offsets": char_offsets,
        }

    def _byte_to_char_offset(self, offset_map: Dict[str, Any], byte_offset: int) -> int:
        byte_offsets = offset_map["byte_offsets"]
        char_offsets = offset_map["char_offsets"]

        if byte_offset <= 0:
            return 0

        if byte_offset >= offset_map["byte_length"]:
            return char_offsets[-1]

        index = bisect_right(byte_offsets, byte_offset) - 1

        if byte_offsets[index] != byte_offset:
            raise ValueError(f"token span is not on a UTF-8 boundary: {byte_offset}")

        return char_offsets[index]

    def _load_json_tokens(self, text: str) -> list:
        if self._tokens is not None:
            return self._tokens

        if self.json_file:
            # Load from file (existing behavior)
            with open(self.json_file, "r", encoding="utf-8") as f:
                tokens = json.load(f)
        else:
            # Execute dolang-highlight with source text
            tokens = self._execute_highlighter(text)

        self._tokens = tokens
        return tokens

    def _execute_highlighter(self, source_text: str) -> list:
        if self.highlighter_command:
            command = self.highlighter_command
        elif os.environ.get("DOLANG_HIGHLIGHT"):
            command = os.environ["DOLANG_HIGHLIGHT"].split()
        else:
            command = ["dolang-highlight"]

        try:
            result = subprocess.run(
                command,
                input=source_text.encode("utf-8"),
                capture_output=True,
                check=True,
            )
            return json.loads(result.stdout.decode("utf-8"))
        except subprocess.CalledProcessError as e:
            raise RuntimeError(f"dolang-highlight failed: {e.stderr.decode('utf-8')}")

    def _map_token_type(self, token_info: Dict[str, Any]) -> Token:
        kind = token_info.get("kind", "text")

        # Core token type mappings
        token_mapping = {
            "keyword": Keyword,
            "variable": Name.Variable,
            "sigil": Name.Variable,
            "number": Number,
            "literal": String,
            "comment": Comment,
            "operand": Operator,
            "string_delim": String.Double,
            "delim": Punctuation,
            "constant": Name.Constant,
            "escape": String.Escape,
            "field": Name.Variable,
            "key": Name.Property,
            "module_name": Name.Namespace,
            "module_item": Name.Property,
            "error": Token.Error,
        }

        base_token = token_mapping.get(kind, Text)
        return base_token

    def _apply_modifiers(self, base_token: Token, token_info: Dict[str, Any]) -> Token:
        origin = token_info.get("origin")
        context = token_info.get("context")

        # Origin-based modifiers
        if origin == "class":
            base_token = Name.Class
        elif origin == "def":
            base_token = Name.Function
        elif origin == "param":
            base_token = Name.Variable.Magic
        elif origin == "import_module":
            base_token = Name.Namespace
        elif origin in ("prelude_item", "prelude_module"):
            base_token = Name.Builtin

        # Context-based modifiers
        if context == "call":
            base_token = Name.Function

        return base_token


__all__ = ["DoLexer"]
