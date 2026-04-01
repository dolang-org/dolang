# xml

XML parsing and serialization.

## Functions

### `from_str xml`

Parses an XML string and returns the root element as an [`Node`](./node.md).

**Parameters:**

| Name  | Type  | Description         |
| ----- | ----- | ------------------- |
| `xml` | `str` | XML string to parse |

**Returns:** [`Node`](./node.md) -- The root element

**Errors:** Raises an error if the XML is invalid or has no root element.

```
let doc = from_str "<root><child>text</child></root>"
assert_eq $doc.tag "root"
```

### `to_str node`

Serializes a value to an XML string.

**Parameters:**

| Name   | Type                         | Description                   |
| ------ | ---------------------------- | ----------------------------- |
| `node` | [`Node`](./node.md) or `str` | The node or text to serialize |

**Returns:** `str` -- XML string

**Errors:** Raises an error if the value cannot be serialized.

```
let n = Node "greeting"
n.push "hello"
assert_eq (to_str $n) "<greeting>hello</greeting>"
```

## Types

- [`Node`](./node.md) -- XML element node
