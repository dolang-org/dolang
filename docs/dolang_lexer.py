import atexit
import json
import os
import subprocess
import threading
from bisect import bisect_right
from typing import Any, Dict, Iterator, List, Optional, Tuple

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


class _CompileServer:
    """A persistent `dolang -m compile server` process.

    Shared by every `DoLexer` instance that requests the same command --
    MkDocs may construct several lexer instances over one build, and each
    renders many code fences, so one live process serves all of them instead
    of every fence separately paying `dolang`'s extension-loading startup
    cost (the whole reason this class exists; see `-m compile server`'s doc
    comment in `dolang-shell/entrypoint/compile.dol`).
    """

    _instances: Dict[Tuple[str, ...], "_CompileServer"] = {}
    _instances_lock = threading.Lock()

    def __init__(self, command: List[str]):
        self._command = command
        self._proc: Optional[subprocess.Popen] = None
        self._lock = threading.Lock()

    @classmethod
    def get(cls, command: List[str]) -> "_CompileServer":
        key = tuple(command)
        with cls._instances_lock:
            server = cls._instances.get(key)
            if server is None:
                server = cls(command)
                cls._instances[key] = server
            return server

    @classmethod
    def _shutdown_all(cls) -> None:
        with cls._instances_lock:
            servers = list(cls._instances.values())
        for server in servers:
            server._shutdown()

    def _shutdown(self) -> None:
        with self._lock:
            if self._proc is not None and self._proc.poll() is None:
                self._proc.terminate()
            self._proc = None

    def _start(self) -> subprocess.Popen:
        return subprocess.Popen(
            self._command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )

    def request(self, payload: dict) -> dict:
        """Sends one JSON request line, returns the decoded JSON response.

        The process is started lazily on first use. If it has died (crashed,
        or killed by something else entirely) it's restarted once and the
        request retried, so a single bad process doesn't wedge every
        remaining code block in the build.
        """
        line = (json.dumps(payload) + "\n").encode("utf-8")
        with self._lock:
            for attempt in range(2):
                if self._proc is None or self._proc.poll() is not None:
                    self._proc = self._start()
                try:
                    self._proc.stdin.write(line)
                    self._proc.stdin.flush()
                    response_line = self._proc.stdout.readline()
                    if not response_line:
                        raise BrokenPipeError("dolang compile server closed its output")
                    return json.loads(response_line.decode("utf-8"))
                except (BrokenPipeError, OSError):
                    self._proc = None
                    if attempt == 1:
                        raise
        raise AssertionError("unreachable")


atexit.register(_CompileServer._shutdown_all)


