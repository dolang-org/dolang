use dolang::runtime::{Error, Result, Strand, Sym, Value};
use indicatif as ix;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

    /// Raw ANSI SGR code for this color as a foreground.
    fn fg_ansi(self) -> &'static str {
        use Color::*;
        match self {
            Black => "30",
            Red => "31",
            Green => "32",
            Yellow => "33",
            Blue => "34",
            Magenta => "35",
            Cyan => "36",
            White => "37",
            BrightBlack => "90",
            BrightRed => "91",
            BrightGreen => "92",
            BrightYellow => "93",
            BrightBlue => "94",
            BrightMagenta => "95",
            BrightCyan => "96",
            BrightWhite => "97",
            // No specific color: brighten whatever the terminal default is.
            Bright => "1",
        }
    }

    /// Raw ANSI SGR code for this color as a background, or `None` if there
    /// is no direct SGR equivalent (bare `Bright` used as a background).
    fn bg_ansi(self) -> Option<&'static str> {
        use Color::*;
        Some(match self {
            Black => "40",
            Red => "41",
            Green => "42",
            Yellow => "43",
            Blue => "44",
            Magenta => "45",
            Cyan => "46",
            White => "47",
            BrightBlack => "100",
            BrightRed => "101",
            BrightGreen => "102",
            BrightYellow => "103",
            BrightBlue => "104",
            BrightMagenta => "105",
            BrightCyan => "106",
            BrightWhite => "107",
            Bright => return None,
        })
    }
}

