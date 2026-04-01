# dolang-ext-url Architecture

URL parsing and manipulation for Do via the `url` crate. A `Global` singleton
holds VM registrations for the `url` module types (`Url`, query iterators, and
segment iterators). The crate also exposes a small unstable helper surface for
other extensions that need to convert between Do `url.Url` values and owned
`url::Url` instances.
