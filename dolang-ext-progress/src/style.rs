use dolang::runtime::{Error, Result, Strand, Sym, Value};
use indicatif as ix;

// --- Color and attribute enums ---

#[derive(Clone, Copy)]
pub(crate) enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    /// Just `.bright` / `.on_bright` — brightens the default color without changing it.
    Bright,
}

impl Color {
    fn fg_fmt(self, s: &mut String) {
        use Color::*;
        s.push('.');
        match self {
            Black => s.push_str("black"),
            Red => s.push_str("red"),
            Green => s.push_str("green"),
            Yellow => s.push_str("yellow"),
            Blue => s.push_str("blue"),
            Magenta => s.push_str("magenta"),
            Cyan => s.push_str("cyan"),
            White => s.push_str("white"),
            BrightBlack => s.push_str("bright.black"),
            BrightRed => s.push_str("bright.red"),
            BrightGreen => s.push_str("bright.green"),
            BrightYellow => s.push_str("bright.yellow"),
            BrightBlue => s.push_str("bright.blue"),
            BrightMagenta => s.push_str("bright.magenta"),
            BrightCyan => s.push_str("bright.cyan"),
            BrightWhite => s.push_str("bright.white"),
            Bright => s.push_str("bright"),
        }
    }

    fn bg_fmt(self, s: &mut String) {
        use Color::*;
        match self {
            Black => s.push_str(".on_black"),
            Red => s.push_str(".on_red"),
            Green => s.push_str(".on_green"),
            Yellow => s.push_str(".on_yellow"),
            Blue => s.push_str(".on_blue"),
            Magenta => s.push_str(".on_magenta"),
            Cyan => s.push_str(".on_cyan"),
            White => s.push_str(".on_white"),
            BrightBlack => s.push_str(".on_bright.on_black"),
            BrightRed => s.push_str(".on_bright.on_red"),
            BrightGreen => s.push_str(".on_bright.on_green"),
            BrightYellow => s.push_str(".on_bright.on_yellow"),
            BrightBlue => s.push_str(".on_bright.on_blue"),
            BrightMagenta => s.push_str(".on_bright.on_magenta"),
            BrightCyan => s.push_str(".on_bright.on_cyan"),
            BrightWhite => s.push_str(".on_bright.on_white"),
            Bright => s.push_str(".on_bright"),
        }
    }
}

impl TryFrom<&str> for Color {
    type Error = String;

