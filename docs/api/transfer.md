# transfer

Downloads, uploads, packs, and safely extracts artifacts.

`get` accepts a local path or an HTTP(S) URL. URL responses are cached under
the application cache directory and revalidated with HTTP validators when
available. Pass `digest:` as `algorithm:hex` to verify an artifact; a cache
entry already verified against that digest can be reused without a request.

`pack` and `unpack` support TAR, gzip-compressed TAR, Zstandard-compressed TAR,
and ZIP. Formats are inferred from filenames when possible. Extraction first
validates the complete archive manifest, rejects unsafe or conflicting paths,
and writes into a staging directory before publishing the destination.

---

::: transfer
