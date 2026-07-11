# Node

Represents an XML element node.

## Constructor

Calling `Node` as a function creates a new element node:

```
let n = Node "item"
n["id"] = "123"
n.push "content"
```

### Parameters

| Name  | Type  | Description          |
| ----- | ----- | -------------------- |
| `tag` | `str` | The tag name         |

## Fields

| Field | Type  | Description              |
| ----- | ----- | ------------------------ |
| `tag` | `str` | The element's tag name   |

The `tag` field can be read and written:

```
let n = Node "old"
assert_eq $n.tag "old"
n.tag = "new"
assert_eq $n.tag "new"
```

## Indexing

Nodes support attribute access via indexing:

```
let n = Node "item"
n["id"] = "123"
assert_eq $n["id"] "123"
```

## Methods

### `attrs()`

Returns an iterator over the node's attributes, yielding `[key, val]` pairs.

#### Returns

An iterator suitable for use in `for` loops and dict
comprehensions.

```
let el = from_str r#"<foo x="1" y="2"/>"#
for k v = el.attrs()
  echo "$k = $v"

# Or use with dict comprehension
let attrs = {...el.attrs()}
```

### `children()`

Returns an iterator over the node's children.

#### Returns

An iterator yielding child nodes (which may be `Node` or
`str` for text).

```
let el = from_str "<root><a/><b/></root>"
for child = el.children()
  echo $child.tag
```

### `traverse()`

Returns a depth-first, parent-first iterator over the node and all its
descendants.

#### Returns

An iterator yielding each node in the tree in document
order. Each yielded value is either an `Node` (element) or a `str` (text
content). The root node itself is the first value yielded.

```
let doc = from_str "<a><b><c/></b><d/></a>"
for n = doc.traverse()
  if (type n Node)
    echo $n.tag
# prints: a, b, c, d
```

To collect all element nodes into an array:

```
let nodes = [...doc.traverse()]
```

### `push child`

Appends a child to the node.

#### Parameters

| Name    | Type            | Description      |
| ------- | --------------- | ---------------- |
| `child` | `Node` or `str` | The child to add |

```
let parent = Node "parent"
let child = Node "child"
parent.push $child
parent.push "text content"
```