    fn try_from(s: &str) -> std::result::Result<Self, Self::Error> {
        use Color::*;
        if s == "bright" {
            return Ok(Bright);
        }
        let (base, bright) = match s.strip_suffix(":bright") {
            Some(base) => (base, true),
            None => (s, false),
        };
        let color = match base {
            "black" => Black,
            "red" => Red,
            "green" => Green,
            "yellow" => Yellow,
            "blue" => Blue,
            "magenta" => Magenta,
            "cyan" => Cyan,
            "white" => White,
            _ => return Err(format!("unknown color: '{s}'")),
        };
        Ok(if bright {
            match color {
                Black => BrightBlack,
                Red => BrightRed,
                Green => BrightGreen,
                Yellow => BrightYellow,
                Blue => BrightBlue,
                Magenta => BrightMagenta,
                Cyan => BrightCyan,
                White => BrightWhite,
                _ => unreachable!(),
            }
        } else {
            color
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Attr {
    Bold,
    Dim,
    Italic,
    Underlined,
    Blink,
    Reverse,
    Hidden,
    Strikethrough,
}

impl Attr {
    fn fmt(self, s: &mut String) {
        use Attr::*;
        s.push('.');
        match self {
            Bold => s.push_str("bold"),
            Dim => s.push_str("dim"),
            Italic => s.push_str("italic"),
            Underlined => s.push_str("underlined"),
            Blink => s.push_str("blink"),
            Reverse => s.push_str("reverse"),
            Hidden => s.push_str("hidden"),
            Strikethrough => s.push_str("strikethrough"),
        }
    }
}

impl TryFrom<&str> for Attr {
    type Error = String;

    fn try_from(s: &str) -> std::result::Result<Self, Self::Error> {
        use Attr::*;
        match s {
            "bold" => Ok(Bold),
            "dim" => Ok(Dim),
            "italic" => Ok(Italic),
            "underlined" => Ok(Underlined),
            "blink" => Ok(Blink),
            "reverse" => Ok(Reverse),
            "hidden" => Ok(Hidden),
            "strikethrough" => Ok(Strikethrough),
            _ => Err(format!("unknown attribute: '{s}'")),
        }
    }
}

// --- Element style ---

#[derive(Clone, Default)]
pub(crate) struct ElementStyle {
    fg: Option<Color>,
    bg: Option<Color>,
    attrs: Vec<Attr>,
}

impl ElementStyle {
    fn to_template_suffix(&self) -> String {
        let mut s = String::new();
        for attr in &self.attrs {
            attr.fmt(&mut s);
        }
        if let Some(fg) = self.fg {
            fg.fg_fmt(&mut s);
        }
        if let Some(bg) = self.bg {
            bg.bg_fmt(&mut s);
        }
        s
    }

    fn to_template_suffix_with_alt(&self, alt: &ElementStyle) -> String {
        let mut s = self.to_template_suffix();
        let alt_s = alt.to_template_suffix();
        if !alt_s.is_empty() {
            s.push('/');
            // Strip leading '.' from alt since '/' already separates
            s.push_str(&alt_s[1..]);
        }
        s
    }
}

// --- Style ---

#[derive(Clone)]
pub(crate) struct Style {
    pub(crate) bar_width: u16,
    pub(crate) message_width: u16,
    pub(crate) icon_width: u16,
    bar: ElementStyle,
    bar_alt: ElementStyle,
    spinner: ElementStyle,
    message: ElementStyle,
    icon: ElementStyle,
    elapsed: ElementStyle,
    position: ElementStyle,
    total: ElementStyle,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bar_width: 20,
            message_width: 40,
            icon_width: 2,
            bar: ElementStyle {
                fg: Some(Color::Cyan),
                ..Default::default()
            },
            bar_alt: ElementStyle {
                fg: Some(Color::Blue),
                ..Default::default()
            },
            spinner: ElementStyle {
                fg: Some(Color::Cyan),
                ..Default::default()
            },
            message: ElementStyle {
                ..Default::default()
            },
            icon: ElementStyle {
                fg: Some(Color::Bright),
                attrs: vec![Attr::Bold],
                ..Default::default()
            },
            elapsed: ElementStyle {
                attrs: vec![Attr::Dim],
                ..Default::default()
            },
            position: ElementStyle::default(),
            total: ElementStyle::default(),
        }
    }
}

// --- Units ---

#[derive(Clone, Copy)]
pub(crate) enum Units {
    Count,
    Bytes,
}

// --- Mode ---

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Bar,
    Spinner,
}

// --- Template generation ---

const MIN_MSG_WIDTH: u16 = 10;

fn effective_indent(style: &Style, depth: u16) -> u16 {
    let max_indent = style.message_width.saturating_sub(MIN_MSG_WIDTH);
    (depth * 2).min(max_indent)
}

pub(crate) fn bar_template(style: &Style, depth: u16, units: Option<Units>) -> String {
    let indent = effective_indent(style, depth);
    let iw = style.icon_width + indent;
    let mw = style.message_width - indent;
    let bw = style.bar_width;
    let ic = style.icon.to_template_suffix();
    let mc = style.message.to_template_suffix();
    let bc = style.bar.to_template_suffix_with_alt(&style.bar_alt);
    let ec = style.elapsed.to_template_suffix();
    let pc = style.position.to_template_suffix();
    let tc = style.total.to_template_suffix();
    match units {
        None | Some(Units::Count) => format!(
            "{{prefix:>{iw}{ic}}} {{msg:{mw}!{mc}}} {{bar:{bw}{bc}}} {{pos:{pc}}}/{{len:{tc}}} {{elapsed:{ec}}}"
        ),
        Some(Units::Bytes) => format!(
            "{{prefix:>{iw}{ic}}} {{msg:{mw}!{mc}}} {{bar:{bw}{bc}}} {{bytes:{pc}}}/{{total_bytes:{tc}}} {{elapsed:{ec}}}"
        ),
    }
}

pub(crate) const BAR_CHARS: &str = "━╸━";
pub(crate) const DEFAULT_ICON: &str = "●";

pub(crate) fn spinner_template(
    style: &Style,
    depth: u16,
    units: Option<Units>,
    leaf: bool,
) -> String {
    let indent = effective_indent(style, depth);
    let iw = style.icon_width + indent;
    let ic = style.icon.to_template_suffix();
    let mc = style.message.to_template_suffix();
    let ec = style.elapsed.to_template_suffix();
    let pc = style.position.to_template_suffix();
    const SW: usize = 1;

    // Leaf nodes show a spinner; non-leaf nodes hide it and give the space to the message.
    let (spinner_part, mw_extra) = if leaf {
        let sc = style.spinner.to_template_suffix();
        (format!(" {{spinner:>{sc}}}"), style.bar_width - SW as u16)
    } else {
        (format!("{:<SW$}", ""), style.bar_width)
    };
    let mw = style.message_width - indent + mw_extra;

    match units {
        None => {
            format!("{{prefix:>{iw}{ic}}} {{msg:{mw}!{mc}}}{spinner_part} {{elapsed:{ec}}}")
        }
        Some(Units::Count) => {
            format!(
                "{{prefix:>{iw}{ic}}} {{msg:{mw}!{mc}}}{spinner_part} {{pos:{pc}}} {{elapsed:{ec}}}"
            )
        }
        Some(Units::Bytes) => {
            format!(
                "{{prefix:>{iw}{ic}}} {{msg:{mw}!{mc}}}{spinner_part} {{bytes:{pc}}} {{elapsed:{ec}}}"
            )
        }
    }
}

// --- Style application helpers ---

pub(crate) fn apply_bar_style(
    bar: &ix::ProgressBar,
    style: &Style,
    depth: u16,
    units: Option<Units>,
) {
    let tmpl = bar_template(style, depth, units);
    let s = ix::ProgressStyle::with_template(&tmpl)
        .expect("valid bar template")
        .progress_chars(BAR_CHARS);
    bar.set_style(s);
}

pub(crate) fn apply_spinner_style(
    bar: &ix::ProgressBar,
    style: &Style,
    depth: u16,
    units: Option<Units>,
    leaf: bool,
) {
    let tmpl = spinner_template(style, depth, units, leaf);
    let s = ix::ProgressStyle::with_template(&tmpl).expect("valid spinner template");
    bar.set_style(s);
}

// --- Style dict parsing ---

/// Keys needed for style dict parsing. All are `Copy` `Sym` values.
#[derive(Clone, Copy)]
pub(crate) struct StyleKeys<'v> {
    pub(crate) bar: Sym<'v, 'v>,
    pub(crate) spinner: Sym<'v, 'v>,
    pub(crate) message: Sym<'v, 'v>,
    pub(crate) icon: Sym<'v, 'v>,
    pub(crate) elapsed: Sym<'v, 'v>,
    pub(crate) position: Sym<'v, 'v>,
    pub(crate) total: Sym<'v, 'v>,
    pub(crate) width: Sym<'v, 'v>,
    pub(crate) fg: Sym<'v, 'v>,
    pub(crate) bg: Sym<'v, 'v>,
    pub(crate) attrs: Sym<'v, 'v>,
    pub(crate) alt: Sym<'v, 'v>,
}

