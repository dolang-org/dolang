//! The one form hover writes a signature in, whether it comes from source or
//! from the doc index. What it writes parses as Do, so it can be highlighted
//! as Do.
//!
//! An annotation follows the style convention: `x @ Int` where a binding
//! stands alone, `x@Int` where several share a line. A default is shown only
//! as `= ...`, since what it is isn't part of the interface, and a signature
//! too wide for one line takes the vertical parameter layout. A function that
//! isn't type-only gets a `...` body, a class member is written in its class,
//! and a declaration from another module starts with a `# module m` comment.

/// The widest a signature is written on one line
const WIDTH: usize = 80;

/// A parameter of a signature
pub(crate) struct Param {
    /// The parameter as written up to its annotation, sigils included, such
    /// as `:host`, `foo: local` or `...args`
    pub(crate) head: String,
    /// Its annotated type, as written
    pub(crate) ty: Option<String>,
    /// Whether it has a default
    pub(crate) default: bool,
}

impl Param {
    /// The parameter with `@` written as `at`
    fn render(&self, at: &str) -> String {
        let mut text = self.head.clone();
        if let Some(ty) = &self.ty {
            text.push_str(at);
            text.push_str(ty);
        }
        if self.default {
            text.push_str(" = ...");
        }
        text
    }

    /// The parameter as a binding standing alone
    fn alone(&self) -> String {
        self.render(" @ ")
    }

    /// The parameter as a `let` binds it, without its default
    pub(crate) fn binding(&self) -> String {
        let mut text = format!("let {}", self.head);
        if let Some(ty) = &self.ty {
            text.push_str(" @ ");
            text.push_str(ty);
        }
        text.push_str(" = ...");
        text
    }
}

/// A function or method's signature: `prefix`, everything before the
/// parameters such as `pub def f[T]`, then its parameters and return type,
/// and a `...` body when it has a `body`
pub(crate) fn def(prefix: &str, params: &[Param], returns: Option<&str>, body: bool) -> String {
    let arrow = returns.map(|ty| format!(" -> {ty}")).unwrap_or_default();
    let line = match params {
        [] => format!("{prefix}{arrow}"),
        [param] => format!("{prefix} {}{arrow}", param.alone()),
        _ => {
            let params: Vec<_> = params.iter().map(|param| param.render("@")).collect();
            format!("{prefix} {}{arrow}", params.join(" "))
        }
    };
    // Decorators are lines of their own
    let last = line.rsplit('\n').next().unwrap_or(&line);
    let mut text = if last.chars().count() <= WIDTH {
        line
    } else {
        let mut text = prefix.to_owned();
        for param in params {
            text.push_str("\n  ");
            text.push_str(&param.alone());
        }
        text.push_str("\ndo");
        text.push_str(&arrow);
        text
    };
    if body {
        text.push_str("\n  ...");
    }
    text
}

/// A binding standing alone, such as `field x @ Int`, from `binding`, its
/// keyword and name
pub(crate) fn binding(binding: &str, ty: Option<&str>) -> String {
    match ty {
        Some(ty) => format!("{binding} @ {ty}"),
        None => binding.to_owned(),
    }
}

/// A variable binding, such as `let x @ Int = ...`, from `binding`, its
/// keyword and name
pub(crate) fn variable(binding: &str, ty: Option<&str>) -> String {
    format!("{} = ...", self::binding(binding, ty))
}

/// `label` in the class whose header is `class`, if it's a member, and noted
/// as in `module` when that's given
pub(crate) fn member(module: Option<&str>, class: Option<&str>, label: &str) -> String {
    let mut text = String::new();
    if let Some(module) = module {
        text.push_str(&format!("# module {module}\n"));
    }
    match class {
        Some(class) => {
            text.push_str(class);
            let lines: Vec<_> = label.lines().map(|line| format!("\n  {line}")).collect();
            text.push_str(&lines.concat());
        }
        None => text.push_str(label),
    }
    text
}

/// A line of what a check concluded: what's `named` has type `ty`, an
/// annotation, and was called as `overload`, from 0. A qualified name such as
/// `m.Box.item` binds its last part, and is kept in a comment.
pub(crate) fn typed(named: Option<&str>, ty: &str, overload: Option<u32>) -> String {
    let name = named.map_or("_", |named| named.rsplit('.').next().unwrap_or(named));
    let mut notes = Vec::new();
    notes.extend(named.filter(|named| named.contains('.')).map(str::to_owned));
    notes.extend(overload.map(|index| format!("overload {}", index + 1)));
    let line = format!("let {name} @ {ty} = ...");
    match &notes[..] {
        [] => line,
        notes => format!("{line}  # {}", notes.join(", ")),
    }
}

