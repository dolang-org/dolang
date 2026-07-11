# type

`type` is the type of all types. It can also be used to query and test types.

## `type value`

Returns the type object representing the value's type.

### Parameters

| Name    | Type | Description        |
| ------- | ---- | ------------------ |
| `value` |      | the value to query |

#### Returns

type object

```
assert_eq (type 42) $int
assert_eq (type "hello") $str
assert_eq (type [1, 2]) $array
assert_eq (type nil) $Nil
```

## `type value type`

Tests whether a value is an instance of the given type, including through
inheritance.

### Parameters

| Name    | Type | Description        |
| ------- | ---- | ------------------ |
| `value` |      | the value to test  |
| `type`  |      | the type to check  |

#### Returns

`bool`

```
assert (type 42 int)
assert (type "hello" str)
assert (type nil Nil)

class Animal
class Dog: Animal

let d = Dog()
assert (type d Dog)
assert (type d Animal)
```