fn parse_color_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
    colors: ColorKeys<'v>,
) -> Result<'v, 's, Color> {
    let value = value
        .as_sym(strand)
        .ok_or_else(|| Error::type_error(strand, format!("style: {name}: expected `Sym`")))?;
    colors
        .get(value)
        .ok_or_else(|| Error::value(strand, format!("style: {name}: unknown color")))
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

impl Attr {
    fn ansi(self) -> &'static str {
        use Attr::*;
        match self {
            Bold => "1",
            Dim => "2",
            Italic => "3",
            Underlined => "4",
            Blink => "5",
            Reverse => "7",
            Hidden => "8",
            Strikethrough => "9",
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

    /// Appends a raw ANSI SGR escape sequence for this style to `s`, or
    /// nothing if the style has no attrs/colors set. Used for the
    /// non-interactive (plain) rendering path, parallel to
    /// [`to_template_suffix`](Self::to_template_suffix) which drives
    /// indicatif's own template mini-language for the interactive path.
    pub(crate) fn write_ansi_prefix(&self, s: &mut String) {
        let mut codes: Vec<&str> = Vec::new();
        for attr in &self.attrs {
            codes.push(attr.ansi());
        }
        if let Some(fg) = self.fg {
            codes.push(fg.fg_ansi());
        }
        if let Some(bg) = self.bg
            && let Some(code) = bg.bg_ansi()
        {
            codes.push(code);
        }
        if !codes.is_empty() {
            s.push_str("\x1b[");
            s.push_str(&codes.join(";"));
            s.push('m');
        }
    }
}

/// ANSI SGR reset sequence, paired with [`ElementStyle::write_ansi_prefix`].
pub(crate) const ANSI_RESET: &str = "\x1b[0m";

// --- Style ---

#[derive(Clone)]
pub(crate) struct Style {
    pub(crate) bar_width: u16,
    pub(crate) message_width: u16,
    pub(crate) icon_width: u16,
    /// Width of the position/total/throughput readout between the bar and
    /// the elapsed time — the "status" category in the style dict. Fixed
    /// (rather than sized to content) so that field stays aligned across
    /// sibling bars even when they mix `COUNT` and `BYTES` units, whose
    /// rendered widths differ a lot (`"42/100"` vs `"1.43 MiB/500.00 MiB
    /// (2.10 MiB/s)"`).
    pub(crate) status_width: u16,
    bar: ElementStyle,
    bar_alt: ElementStyle,
    spinner: ElementStyle,
    message: ElementStyle,
    icon: ElementStyle,
    elapsed: ElementStyle,
    position: ElementStyle,
    total: ElementStyle,
}

impl Style {
    // Accessors for the plain (non-interactive) rendering path, which
    // builds raw ANSI lines directly rather than going through indicatif's
    // template mini-language.
    pub(crate) fn bar(&self) -> &ElementStyle {
        &self.bar
    }

    pub(crate) fn bar_alt(&self) -> &ElementStyle {
        &self.bar_alt
    }

    pub(crate) fn message(&self) -> &ElementStyle {
        &self.message
    }

    pub(crate) fn position(&self) -> &ElementStyle {
        &self.position
    }

    pub(crate) fn total(&self) -> &ElementStyle {
        &self.total
    }

    pub(crate) fn icon(&self) -> &ElementStyle {
        &self.icon
    }

    pub(crate) fn elapsed(&self) -> &ElementStyle {
        &self.elapsed
    }
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bar_width: 20,
            message_width: 40,
            icon_width: 2,
            // Comfortably fits a `BYTES` readout with throughput (e.g.
            // `"1.43/500.00 MiB (2.10 MiB/s)"` is 29 columns, thanks to
            // `write_status_text` sharing one unit between position and
            // total instead of repeating it) without being so wide that a
            // short `COUNT` readout like `"5/10"` leaves a large dead gap
            // before the elapsed column.
            status_width: 30,
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

impl Style {
    /// Widest a default (no explicit `style:` kwarg) message column is
    /// allowed to grow to, even when the terminal is very wide — an
    /// unbounded message column reads as unaesthetically long lines rather
    /// than "using space well".
    const MAX_DEFAULT_MESSAGE_WIDTH: u16 = 100;

    /// Rough width budget for the elapsed column in the default template —
    /// covers `"an hour"`-style `HumanDuration` output without needing an
    /// exact figure, since elapsed isn't a fixed-width field.
    const ELAPSED_BUDGET: u16 = 10;

    /// Number of single-space separators between the icon/message/bar/status/
    /// elapsed columns in the default template.
    const SEPARATORS: u16 = 4;

    /// Builds the default style, sizing the message column to use whatever
    /// terminal width is available (from `cols`, e.g. `stderr_cols`) beyond
    /// the other fixed-width columns, capped at
    /// [`MAX_DEFAULT_MESSAGE_WIDTH`](Self::MAX_DEFAULT_MESSAGE_WIDTH) and
    /// floored at [`MIN_MSG_WIDTH`]. `cols: None` (not a terminal, or the
    /// terminal declined to report its size) keeps the fixed built-in
    /// default.
    pub(crate) fn default_for_cols(cols: Option<u16>) -> Self {
        let mut style = Self::default();
        if let Some(cols) = cols {
            let fixed = style.icon_width
                + style.bar_width
                + style.status_width
                + Self::ELAPSED_BUDGET
                + Self::SEPARATORS;
            style.message_width = cols
                .saturating_sub(fixed)
                .clamp(MIN_MSG_WIDTH, Self::MAX_DEFAULT_MESSAGE_WIDTH);
        }
        style
    }
}

// --- Units ---

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Units {
    Count,
    Bytes,
    Percent,
}

// --- Mode ---

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Bar,
    Spinner,
}

// --- Status text rendering ---
//
// The "status" template field (position/total, percentage, plus throughput
// for `BYTES` indicators) is rendered by hand rather than through indicatif's own
// `{pos}`/`{bytes}`/`{bytes_per_sec}` keys, so that both units share one
// fixed-width, ANSI-aware field — the same text this module hands to
// indicatif via a custom template key is also what `plain.rs` prints
// directly for the non-interactive path.

/// Appends `text` to `line`, wrapped in `style`'s ANSI SGR codes if `ansi` is
/// set (and the style actually sets anything — an empty prefix means no
/// reset is needed either). Shared by the interactive status field and
/// `plain.rs`'s hand-rolled line rendering, which both need the same
/// "only pay for ANSI when it's wanted" behavior.
pub(crate) fn write_styled(line: &mut String, ansi: bool, style: &ElementStyle, text: &str) {
    if !ansi {
        line.push_str(text);
        return;
    }
    let before = line.len();
    style.write_ansi_prefix(line);
    let styled = line.len() != before;
    line.push_str(text);
    if styled {
        line.push_str(ANSI_RESET);
    }
}

/// Pads `text` with spaces to `width` columns, or truncates it to `width`
/// columns (replacing the cut-off tail with a single `…`) if it's longer.
/// Uses display width, not char count, so wide glyphs (most emoji, CJK
/// text) don't throw off later columns. Assumes `text` carries no embedded
/// ANSI codes — callers that need color apply it after fitting (padding
/// spaces are safe to add after colored text; truncating colored text
/// safely is not, so [`fit_status`] measures the plain form first).
pub(crate) fn fit(text: &str, width: usize) -> String {
    let text_width = UnicodeWidthStr::width(text);
    if text_width <= width {
        let mut s = text.to_string();
        for _ in 0..width - text_width {
            s.push(' ');
        }
        return s;
    }
    if width == 0 {
        return String::new();
    }
    // Reserve 1 column for the ellipsis itself.
    let budget = width - 1;
    let mut truncated = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        truncated.push(ch);
        used += w;
    }
    // The truncated prefix may be narrower than `budget` if the next
    // character didn't fit (e.g. a wide char at the boundary) — pad so the
    // field still ends at `width` columns.
    for _ in 0..budget - used {
        truncated.push(' ');
    }
    truncated.push('…');
    truncated
}

/// Binary-prefix exponent for `value` — 0 for a bare byte count, 1 for
/// `Ki`, 2 for `Mi`, and so on — chosen the same way indicatif's own
/// `HumanBytes` picks a prefix (repeatedly divide by 1024 while still
/// `>= 1024`, capped at `Yi`).
fn binary_exponent(value: u64) -> u32 {
    let mut v = value as f64;
    let mut exp = 0u32;
    while v >= 1024.0 && exp < 8 {
        v /= 1024.0;
        exp += 1;
    }
    exp
}

const BINARY_UNIT_PREFIXES: [&str; 9] = ["", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei", "Zi", "Yi"];

/// `value` scaled to the binary prefix at `exp`, formatted as a bare number
/// with no unit suffix — 0 decimal places at `exp == 0` (a whole byte count
/// looks odd as `"15.00"`), 1 otherwise. `HumanBytes` itself uses 2, but a
/// single fractional digit is plenty of precision for a live status readout
/// and it's a couple of columns saved on every `BYTES` field.
fn scaled_bytes_number(value: u64, exp: u32) -> String {
    let scaled = value as f64 / 1024f64.powi(exp as i32);
    if exp == 0 {
        format!("{scaled:.0}")
    } else {
        format!("{scaled:.1}")
    }
}

fn binary_unit_suffix(exp: u32) -> &'static str {
    BINARY_UNIT_PREFIXES[exp as usize]
}

/// Renders the position/total readout, plus a trailing `(N/s)` throughput
/// suffix for `BYTES` indicators when `rate` is known. `rate` is ignored
/// for `COUNT`/unitless indicators — per-item rates aren't a thing indicators
/// report today.
///
/// For `BYTES` with a total, the *total* picks the unit (e.g. `MiB`) and
/// the position is scaled to match it without repeating the suffix —
/// `"12.34/500.00 MiB"` rather than indicatif's own `"12.34 MiB/500.00
/// MiB"` — since the position's own magnitude is rarely interesting on its
/// own and the repeated unit just eats into the fixed `status` width for no
/// benefit.
pub(crate) fn write_status_text(
    units: Option<Units>,
    pos: u64,
    total: Option<u64>,
    rate: Option<f64>,
    ansi: bool,
    style: &Style,
) -> String {
    let position_style = style.position();
    let total_style = style.total();
    let mut s = String::new();
    match (units, total) {
        (Some(Units::Percent), Some(total)) => {
            let percent = if total == 0 {
                0
            } else {
                u64::try_from((u128::from(pos) * 100) / u128::from(total))
                    .expect("percentage fits in u64")
            };
            write_styled(&mut s, ansi, position_style, &format!("{percent}%"));
        }
        (Some(Units::Percent), None) => {
            write_styled(&mut s, ansi, position_style, &format!("{pos}%"));
        }
        (Some(Units::Bytes), Some(total)) => {
            let exp = binary_exponent(total);
            write_styled(&mut s, ansi, position_style, &scaled_bytes_number(pos, exp));
            s.push('/');
            write_styled(
                &mut s,
                ansi,
                total_style,
                &format!(
                    "{} {}B",
                    scaled_bytes_number(total, exp),
                    binary_unit_suffix(exp)
                ),
            );
        }
        (Some(Units::Bytes), None) => {
            let exp = binary_exponent(pos);
            write_styled(
                &mut s,
                ansi,
                position_style,
                &format!(
                    "{} {}B",
                    scaled_bytes_number(pos, exp),
                    binary_unit_suffix(exp)
                ),
            );
        }
        (_, Some(total)) => {
            write_styled(&mut s, ansi, position_style, &pos.to_string());
            s.push('/');
            write_styled(&mut s, ansi, total_style, &total.to_string());
        }
        (_, None) => {
            write_styled(&mut s, ansi, position_style, &pos.to_string());
        }
    }
    if let (Some(Units::Bytes), Some(rate)) = (units, rate) {
        let rate = rate.round() as u64;
        let exp = binary_exponent(rate);
        s.push_str(" (");
        write_styled(
            &mut s,
            ansi,
            position_style,
            &format!(
                "{} {}B/s",
                scaled_bytes_number(rate, exp),
                binary_unit_suffix(exp)
            ),
        );
        s.push(')');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{Style, Units, write_status_text};

    #[test]
    fn percent_status_uses_total() {
        assert_eq!(
            write_status_text(
                Some(Units::Percent),
                4,
                Some(10),
                None,
                false,
                &Style::default()
            ),
            "40%"
        );
    }
}

/// [`write_status_text`], capped to `width` if it would exceed it —
/// deliberately *not* padded when it's shorter, unlike [`fit`] (used for
/// `message`, where a fixed width keeps sibling rows' bar/status columns
/// aligned). `status` has no such row to keep aligned with: a short `COUNT`
/// readout like `"5/10"` should be followed immediately by elapsed, the
/// same way a spinner with no total already is, rather than padded out to
/// match how wide a `BYTES` sibling's readout happens to be — the elapsed
/// column visibly jumping around next to short/long status text is normal
/// (it already does across a `COUNT` bar and a unitless spinner in the same
/// scope) and looks a lot less odd than an arbitrary block of blank space
/// forcing it to a fixed column. `width` remains a safety cap against a
/// pathologically long throughput suffix, not a target to pad up to.
pub(crate) fn cap_status(
    units: Option<Units>,
    pos: u64,
    total: Option<u64>,
    rate: Option<f64>,
    width: usize,
    ansi: bool,
    style: &Style,
) -> String {
    let plain = write_status_text(units, pos, total, rate, false, style);
    if UnicodeWidthStr::width(plain.as_str()) > width {
        return fit(&plain, width);
    }
    write_status_text(units, pos, total, rate, ansi, style)
}

// --- Template generation ---

const MIN_MSG_WIDTH: u16 = 10;

pub(crate) fn effective_indent(style: &Style, depth: u16) -> u16 {
    let max_indent = style.message_width.saturating_sub(MIN_MSG_WIDTH);
    (depth * 2).min(max_indent)
}

pub(crate) fn bar_template(style: &Style, depth: u16) -> String {
    let indent = effective_indent(style, depth);
    let iw = style.icon_width + indent;
    let mw = style.message_width - indent;
    let bw = style.bar_width;
    let ic = style.icon.to_template_suffix();
    let mc = style.message.to_template_suffix();
    let bc = style.bar.to_template_suffix_with_alt(&style.bar_alt);
    let ec = style.elapsed.to_template_suffix();
    // `status` deliberately carries no width here — see `cap_status` for
    // why it isn't padded to a fixed column the way `msg` is.
    format!("{{prefix:>{iw}{ic}}} {{msg:{mw}!{mc}}} {{bar:{bw}{bc}}} {{status}} {{elapsed:{ec}}}")
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
    const SW: usize = 1;

    // Leaf nodes show a spinner; non-leaf nodes hide it and give the space to the message.
    let (spinner_part, mw_extra) = if leaf {
        let sc = style.spinner.to_template_suffix();
        (format!(" {{spinner:>{sc}}}"), style.bar_width - SW as u16)
    } else {
        (format!("{:<SW$}", ""), style.bar_width)
    };
    let mw = style.message_width - indent + mw_extra;

    // A spinner has no total, so there's nothing for `status` to show
    // unless the caller at least declared units (a bare running count or
    // byte tally). No width here either — same reasoning as `bar_template`.
    let status_part = if units.is_some() {
        " {status}".to_string()
    } else {
        String::new()
    };

    format!("{{prefix:>{iw}{ic}}} {{msg:{mw}!{mc}}}{spinner_part}{status_part} {{elapsed:{ec}}}")
}

// --- Style application helpers ---

/// Registers the `status` custom template key shared by [`apply_bar_style`]
/// and [`apply_spinner_style`] — harmless to attach even when the template
/// string doesn't reference `{status}` (spinner with no units).
fn with_status_key(
    s: ix::ProgressStyle,
    style: &Style,
    units: Option<Units>,
    ansi: bool,
) -> ix::ProgressStyle {
    let style = style.clone();
    let status_width = style.status_width as usize;
    s.with_key(
        "status",
        move |state: &ix::ProgressState, w: &mut dyn std::fmt::Write| {
            let text = cap_status(
                units,
                state.pos(),
                state.len(),
                Some(state.per_sec()),
                status_width,
                ansi,
                &style,
            );
            let _ = w.write_str(&text);
        },
    )
}

pub(crate) fn apply_bar_style(
    bar: &ix::ProgressBar,
    style: &Style,
    depth: u16,
    units: Option<Units>,
    ansi: bool,
) {
    let tmpl = bar_template(style, depth);
    let s = ix::ProgressStyle::with_template(&tmpl)
        .expect("valid bar template")
        .progress_chars(BAR_CHARS);
    let s = with_status_key(s, style, units, ansi);
    bar.set_style(s);
}

pub(crate) fn apply_spinner_style(
    bar: &ix::ProgressBar,
    style: &Style,
    depth: u16,
    units: Option<Units>,
    leaf: bool,
    ansi: bool,
) {
    let tmpl = spinner_template(style, depth, units, leaf);
    let s = ix::ProgressStyle::with_template(&tmpl).expect("valid spinner template");
    let s = with_status_key(s, style, units, ansi);
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
    pub(crate) status: Sym<'v, 'v>,
    pub(crate) width: Sym<'v, 'v>,
    pub(crate) fg: Sym<'v, 'v>,
    pub(crate) bg: Sym<'v, 'v>,
    pub(crate) attrs: Sym<'v, 'v>,
    pub(crate) alt: Sym<'v, 'v>,
    pub(crate) colors: ColorKeys<'v>,
    pub(crate) attributes: AttrKeys<'v>,
}

#[derive(Clone, Copy)]
pub(crate) struct ColorKeys<'v> {
    pub(crate) values: [(Sym<'v, 'v>, Color); 17],
}

impl<'v> ColorKeys<'v> {
    fn get<'a>(self, value: Sym<'v, 'a>) -> Option<Color>
    where
        'v: 'a,
    {
        self.values
            .binary_search_by_key(&value, |(symbol, _)| -> Sym<'v, 'a> { *symbol })
            .ok()
            .map(|index| self.values[index].1)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct AttrKeys<'v> {
    pub(crate) values: [(Sym<'v, 'v>, Attr); 8],
}

impl<'v> AttrKeys<'v> {
    fn get<'a>(self, value: Sym<'v, 'a>) -> Option<Attr>
    where
        'v: 'a,
    {
        self.values
            .binary_search_by_key(&value, |(symbol, _)| -> Sym<'v, 'a> { *symbol })
            .ok()
            .map(|index| self.values[index].1)
    }
}

fn unknown_key_error<'v, 's>(strand: &mut Strand<'v, 's>, sym: Sym<'v, '_>) -> Error<'v, 's> {
    Error::value(
        strand,
        format!("style: unknown key: {}", sym.as_str(strand)),
    )
}

fn as_style_dict<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    val: &'a Value<'v>,
) -> Result<'v, 's, dolang::runtime::value::Dict<'v, 'a>> {
    val.as_dict(strand)
        .ok_or_else(|| Error::type_error(strand, "style: expected `Dict`"))
}

fn parse_attrs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    val: &Value<'v>,
    keys: AttrKeys<'v>,
) -> Result<'v, 's, Vec<Attr>> {
    let arr = val
        .as_array(strand.vm())
        .ok_or_else(|| Error::type_error(strand, "style: attrs: expected Array"))?;
    let len = arr.len(strand)?;
    let mut attrs = Vec::with_capacity(len);
    for i in 0..len {
        strand.with_slots_sync(|strand, [mut elem]| {
            arr.get(strand, i, &mut elem)?;
            let sym = elem
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "style: attrs: expected `Sym` element"))?;
            let attr = keys.get(sym).ok_or_else(|| {
                Error::value(
                    strand,
                    format!("style: attrs: unknown attribute: {}", sym.as_str(strand)),
                )
            })?;
            attrs.push(attr);
            Ok(())
        })?;
    }
    Ok(attrs)
}