/// Fails unless `text` parses as Do, leaving out names it can't resolve
#[cfg(test)]
pub(crate) fn assert_parses(text: &str) {
    use dolang_compile::{Config, diag::Severity};
    let unit = Config::new().unit(std::path::Path::new("hover.dol"), text);
    let errors: Vec<_> = unit
        .diagnostics()
        .filter(|diag| diag.severity() == Severity::Error)
        .map(|diag| diag.message().to_string())
        .filter(|message| message != "unbound identifier")
        .collect();
    assert!(errors.is_empty(), "{text}\n{errors:#?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(head: &str, ty: Option<&str>, default: bool) -> Param {
        Param {
            head: head.to_owned(),
            ty: ty.map(str::to_owned),
            default,
        }
    }

    #[test]
    fn one_line() {
        assert_eq!(def("def f", &[], Some("Int"), false), "def f -> Int");
        assert_eq!(def("def f", &[], None, true), "def f\n  ...");
        // A binding alone on its line has a spaced `@`
        let x = param("x", Some("Int"), true);
        assert_eq!(
            def("def f", &[x], Some("Int"), true),
            "def f x @ Int = ... -> Int\n  ..."
        );
        // Bindings sharing a line have none
        let params = [
            param("self", None, false),
            param("foo: local", Some("(Int | nil)"), true),
            param(":host", Some("Str"), false),
            param("...args", Some("Str"), false),
        ];
        let text = def("pub def g", &params, None, true);
        assert_eq!(
            text,
            "pub def g self foo: local@(Int | nil) = ... :host@Str ...args@Str\n  ..."
        );
        assert_parses(&text);
        assert_parses(&def("@def f", &params, Some("Int"), false));
        assert_parses(&def("#[getter]\npub def g", &params, None, true));
    }

    #[test]
    fn vertical() {
        let params = [
            param(":from", Some("Str"), false),
            param(":pull", Some("Bool"), true),
            param(":tag", Some("Str"), false),
            param("...args", Some("Str"), false),
        ];
        let horizontal = def("pub def build", &params, Some("Array[Str]"), false);
        assert_eq!(
            horizontal,
            "pub def build :from@Str :pull@Bool = ... :tag@Str ...args@Str -> Array[Str]"
        );
        let width = horizontal.chars().count();
        let wide = format!("pub def {}", "b".repeat(WIDTH - width + 6));
        let text = def(&wide, &params, Some("Array[Str]"), true);
        assert_eq!(
            text,
            format!(
                "{wide}\n  :from @ Str\n  :pull @ Bool = ...\n  :tag @ Str\n  ...args @ Str\ndo -> Array[Str]\n  ..."
            )
        );
        assert_parses(&text);
        let wider = format!("{wide}{}", "b".repeat(16));
        assert!(def(&wider, &params, None, false).ends_with("\n  ...args @ Str\ndo"));
        // At the width, it stays on one line
        let fits = format!("pub def {}", "b".repeat(WIDTH - width + 5));
        assert_eq!(
            def(&fits, &params, Some("Array[Str]"), false)
                .chars()
                .count(),
            WIDTH
        );
        // Decorators don't count toward it
        let decorated = format!("#[getter]\n{fits}");
        assert!(!def(&decorated, &params, Some("Array[Str]"), false).contains("\ndo"));
    }

    #[test]
    fn bindings() {
        assert_eq!(binding("field x", Some("Int")), "field x @ Int");
        assert_eq!(variable("let x", None), "let x = ...");
        assert_eq!(variable("pub let x", Some("Int")), "pub let x @ Int = ...");
        assert_parses(&variable("pub let x", Some("(Int | nil)")));
        let params = [
            param(":limit", Some("Int"), true),
            param("foo: local", Some("(Int | nil)"), false),
            param("...args", Some("Str"), false),
            param("self", None, false),
        ];
        for param in &params {
            assert_parses(&param.binding());
        }
        assert_eq!(params[0].binding(), "let :limit @ Int = ...");
    }

    #[test]
    fn members() {
        let method = def("def (init)", &[param("self", None, false)], None, true);
        let text = member(Some("m"), Some("class Box[T]"), &method);
        assert_eq!(text, "# module m\nclass Box[T]\n  def (init) self\n    ...");
        assert_parses(&text);
        let field = member(
            None,
            Some("class Box"),
            &binding("pub field x", Some("Int")),
        );
        assert_eq!(field, "class Box\n  pub field x @ Int");
        assert_parses(&field);
        let top = member(Some("m"), None, "let x = ...");
        assert_eq!(top, "# module m\nlet x = ...");
    }

    #[test]
    fn type_lines() {
        assert_eq!(typed(Some("x"), "std.Int", None), "let x @ std.Int = ...");
        assert_eq!(
            typed(Some("m.Box.item"), "std.Int", None),
            "let item @ std.Int = ...  # m.Box.item"
        );
        assert_eq!(
            typed(Some("m.f"), "((std.Str) -> std.Str)", Some(1)),
            "let f @ ((std.Str) -> std.Str) = ...  # m.f, overload 2"
        );
        assert_eq!(
            typed(None, "std.Int", Some(0)),
            "let _ @ std.Int = ...  # overload 1"
        );
        assert_parses(&typed(Some("m.A.x | m.B.x"), "(std.Int | nil)", None));
        assert_parses(&typed(Some("f"), "((*Value, **Value) -> nil)", Some(1)));
    }
}
