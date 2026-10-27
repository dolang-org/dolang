//! The one form hover writes a signature in, whether it comes from source or
//! from the doc index.
//!
//! An annotation follows the style convention: `x @ Int` where a binding
//! stands alone, `x@Int` where several share a line. A default is shown only
//! as `= …`, since what it is isn't part of the interface, and a signature too
//! wide for one line takes the vertical parameter layout.

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
            text.push_str(" = …");
        }
        text
    }

    /// The parameter as a binding standing alone
    pub(crate) fn alone(&self) -> String {
        self.render(" @ ")
    }
}

/// A function or method's signature: `prefix`, everything before the
/// parameters such as `pub def f[T]`, then its parameters and return type
pub(crate) fn def(prefix: &str, params: &[Param], returns: Option<&str>) -> String {
    let arrow = returns.map(|ty| format!(" -> {ty}")).unwrap_or_default();
    let line = match params {
        [] => format!("{prefix}{arrow}"),
        [param] => format!("{prefix} {}{arrow}", param.alone()),
        _ => {
            let params: Vec<_> = params.iter().map(|param| param.render("@")).collect();
            format!("{prefix} {}{arrow}", params.join(" "))
        }
    };
    if line.chars().count() <= WIDTH {
        return line;
    }
    let mut text = prefix.to_owned();
    for param in params {
        text.push_str("\n  ");
        text.push_str(&param.alone());
    }
    text.push_str("\ndo");
    text.push_str(&arrow);
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
        assert_eq!(def("def f", &[], Some("Int")), "def f -> Int");
        assert_eq!(def("def f", &[], None), "def f");
        // A binding alone on its line has a spaced `@`
        let x = param("x", Some("Int"), true);
        assert_eq!(def("def f", &[x], Some("Int")), "def f x @ Int = … -> Int");
        // Bindings sharing a line have none
        let params = [
            param("self", None, false),
            param("foo: local", Some("(Int | nil)"), true),
            param(":host", Some("Str"), false),
            param("...args", Some("Str"), false),
        ];
        assert_eq!(
            def("pub def g", &params, None),
            "pub def g self foo: local@(Int | nil) = … :host@Str ...args@Str"
        );
    }

    #[test]
    fn vertical() {
        let params = [
            param(":from", Some("Str"), false),
            param(":pull", Some("Bool"), true),
            param(":tag", Some("Str"), false),
            param("...args", Some("Str"), false),
        ];
        let horizontal = def("pub def build", &params, Some("Array[Str]"));
        assert_eq!(
            horizontal,
            "pub def build :from@Str :pull@Bool = … :tag@Str ...args@Str -> Array[Str]"
        );
        let width = horizontal.chars().count();
        let wide = format!("pub def {}", "b".repeat(WIDTH - width + 6));
        assert_eq!(
            def(&wide, &params, Some("Array[Str]")),
            format!(
                "{wide}\n  :from @ Str\n  :pull @ Bool = …\n  :tag @ Str\n  ...args @ Str\ndo -> Array[Str]"
            )
        );
        let wider = format!("{wide}{}", "b".repeat(16));
        assert!(def(&wider, &params, None).ends_with("\n  ...args @ Str\ndo"));
        // At the width, it stays on one line
        let fits = format!("pub def {}", "b".repeat(WIDTH - width + 5));
        assert_eq!(
            def(&fits, &params, Some("Array[Str]")).chars().count(),
            WIDTH
        );
    }

    #[test]
    fn bindings() {
        assert_eq!(binding("field x", Some("Int")), "field x @ Int");
        assert_eq!(binding("let x", None), "let x");
    }
}