/// Parses a plain element-style dict — `fg`, `bg`, `attrs` only. Used for
/// `spinner`/`elapsed`/`position`/`total`, and for `bar.alt`. Iterates the
/// dict's actual entries (rather than probing for expected keys) so an
/// unrecognized key is caught as an error instead of silently ignored.
fn parse_element_style<'v, 's>(
    strand: &mut Strand<'v, 's>,
    cat: &Value<'v>,
    keys: &StyleKeys<'v>,
    es: &mut ElementStyle,
) -> Result<'v, 's, ()> {
    let dict = as_style_dict(strand, cat)?;
    let mut pairs = dict.pairs();
    strand.with_slots_sync(|strand, [mut key, mut val]| {
        while pairs.next(strand, &mut key, &mut val)? {
            let sym = key
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "style: expected `Sym` key"))?;
            if sym == keys.fg {
                es.fg = Some(parse_color_value(strand, &val, "fg", keys.colors)?);
            } else if sym == keys.bg {
                es.bg = Some(parse_color_value(strand, &val, "bg", keys.colors)?);
            } else if sym == keys.attrs {
                es.attrs = parse_attrs(strand, &val, keys.attributes)?;
            } else {
                return Err(unknown_key_error(strand, sym));
            }
        }
        Ok(())
    })
}

