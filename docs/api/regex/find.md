# Find

An iterator over all non-overlapping matches of a pattern in a string.
`Find` is returned by [`Regex.find()`](./regex.md#find-haystack) and
implements `Iter`, so it can be used directly in `for` loops.

Each iteration yields a [`Captures`](./captures.md) value for one match.
