# Decorators

Decorators transform a function, method, or class immediately after it is
defined.

## Syntax

Write each decorator as `#[expr]` on the lines immediately before the
declaration:

```
#[trace]
pub def build path
  echo "building $path"
```

The decorator expression can be any function, including a call that
configures and returns a decorator:

```
#[test]
def default_name()
  nil

#[test name: "custom_name"]
def renamed()
  nil
```

## Valid Positions

Decorators are valid in four places:

- Before a top-level `def`
- Before a top-level `class`
- Before a `def` inside a class body
- Before a `field` inside a class body, restricted to the prelude `class` and
  `static` decorators

They are not valid before `let` or other statements.

`pub` goes after any decorators:

```
#[getter]
pub def port self
  self.#port
```

## Semantics

Each decorator expression is evaluated in the surrounding scope before the
function, method, or class value is bound to its name.

After the value is created, decorators are applied from bottom to top. Each
decorator receives the current value and returns the replacement value to bind.

```
#[outer]
#[inner]
def work()
  nil
```

Conceptually, this binds `work` to `outer(inner(<function value>))`. The same
ordering applies to classes and methods.

## Function Decorators

A decorator is usually a function that accepts the defined function and returns
either the same function or a wrapped replacement.

```
let commands = []

def command name
  do |func|
    commands.push [name, func]
    func

#[command "build"]
def build()
  echo building
```

`command` runs when `build` is defined, not when `build()` is called.

Decorator factories are ordinary functions. `command "build"` evaluates first
and returns the actual decorator.

## Class Decorators

Class decorators work the same way, but receive the class object:

```
let registry = {}

def register type
  registry[str(type)] = type
  type

#[register]
pub class Job
  pub field name = ""
```

This is useful for registries, metadata attachment, or replacing the class with
an adapted version.

## Method Decorators

Method decorators receive the method function and return the value to install in
the class under that name.

The prelude's `getter` and `setter` decorators make a method a computed field's
getter or setter. Each wraps a callable, which the class calls on each read or
write of the field. Another decorator makes a computed field by returning
`getter` or `setter` applied to a callable of its own, typed as a
[`Getter`](std.Getter) or [`Setter`](std.Setter).

```
class Config
  field port = 8080

  #[getter]
  pub def port self
    self.#port

  #[setter]
  pub def port self value
    self.#port = value
```

## Member Scope Decorators

`class` and `static` place a member in the class-level namespace rather than
the instance namespace. They apply to both `def` and `field`, and they are the
only decorators a `field` accepts:

```
class Registry
  #[class]
  pub field entries = []

  #[static]
  pub def create()
    Registry()
```

On a `field` these are pure scope annotations resolved at compile time, so they
must name the prelude bindings — a shadowed `class` or `static` is rejected
rather than silently ignored. See
[Class and Static Members](./classes.md#class-and-static-members).