/// Parses a width+color category dict — `width`, `fg`, `bg`, `attrs`, and
/// (bar only, when `alt` is `Some`) `alt`. Used for `bar`/`message`/`icon`.
fn parse_width_category<'v, 's>(
    strand: &mut Strand<'v, 's>,
    cat: &Value<'v>,
    keys: &StyleKeys<'v>,
    width: &mut u16,
    es: &mut ElementStyle,
    mut alt: Option<&mut ElementStyle>,
) -> Result<'v, 's, ()> {
    let dict = as_style_dict(strand, cat)?;
    let mut pairs = dict.pairs();
    strand.with_slots_sync(|strand, [mut key, mut val]| {
        while pairs.next(strand, &mut key, &mut val)? {
            let sym = key
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "style: expected `Sym` key"))?;
            if sym == keys.width {
                let n = val
                    .to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "style: width: expected `Int`"))?;
                *width = n as u16;
            } else if sym == keys.fg {
                es.fg = Some(parse_color_value(strand, &val, "fg", keys.colors)?);
            } else if sym == keys.bg {
                es.bg = Some(parse_color_value(strand, &val, "bg", keys.colors)?);
            } else if sym == keys.attrs {
                es.attrs = parse_attrs(strand, &val, keys.attributes)?;
            } else if sym == keys.alt
                && let Some(a) = alt.as_deref_mut()
            {
                parse_element_style(strand, &val, keys, a)?;
            } else {
                return Err(unknown_key_error(strand, sym));
            }
        }
        Ok(())
    })
}