fn parse_element_style<'v, 's>(
    strand: &mut Strand<'v, 's>,
    cat: &Value<'v>,
    keys: &StyleKeys<'v>,
    es: &mut ElementStyle,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut slot]| {
        // fg
        if cat.index(strand, keys.fg, &mut slot).is_ok() && !slot.is_nil() {
            let s = slot
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "style: fg: expected `str`"))?
                .to_string();
            es.fg = Some(
                Color::try_from(s.as_str())
                    .map_err(|e| Error::runtime(strand, format!("style: fg: {e}")))?,
            );
        }
        // bg
        if cat.index(strand, keys.bg, &mut slot).is_ok() && !slot.is_nil() {
            let s = slot
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "style: bg: expected `str`"))?
                .to_string();
            es.bg = Some(
                Color::try_from(s.as_str())
                    .map_err(|e| Error::runtime(strand, format!("style: bg: {e}")))?,
            );
        }
        // attrs
        if cat.index(strand, keys.attrs, &mut slot).is_ok() && !slot.is_nil() {
            let vm = strand.vm();
            let arr = slot
                .as_array(vm)
                .ok_or_else(|| Error::type_error(strand, "style: attrs: expected array"))?;
            let len = arr.len(strand)?;
            es.attrs.clear();
            for i in 0..len {
                strand.with_slots_sync(|strand, [mut elem]| {
                    arr.get(strand, i, &mut elem)?;
                    let s = elem
                        .as_str(strand)
                        .ok_or_else(|| {
                            Error::type_error(strand, "style: attrs: expected `str` element")
                        })?
                        .to_string();
                    let attr = Attr::try_from(s.as_str())
                        .map_err(|e| Error::runtime(strand, format!("style: attrs: {e}")))?;
                    es.attrs.push(attr);
                    Ok(())
                })?;
            }
        }
        Ok(())
    })
}

