# transfer

Downloads, uploads, packs, and safely extracts artifacts.

A URL source is resolved through `get` wherever one is accepted, so its cache
serves `put`, `pack`, and `unpack` as well.

Results are published atomically: output is staged and renamed into place, so
an interrupted operation leaves no partial destination behind.

---

::: transfer