/// Parses a width-only category dict — `width` only. Used for `status`,
/// which has no color of its own (it's rendered from the `position`/`total`
/// element styles).
fn parse_width_only<'v, 's>(
    strand: &mut Strand<'v, 's>,
    cat: &Value<'v>,
    keys: &StyleKeys<'v>,
    width: &mut u16,
) -> Result<'v, 's, ()> {
    let dict = as_style_dict(strand, cat)?;
    let mut pairs = dict.pairs();
    strand.with_slots_sync(|strand, [mut key, mut val]| {
        while pairs.next(strand, &mut key, &mut val)? {
            let sym = key
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "style: expected `Sym` key"))?;
            if sym == keys.width {
                let n = val
                    .to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "style: width: expected `Int`"))?;
                *width = n as u16;
            } else {
                return Err(unknown_key_error(strand, sym));
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

    let dict = as_style_dict(strand, style_val)?;
    let mut pairs = dict.pairs();
    strand.with_slots_sync(|strand, [mut key, mut val]| {
        while pairs.next(strand, &mut key, &mut val)? {
            let sym = key
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "style: expected `Sym` key"))?;
            if sym == keys.bar {
                parse_width_category(
                    strand,
                    &val,
                    keys,
                    &mut style.bar_width,
                    &mut style.bar,
                    Some(&mut style.bar_alt),
                )?;
            } else if sym == keys.message {
                parse_width_category(
                    strand,
                    &val,
                    keys,
                    &mut style.message_width,
                    &mut style.message,
                    None,
                )?;
            } else if sym == keys.icon {
                parse_width_category(
                    strand,
                    &val,
                    keys,
                    &mut style.icon_width,
                    &mut style.icon,
                    None,
                )?;
            } else if sym == keys.spinner {
                parse_element_style(strand, &val, keys, &mut style.spinner)?;
            } else if sym == keys.elapsed {
                parse_element_style(strand, &val, keys, &mut style.elapsed)?;
            } else if sym == keys.position {
                parse_element_style(strand, &val, keys, &mut style.position)?;
            } else if sym == keys.total {
                parse_element_style(strand, &val, keys, &mut style.total)?;
            } else if sym == keys.status {
                parse_width_only(strand, &val, keys, &mut style.status_width)?;
            } else {
                return Err(unknown_key_error(strand, sym));
            }
        }
        Ok(())
    })?;

    Ok(style)
}