pub(crate) fn parse_style<'v, 's>(
    strand: &mut Strand<'v, 's>,
    style_val: &Value<'v>,
    keys: &StyleKeys<'v>,
) -> Result<'v, 's, Style> {
    let mut style = Style::default();

    // Categories with width + color
    struct WidthCat<'a> {
        width: &'a mut u16,
        es: &'a mut ElementStyle,
    }
    for (key, wc) in [
        (
            keys.bar,
            WidthCat {
                width: &mut style.bar_width,
                es: &mut style.bar,
            },
        ),
        (
            keys.message,
            WidthCat {
                width: &mut style.message_width,
                es: &mut style.message,
            },
        ),
        (
            keys.icon,
            WidthCat {
                width: &mut style.icon_width,
                es: &mut style.icon,
            },
        ),
    ] {
        strand.with_slots_sync(|strand, [mut cat, mut val]| {
            if style_val.index(strand, key, &mut cat).is_ok() && !cat.is_nil() {
                if cat.index(strand, keys.width, &mut val).is_ok() && !val.is_nil() {
                    let n = val
                        .to_i64(strand)
                        .map_err(|_| Error::type_error(strand, "style: width: expected `int`"))?;
                    *wc.width = n as u16;
                }
                parse_element_style(strand, &cat, keys, wc.es)?;
            }
            Ok(())
        })?;
    }

    // bar.alt
    strand.with_slots_sync(|strand, [mut cat, mut alt_val]| {
        if style_val.index(strand, keys.bar, &mut cat).is_ok()
            && !cat.is_nil()
            && cat.index(strand, keys.alt, &mut alt_val).is_ok()
            && !alt_val.is_nil()
        {
            parse_element_style(strand, &alt_val, keys, &mut style.bar_alt)?;
        }
        Ok(())
    })?;

    // Color-only categories
    for (key, es) in [
        (keys.spinner, &mut style.spinner),
        (keys.elapsed, &mut style.elapsed),
        (keys.position, &mut style.position),
        (keys.total, &mut style.total),
    ] {
        strand.with_slots_sync(|strand, [mut cat]| {
            if style_val.index(strand, key, &mut cat).is_ok() && !cat.is_nil() {
                parse_element_style(strand, &cat, keys, es)?;
            }
            Ok(())
        })?;
    }

    Ok(style)
}