class DoLexer(Lexer):
    name = "Do"
    aliases = ["dolang", "dol"]
    filenames = ["*.dol"]
    mimetypes = ["text/x-dolang"]

    def __init__(self, **options):
        """
        Initialize the lexer.

        Options:
            json_file: Path to JSON file containing `dolang -m compile extract` output.
                Bypasses the compile server entirely.
            highlighter_command: The one-shot `dolang -m compile extract` command (list of
                strings). Defaults to DOLANG_HIGHLIGHT env var if set, otherwise
                "dolang -m compile extract". The persistent server command is derived from
                this by replacing a trailing "extract" with "server" (or appending "server"
                if it doesn't end in "extract").
        """
        super().__init__(**options)
        self.json_file = options.get("json_file")
        self.highlighter_command = options.get("highlighter_command")
        self._payload = None

    def get_tokens_unprocessed(self, text: str) -> Iterator[Tuple[int, Token, str]]:
        # Load the extractor payload (will execute the extractor if needed)
        payload = self._load_payload(text)

        if not payload or not text:
            # Fallback to text if no payload or source text available
            yield 0, Text, text
            return

        # Process and sort tokens by start position
        processed_tokens = self._process_tokens(payload, text)

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
        "COMMENT": 0,
        "CONSTANT": 0,
        "DELIM": 0,
        "ESCAPE": 0,
        "FIELD": 0,
        "KEY": 1,
        "MODULE_NAME": 0,
        "MODULE_ITEM": 1,
        "KEYWORD": 0,
        "LITERAL": 0,
        "NUMBER": 0,
        "OPERATOR": 0,
        "STRING_DELIM": 0,
        "VARIABLE": 0,
        "SIGIL": 0,
        # Diagnostic severities, synthesized as low-priority candidates
        "ERROR": 100,  # Low priority - overshadowed by regular tokens
        "WARNING": 100,  # Low priority - filtered out anyway, but explicit here
    }

    def _get_token_priority(self, kind: str) -> int:
        """Get priority for token kind. Lower number = higher priority."""
        return self._TOKEN_PRIORITY_TABLE.get(kind, 0)

    def _process_tokens(self, payload: dict, source_text: str) -> list:
        offset_map = self._build_offset_map(source_text)
        nodes = payload.get("nodes", [])

        # Diagnostics ride through the same span/priority pipeline as tokens,
        # as low-priority candidates: a real token at the same span always
        # wins, but an uncovered error span still surfaces (as Token.Error,
        # logged below) rather than silently vanishing.
        candidates = list(payload.get("tokens", [])) + [
            {"kind": d.get("severity"), "span": d.get("span")}
            for d in payload.get("diagnostics", [])
        ]

        # First, collect all valid tokens with their metadata
        token_candidates = []

        for token_info in candidates:
            span = token_info.get("span") or {}
            start_pos = span.get("start", {})
            end_pos = span.get("end", {})

            start_offset = start_pos.get("byte_offset", 0)
            end_offset = end_pos.get("byte_offset", start_offset)

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

            # Apply node/context modifiers for richer highlighting
            token_type = self._apply_modifiers(token_type, token_info, nodes)

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

    def _load_payload(self, text: str) -> dict:
        if self._payload is not None:
            return self._payload

        if self.json_file:
            # Load from file (existing behavior)
            with open(self.json_file, "r", encoding="utf-8") as f:
                payload = json.load(f)
        else:
            payload = self._request_from_server(text)

        self._payload = payload
        return payload

    def _base_command(self) -> List[str]:
        if self.highlighter_command:
            return list(self.highlighter_command)
        if os.environ.get("DOLANG_HIGHLIGHT"):
            return os.environ["DOLANG_HIGHLIGHT"].split()
        return ["dolang", "-m", "compile", "extract"]

    def _server_command(self) -> List[str]:
        base = self._base_command()
        if base and base[-1] == "extract":
            return base[:-1] + ["server"]
        return base + ["server"]

    def _request_from_server(self, source_text: str) -> dict:
        server = _CompileServer.get(self._server_command())
        response = server.request({"source": source_text})
        if "error" in response:
            raise RuntimeError(f"dolang compile server failed: {response['error']}")
        return response

    def _map_token_type(self, token_info: Dict[str, Any]) -> Token:
        kind = token_info.get("kind", "text")

        # Core token type mappings
        token_mapping = {
            "KEYWORD": Keyword,
            "VARIABLE": Name.Variable,
            "SIGIL": Name.Variable,
            "NUMBER": Number,
            "LITERAL": String,
            "COMMENT": Comment,
            "OPERATOR": Operator,
            "STRING_DELIM": String.Double,
            "DELIM": Punctuation,
            "CONSTANT": Name.Constant,
            "ESCAPE": String.Escape,
            "FIELD": Name.Variable,
            "KEY": Name.Property,
            "MODULE_NAME": Name.Namespace,
            "MODULE_ITEM": Name.Property,
            "ERROR": Token.Error,
        }

        base_token = token_mapping.get(kind, Text)
        return base_token

    def _apply_modifiers(
        self, base_token: Token, token_info: Dict[str, Any], nodes: List[dict]
    ) -> Token:
        ref = token_info.get("ref")
        node_kind = (
            nodes[ref]["kind"] if ref is not None and 0 <= ref < len(nodes) else None
        )
        context = token_info.get("context")

        # What the name refers to, if it refers to a declaration
        if node_kind == "Class":
            base_token = Name.Class
        elif node_kind in ("Function", "Method", "SpecialMethod"):
            base_token = Name.Function
        elif node_kind in ("PositionalParam", "KeyParam", "RestParam", "SelfParam"):
            base_token = Name.Variable.Magic
        elif node_kind == "ImportModule":
            base_token = Name.Namespace
        elif node_kind in ("PreludeItem", "PreludeModule"):
            base_token = Name.Builtin

        # Context-based modifiers
        if context == "CALL":
            base_token = Name.Function

        return base_token


__all__ = ["DoLexer"]
