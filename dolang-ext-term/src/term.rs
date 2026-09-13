use dolang::runtime::object::fmt;
use dolang::runtime::value::fmt::{self as fmt_spec, Fill, Format, Kind, Pad, Spec};
use dolang::{
    compile::Config,
    runtime::{
        Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Value,
        method,
        object::{Mut, Ref, TypeBuilder},
        strand::Redirect,
        unpack,
        value::{Singleton, StrEmbryo, TypeObject, View},
        vm::Builder,
    },
};

use crate::{
    console::{self, DefaultOutput, SubConsole},
    global::Global,
    util::{Framing, strip_line_ending},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// Runs `f` with `console` installed as the ambient console for this strand.
///
/// The override lives in strand-local GC roots, so it is inherited by strands
/// spawned inside `f` and restored on every path out.
///
/// `can_style` and `line_ending` are the answers the console gave when it was
/// handed over; they are snapshotted rather than re-read, which is what fixes
/// them for the life of the capture and spares every `echo` a dispatch.
async fn with_capture<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    console: &Slot<'v, '_>,
    can_style: bool,
    line_ending: &Slot<'v, '_>,
    f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> R,
) -> R {
    strand
        .with_slots(async move |strand, [mut prev, mut prev_ending]| {
            let mut root = global.capture.slot(strand);
            Output::set(strand, &mut prev, &root);
            Output::set(strand, &mut root, console);
            let mut ending = global.capture_line_ending.slot(strand);
            Output::set(strand, &mut prev_ending, &ending);
            Output::set(strand, &mut ending, line_ending);
            let prev_can_style = global.local.get(strand).set_capture_can_style(can_style);
            let result = f(strand).await;
            let mut root = global.capture.slot(strand);
            Output::set(strand, &mut root, &prev);
            let mut ending = global.capture_line_ending.slot(strand);
            Output::set(strand, &mut ending, &prev_ending);
            global
                .local
                .get(strand)
                .set_capture_can_style(prev_can_style);
            result
        })
        .await
}

const BOLD: usize = 0;
const DIM: usize = 1;
const ITALIC: usize = 2;
const UNDERLINE: usize = 3;
const BLINK: usize = 4;
const REVERSE: usize = 5;
const HIDDEN: usize = 6;
const STRIKETHROUGH: usize = 7;
const ATTR_COUNT: usize = 8;
const SPACE_CHUNK: &str = "                                                                ";

const fn index_to_code(index: usize) -> u8 {
    match index {
        BOLD => 1,
        DIM => 2,
        ITALIC => 3,
        UNDERLINE => 4,
        BLINK => 5,
        REVERSE => 7,
        HIDDEN => 8,
        STRIKETHROUGH => 9,
        _ => unreachable!(),
    }
}

fn write_spaces<'v, 's>(
    strand: &mut Strand<'v, 's>,
    out: &mut dyn Format<'v>,
    mut count: usize,
) -> Result<'v, 's, ()> {
    while count != 0 {
        let chunk = count.min(SPACE_CHUNK.len());
        out.write_str(strand, &SPACE_CHUNK[..chunk])?;
        count -= chunk;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Color {
    Ansi(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Style {
    fg: Option<Color>,
    bg: Option<Color>,
    attrs: [bool; ATTR_COUNT],
}

impl Style {
    fn write<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        if self == Self::default() {
            return Ok(());
        }

        out.write_str(strand, "\x1b[")?;
        let mut first = true;
        for (index, enabled) in self.attrs.into_iter().enumerate() {
            if enabled {
                write_separator(strand, out, &mut first)?;
                fmt!(strand, out, "{}", index_to_code(index))?;
            }
        }
        if let Some(color) = self.fg {
            write_separator(strand, out, &mut first)?;
            write_color_params(strand, out, color, true)?;
        }
        if let Some(color) = self.bg {
            write_separator(strand, out, &mut first)?;
            write_color_params(strand, out, color, false)?;
        }
        out.write_str(strand, "m")
    }

    fn write_attr<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
        attr: usize,
    ) -> Result<'v, 's, ()> {
        if self.attrs[attr] {
            let code = index_to_code(attr);
            fmt!(strand, out, "\x1b[{code}m")?;
        }
        Ok(())
    }

    fn apply(&mut self, op: Sgr) {
        match op {
            Sgr::Reset => *self = Self::default(),
            Sgr::AttrOn(attr) => self.attrs[attr] = true,
            Sgr::AttrOff(attr, _) => self.attrs[attr] = false,
            Sgr::IntensityOff => {
                self.attrs[BOLD] = false;
                self.attrs[DIM] = false;
            }
            Sgr::Fg(color) => self.fg = color,
            Sgr::Bg(color) => self.bg = color,
        }
    }
}

fn write_separator<'v, 's>(
    strand: &mut Strand<'v, 's>,
    out: &mut dyn Format<'v>,
    first: &mut bool,
) -> Result<'v, 's, ()> {
    if *first {
        *first = false;
        Ok(())
    } else {
        out.write_str(strand, ";")
    }
}

fn write_color_params<'v, 's>(
    strand: &mut Strand<'v, 's>,
    out: &mut dyn Format<'v>,
    color: Color,
    foreground: bool,
) -> Result<'v, 's, ()> {
    match color {
        Color::Ansi(value @ 0..=7) => fmt!(
            strand,
            out,
            "{}",
            (if foreground { 30 } else { 40 }) + value
        ),
        Color::Ansi(value) => fmt!(
            strand,
            out,
            "{}",
            (if foreground { 90 } else { 100 }) + value - 8
        ),
        Color::Indexed(value) => fmt!(
            strand,
            out,
            "{};5;{value}",
            if foreground { 38 } else { 48 }
        ),
        Color::Rgb(r, g, b) => fmt!(
            strand,
            out,
            "{};2;{r};{g};{b}",
            if foreground { 38 } else { 48 }
        ),
    }
}

pub(crate) struct Text;

impl<'v> Object<'v> for Text {
    const NAME: &'v str = "Text";
    const MODULE: &'v str = "term";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    /// Converts to a plain string, dropping the styling.
    ///
    /// The escape sequences are terminal instructions, not content: a `Str`
    /// carrying them counts them in its length, matches them in a search, and
    /// writes them into whatever file or pipe it reaches. Producing the
    /// content and leaving the encoding to `encode` keeps the common
    /// conversion the safe one — and `verbatim` falls through to here, since
    /// styled text has no source form to reproduce.
    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
        let mut filter = Filter::new(out, FilterMode::Plain);
        filter.write_str(strand, &text)?;
        filter.finish(strand)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
        dolang::runtime::object::fmt!(strand, out, "<term.Text {:?}>", &*text)
    }

    /// Formats according to `spec`, measuring in terminal cells rather than
    /// grapheme clusters.
    ///
    /// The default implementation would measure and clip the encoded form,
    /// counting the bytes of every escape sequence and potentially cutting one
    /// in half. Rendering first and then applying the specification with
    /// [`encoded_width`]/[`clip_encoding`] measures what the terminal will
    /// actually display and keeps the styling well-formed.
    fn fmt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let kind = spec
            .kind
            .ok_or_else(|| Error::type_error(strand, "unresolved format kind"))?;
        if spec.sign.is_some() || spec.alt || spec.fill == Fill::Zero {
            return Err(Error::type_error(strand, "unsupported format option"));
        }
        let mut rendered = String::new();
        match kind {
            Kind::Str | Kind::Verbatim => Self::display(this, strand, &mut rendered)?,
            Kind::Dbg => Self::debug(this, strand, &mut rendered)?,
            _ => return Err(Error::type_error(strand, "unsupported format option")),
        }
        lay_out(strand, spec, &rendered, w)
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let suffix_sym = builder.sym("suffix");
        builder
            .method("encode", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
                let mut encoded = StrEmbryo::new();
                encoded.write_str(strand, &text)?;
                drop(text);
                drop(borrow);
                encoded.finish(strand, out);
                Ok(())
            })
            .method("width", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
                let width = TextLayout::new(&text).width();
                drop(text);
                drop(borrow);
                Output::set(strand, out, width);
                Ok(())
            })
            .method("clip", async move |this, strand, args, out| {
                let ([width], [suffix]) = unpack!(strand, args, 1, 0, suffix_sym = None)?;
                let width = width.to_usize(strand)?;

                let borrow = this.borrow(strand)?;
                let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
                let source = String::from(&*text);
                let source_width = TextLayout::new(&source).width();
                drop(text);
                drop(borrow);
                if source_width <= width {
                    Output::set(strand, out, this);
                    return Ok(());
                }

                let global = strand.state::<Global<'v>>();
                let suffix = suffix
                    .as_ref()
                    .map(|suffix| text_encoding(strand, global, suffix))
                    .transpose()?
                    .unwrap_or_default();
                let suffix = clip_encoding(&suffix, width);
                let suffix_width = TextLayout::new(&suffix).width();
                let prefix = clip_encoding(&source, width.saturating_sub(suffix_width));

                let mut clipped = StrEmbryo::new();
                clipped.write_str(strand, &prefix)?;
                clipped.write_str(strand, &suffix)?;
                create_text(strand, global, clipped, out);
                Ok(())
            })
            .method("indent", async move |this, strand, args, out| {
                let ([spaces], []) = unpack!(strand, args, 1, 0)?;
                let spaces = spaces.to_usize(strand)?;
                if spaces == 0 {
                    Output::set(strand, out, this);
                    return Ok(());
                }

                let borrow = this.borrow(strand)?;
                let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
                if text.is_empty() {
                    Output::set(strand, out, this);
                    return Ok(());
                }

                let mut indented = StrEmbryo::new();
                for line in text.split_inclusive('\n') {
                    write_spaces(strand, &mut indented, spaces)?;
                    indented.write_str(strand, line)?;
                }
                drop(text);
                drop(borrow);
                let global = strand.state::<Global<'v>>();
                create_text(strand, global, indented, out);
                Ok(())
            })
            .method_with_slots(
                "join",
                async move |this, strand, args, out, [mut items, mut item]| {
                    let ([], [source]) = unpack!(strand, args, 0, 1)?;
                    match source {
                        Some(source) => source.iter(strand, &mut items).await?,
                        None => strand.input(&mut items),
                    }

                    let borrow = this.borrow(strand)?;
                    let separator = Ref::slot::<0>(&borrow).as_str(strand).unwrap().to_string();
                    drop(borrow);

                    let global = strand.state::<Global<'v>>();
                    let mut joined = StrEmbryo::new();
                    let mut first = true;
                    while items.next(strand, &mut item).await? {
                        if !first {
                            joined.write_str(strand, &separator)?;
                        }
                        first = false;
                        // Each value is taken the way `text` takes an argument
                        // of its own: styling kept where there is any, and
                        // everything else converted and sanitized.
                        append_value(
                            strand,
                            global,
                            &mut joined,
                            Style::default(),
                            true,
                            false,
                            &item,
                        )?;
                        strand.check_trap_gc()?;
                    }
                    create_text(strand, global, joined, out);
                    Ok(())
                },
            )
    }
}

pub(crate) struct StyleObject;

#[derive(Clone, Copy)]
pub(crate) struct StyleAnnex<'v> {
    global: State<'v, Global<'v>>,
    keys: StyleKeys<'v>,
    style: Style,
}

impl<'v> Object<'v> for StyleObject {
    const NAME: &'v str = "Style";
    const MODULE: &'v str = "term";
    type Annex = StyleAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        _this: dolang::runtime::Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let keys = global.style_keys;
        make_style(strand, global, keys, Style::default(), args, out)
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        apply_style(strand, annex.global, annex.keys, annex.style, args, out)
    }
}

#[derive(Clone, Copy)]
enum FilterMode {
    Plain,
    Preformat,
    Child(Style),
}

enum ScanState {
    Ground,
    Esc,
    EscIntermediate,
    Csi(String),
    StringControl { esc: bool },
}

struct Filter<'a, 'v> {
    out: &'a mut dyn Format<'v>,
    mode: FilterMode,
    state: ScanState,
    style: Style,
}

impl<'a, 'v> Filter<'a, 'v> {
    fn new(out: &'a mut dyn Format<'v>, mode: FilterMode) -> Self {
        Self {
            out,
            mode,
            state: ScanState::Ground,
            style: Style::default(),
        }
    }

    fn finish<'s>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        if matches!(self.mode, FilterMode::Preformat) && self.style != Style::default() {
            self.out.write_str(strand, "\x1b[0m")?;
        }
        Ok(())
    }

    fn write_ground<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        input: &str,
        start: &mut usize,
        at: usize,
        ch: char,
    ) -> Result<'v, 's, ()> {
        if ch == '\x1b' || ch.is_control() && ch != '\n' && ch != '\t' {
            if *start < at {
                self.out.write_str(strand, &input[*start..at])?;
            }
            self.state = match ch {
                '\x1b' => ScanState::Esc,
                '\u{009b}' => ScanState::Csi(String::new()),
                '\u{0090}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                    ScanState::StringControl { esc: false }
                }
                _ => ScanState::Ground,
            };
            *start = at + ch.len_utf8();
        }
        Ok(())
    }

    fn write_sgr<'s>(&mut self, strand: &mut Strand<'v, 's>, params: &str) -> Result<'v, 's, ()> {
        for op in SgrParser::new(params) {
            match self.mode {
                FilterMode::Plain => {}
                FilterMode::Preformat => {
                    write_sgr_op(strand, self.out, op)?;
                    self.style.apply(op);
                }
                FilterMode::Child(parent) => {
                    write_sgr_op(strand, self.out, op)?;
                    match op {
                        Sgr::Reset => parent.write(strand, self.out)?,
                        Sgr::AttrOff(attr, _) => {
                            parent.write_attr(strand, self.out, attr)?;
                        }
                        Sgr::IntensityOff => {
                            parent.write_attr(strand, self.out, BOLD)?;
                            parent.write_attr(strand, self.out, DIM)?;
                        }
                        Sgr::Fg(None) => {
                            if let Some(color) = parent.fg {
                                write_sgr_op(strand, self.out, Sgr::Fg(Some(color)))?;
                            }
                        }
                        Sgr::Bg(None) => {
                            if let Some(color) = parent.bg {
                                write_sgr_op(strand, self.out, Sgr::Bg(Some(color)))?;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }
}

impl<'v> Format<'v> for Filter<'_, 'v> {
    fn write_str<'s>(&mut self, strand: &mut Strand<'v, 's>, input: &str) -> Result<'v, 's, ()> {
        let mut start = 0;
        for (at, ch) in input.char_indices() {
            match &mut self.state {
                ScanState::Ground => self.write_ground(strand, input, &mut start, at, ch)?,
                ScanState::Esc => {
                    self.state = match ch {
                        '[' => ScanState::Csi(String::new()),
                        ']' | 'P' | 'X' | '^' | '_' => ScanState::StringControl { esc: false },
                        '\u{20}'..='\u{2f}' => ScanState::EscIntermediate,
                        _ => ScanState::Ground,
                    };
                    start = at + ch.len_utf8();
                }
                ScanState::EscIntermediate => {
                    if ('\u{30}'..='\u{7e}').contains(&ch) {
                        self.state = ScanState::Ground;
                    }
                    start = at + ch.len_utf8();
                }
                ScanState::Csi(params) => {
                    if ('\u{40}'..='\u{7e}').contains(&ch) {
                        let params = std::mem::take(params);
                        self.state = ScanState::Ground;
                        if ch == 'm' {
                            self.write_sgr(strand, &params)?;
                        }
                    } else {
                        params.push(ch);
                    }
                    start = at + ch.len_utf8();
                }
                ScanState::StringControl { esc } => {
                    if ch == '\u{0007}' || *esc && ch == '\\' {
                        self.state = ScanState::Ground;
                    } else {
                        *esc = ch == '\x1b';
                    }
                    start = at + ch.len_utf8();
                }
            }
        }
        if matches!(self.state, ScanState::Ground) && start < input.len() {
            self.out.write_str(strand, &input[start..])?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Sgr {
    Reset,
    AttrOn(usize),
    AttrOff(usize, u8),
    IntensityOff,
    Fg(Option<Color>),
    Bg(Option<Color>),
}

struct SgrParser<'a> {
    values: std::str::Split<'a, char>,
}

impl<'a> SgrParser<'a> {
    fn new(params: &'a str) -> Self {
        Self {
            values: params.split(';'),
        }
    }
}

fn parse_sgr_value(value: &str) -> Option<u16> {
    if value.is_empty() {
        Some(0)
    } else {
        value.parse().ok()
    }
}

impl Iterator for SgrParser<'_> {
    type Item = Sgr;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let raw = self.values.next()?;
            let Some(value) = parse_sgr_value(raw) else {
                continue;
            };
            let op = match value {
                0 => Some(Sgr::Reset),
                1 => Some(Sgr::AttrOn(BOLD)),
                2 => Some(Sgr::AttrOn(DIM)),
                3 => Some(Sgr::AttrOn(ITALIC)),
                4 => Some(Sgr::AttrOn(UNDERLINE)),
                5 | 6 => Some(Sgr::AttrOn(BLINK)),
                7 => Some(Sgr::AttrOn(REVERSE)),
                8 => Some(Sgr::AttrOn(HIDDEN)),
                9 => Some(Sgr::AttrOn(STRIKETHROUGH)),
                22 => Some(Sgr::IntensityOff),
                23 => Some(Sgr::AttrOff(ITALIC, 23)),
                24 => Some(Sgr::AttrOff(UNDERLINE, 24)),
                25 => Some(Sgr::AttrOff(BLINK, 25)),
                27 => Some(Sgr::AttrOff(REVERSE, 27)),
                28 => Some(Sgr::AttrOff(HIDDEN, 28)),
                29 => Some(Sgr::AttrOff(STRIKETHROUGH, 29)),
                30..=37 => Some(Sgr::Fg(Some(Color::Ansi((value - 30) as u8)))),
                39 => Some(Sgr::Fg(None)),
                40..=47 => Some(Sgr::Bg(Some(Color::Ansi((value - 40) as u8)))),
                49 => Some(Sgr::Bg(None)),
                90..=97 => Some(Sgr::Fg(Some(Color::Ansi((value - 90 + 8) as u8)))),
                100..=107 => Some(Sgr::Bg(Some(Color::Ansi((value - 100 + 8) as u8)))),
                38 | 48 => {
                    let mut values = self.values.clone();
                    let mode = values.next().and_then(parse_sgr_value);
                    let color = match mode {
                        Some(5) => values
                            .next()
                            .and_then(parse_sgr_value)
                            .and_then(|value| u8::try_from(value).ok())
                            .map(Color::Indexed),
                        Some(2) => (|| {
                            Some(Color::Rgb(
                                u8::try_from(parse_sgr_value(values.next()?)?).ok()?,
                                u8::try_from(parse_sgr_value(values.next()?)?).ok()?,
                                u8::try_from(parse_sgr_value(values.next()?)?).ok()?,
                            ))
                        })(),
                        _ => None,
                    };
                    if let Some(color) = color {
                        self.values = values;
                        Some(if value == 38 {
                            Sgr::Fg(Some(color))
                        } else {
                            Sgr::Bg(Some(color))
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if op.is_some() {
                return op;
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TextOffset {
    plain_end: usize,
    encoded_end: usize,
    style: Style,
}

struct TextLayout {
    plain: String,
    offsets: Vec<TextOffset>,
}

impl TextLayout {
    fn new(encoded: &str) -> Self {
        let mut plain = String::new();
        let mut offsets = Vec::new();
        let mut style = Style::default();
        let mut at = 0;
        while at < encoded.len() {
            if let Some((end, params)) = sgr_at(encoded, at) {
                for op in SgrParser::new(params) {
                    style.apply(op);
                }
                at = end;
                continue;
            }
            let ch = encoded[at..].chars().next().unwrap();
            at += ch.len_utf8();
            plain.push(ch);
            offsets.push(TextOffset {
                plain_end: plain.len(),
                encoded_end: at,
                style,
            });
        }
        Self { plain, offsets }
    }

    fn width(&self) -> usize {
        display_width(&self.plain)
    }

    fn prefix(&self, encoded: &str, width: usize) -> String {
        let mut used: usize = 0;
        let mut plain_end = 0;
        for grapheme in self.plain.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if used.saturating_add(grapheme_width) > width {
                break;
            }
            used += grapheme_width;
            plain_end += grapheme.len();
        }
        let Some(offset) = self
            .offsets
            .iter()
            .find(|offset| offset.plain_end == plain_end)
        else {
            return String::new();
        };
        let mut clipped = encoded[..offset.encoded_end].to_owned();
        if offset.style != Style::default() {
            clipped.push_str("\x1b[0m");
        }
        clipped
    }
}

fn sgr_at(value: &str, at: usize) -> Option<(usize, &str)> {
    let rest = value.get(at..)?;
    let body = rest.strip_prefix("\x1b[")?;
    for (offset, ch) in body.char_indices() {
        if ('\u{40}'..='\u{7e}').contains(&ch) {
            if ch != 'm' {
                return None;
            }
            let params_start = at + 2;
            let params_end = params_start + offset;
            return Some((params_end + ch.len_utf8(), &value[params_start..params_end]));
        }
    }
    None
}

fn display_width(value: &str) -> usize {
    value
        .graphemes(true)
        .map(|grapheme| {
            grapheme
                .chars()
                .map(|ch| {
                    if ('\0'..='\u{1f}').contains(&ch) {
                        0
                    } else {
                        UnicodeWidthChar::width(ch).unwrap_or(0)
                    }
                })
                .sum::<usize>()
                .min(2)
        })
        .sum()
}

/// Applies `spec`'s layout to already-rendered text, measuring terminal cells.
///
/// Shared by [`Text`]'s own formatting, which lays out its stripped content,
/// and by `echo`/`print`, which lay out the encoded form of a `FmtValue` bound to
/// a `Text`. Escape sequences measure zero either way, so the two agree on
/// every column.
fn lay_out<'v, 's>(
    strand: &mut Strand<'v, 's>,
    spec: &Spec,
    content: &str,
    out: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    let clipped;
    let content = match spec.precision {
        Some(precision) => {
            clipped = clip_encoding(content, precision as usize);
            clipped.as_str()
        }
        None => content,
    };
    // Precision is applied above: left to `Pad`, it would clip the already
    // clipped text a second time, splitting an escape sequence in half.
    let mut spec = *spec;
    spec.precision = None;
    let mut pad = Pad::with_measure(spec, out, encoded_width);
    pad.write_str(strand, content)?;
    pad.finish(strand)
}

/// Measures `encoded` in terminal cells, skipping its escape sequences.
fn encoded_width(encoded: &str) -> usize {
    TextLayout::new(encoded).width()
}

fn clip_encoding(encoded: &str, width: usize) -> String {
    let layout = TextLayout::new(encoded);
    if layout.width() <= width {
        encoded.to_owned()
    } else {
        layout.prefix(encoded, width)
    }
}

fn text_encoding<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, String> {
    if let Some(value) = value.as_str(strand) {
        let value = value.to_string();
        return filter_preformatted(strand, &value, false);
    }
    let Some(text) = global.types.text.cast(value) else {
        return Err(Error::type_error(
            strand,
            "suffix: expected `Str` or `term.Text`",
        ));
    };
    text.enter_sync(strand, |strand, text| {
        let borrow = text.borrow(strand)?;
        let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
        Ok(String::from(&*text))
    })
}

fn write_sgr_op<'v, 's>(
    strand: &mut Strand<'v, 's>,
    out: &mut dyn Format<'v>,
    op: Sgr,
) -> Result<'v, 's, ()> {
    match op {
        Sgr::Reset => out.write_str(strand, "\x1b[0m"),
        Sgr::AttrOn(index) => {
            let code = index_to_code(index);
            fmt!(strand, out, "\x1b[{code}m")
        }
        Sgr::AttrOff(_, code) => {
            fmt!(strand, out, "\x1b[{code}m")
        }
        Sgr::IntensityOff => out.write_str(strand, "\x1b[22m"),
        Sgr::Fg(None) => out.write_str(strand, "\x1b[39m"),
        Sgr::Bg(None) => out.write_str(strand, "\x1b[49m"),
        Sgr::Fg(Some(color)) => write_color(strand, out, color, true),
        Sgr::Bg(Some(color)) => write_color(strand, out, color, false),
    }
}

fn write_color<'v, 's>(
    strand: &mut Strand<'v, 's>,
    out: &mut dyn Format<'v>,
    color: Color,
    foreground: bool,
) -> Result<'v, 's, ()> {
    match color {
        Color::Ansi(value @ 0..=7) => fmt!(
            strand,
            out,
            "\x1b[{}m",
            (if foreground { 30 } else { 40 }) + value
        ),
        Color::Ansi(value) => fmt!(
            strand,
            out,
            "\x1b[{}m",
            (if foreground { 90 } else { 100 }) + value - 8
        ),
        Color::Indexed(value) => fmt!(
            strand,
            out,
            "\x1b[{};5;{value}m",
            if foreground { 38 } else { 48 }
        ),
        Color::Rgb(r, g, b) => fmt!(
            strand,
            out,
            "\x1b[{};2;{r};{g};{b}m",
            if foreground { 38 } else { 48 }
        ),
    }
}

/// Renders one value for the console.
///
/// Three shapes carry terminal structure and are walked rather than converted:
/// a [`Text`] is written with its styling rewritten relative to `parent`, a
/// `Fmt` is walked segment by segment, and a `FmtValue` bound to either of
/// those is rendered by this same walk and then laid out. Everything else is
/// converted — `verbatim` in argument position, `display` inside a `Text` —
/// and sanitized.
///
/// Walking, rather than converting, is what lets styling survive interpolation:
/// a `Text` inside a `t"..."` inside another still arrives here as a `Text`,
/// with every enclosing specification laying out what it rendered to.
fn append_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    out: &mut dyn Format<'v>,
    parent: Style,
    ansi: bool,
    argument: bool,
    value: &Value<'v>,
) -> Result<'v, 's, ()> {
    let mode = if ansi {
        FilterMode::Child(parent)
    } else {
        FilterMode::Plain
    };
    if let Some(text) = global.types.text.cast(value) {
        return text.enter_sync(strand, |strand, text| {
            let borrow = text.borrow(strand)?;
            let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
            let mut filter = Filter::new(out, mode);
            filter.write_str(strand, &text)?;
            filter.finish(strand)
        });
    }
    if value.is_instance_of(strand, TypeObject::Fmt) {
        return append_segments(strand, global, out, parent, ansi, argument, value);
    }
    if let Some(encoded) = bound_layout(strand, global, ansi, argument, value)? {
        let mut filter = Filter::new(out, mode);
        filter.write_str(strand, &encoded)?;
        return filter.finish(strand);
    }
    let mut filter = Filter::new(out, FilterMode::Plain);
    if argument {
        value.verbatim(strand, &mut filter)?;
    } else {
        value.display(strand, &mut filter)?;
    }
    filter.finish(strand)
}

/// Appends each segment of a `Fmt` in turn.
///
/// Expanding a sequence is the console's own decision, not a conversion: no
/// conversion expands one, precisely so that a consumer which cannot act on
/// the segments never receives them already flattened. The console can act on
/// them — that is what this walk is — so it expands, and each segment is taken
/// exactly as an argument in its own right would be.
fn append_segments<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    out: &mut dyn Format<'v>,
    parent: Style,
    ansi: bool,
    argument: bool,
    value: &Value<'v>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut len, mut segment, mut name]| {
        value.get(strand, global.syms.len, &mut len)?;
        let len = len.to_usize(strand)?;
        for index in 0..len {
            value.index(strand, index, &mut segment)?;
            // A parameter shows itself readily enough on its own, but one
            // still standing in a sequence means the template was never
            // finished. `Fmt.format()` refuses that, and the console agrees
            // rather than printing a hole where a value was meant to go.
            if segment.is_instance_of(strand, TypeObject::FmtParam) {
                segment.get(strand, global.syms.name, &mut name)?;
                // A hole nobody filled is a parameter nobody supplied, and is
                // reported as one: positionally when its name is an integer,
                // by key otherwise.
                return Err(
                    match name.as_int(strand).and_then(|i| usize::try_from(i).ok()) {
                        Some(index) => Error::missing_positional(strand, index),
                        None => Error::missing_key(strand, &name),
                    },
                );
            }
            append_value(strand, global, out, parent, ansi, argument, &segment)?;
        }
        Ok(())
    })
}

/// Lays out a `FmtValue` bound to something the console renders structurally —
/// a [`Text`] or a `Fmt` — or returns `None` when `value` is neither.
///
/// A `FmtValue` renders through the bound value's own conversions, and neither
/// of those two converts the way the console needs: a `Text` converts to its
/// content, which is right for a `Str` and wrong for the console, and a
/// sequence refuses to convert at all. So the console renders the bound value
/// by the same walk and applies the layout itself, in the same terminal cells
/// [`Text`] measures itself in.
///
/// A specification asking for something else — a debug or numeric rendering —
/// is left to the ordinary path: it is no longer a request for terminal
/// presentation.
fn bound_layout<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    ansi: bool,
    argument: bool,
    value: &Value<'v>,
) -> Result<'v, 's, Option<String>> {
    if !value.is_instance_of(strand, TypeObject::FmtValue) {
        return Ok(None);
    }
    let spec = fmt_spec::spec_of(strand, value)?;
    if !matches!(spec.kind, None | Some(Kind::Str) | Some(Kind::Verbatim))
        || spec.sign.is_some()
        || spec.alt
        || spec.fill == Fill::Zero
    {
        return Ok(None);
    }
    strand.with_slots_sync(|strand, [mut bound]| {
        value.get(strand, global.syms.value, &mut bound)?;
        if global.types.text.cast(&bound).is_none()
            && !bound.is_instance_of(strand, TypeObject::Fmt)
        {
            return Ok(None);
        }
        // Rendered as it would be standing alone — styling stated in full — so
        // that the caller can rewrite it against whatever style encloses it.
        let mut encoded = String::new();
        append_value(
            strand,
            global,
            &mut encoded,
            Style::default(),
            ansi,
            argument,
            &bound,
        )?;
        let mut laid_out = String::new();
        lay_out(strand, &spec, &encoded, &mut laid_out)?;
        Ok(Some(laid_out))
    })
}

fn append_key<'v, 's>(
    strand: &mut Strand<'v, 's>,
    out: &mut dyn Format<'v>,
    key: Sym<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut filter = Filter::new(out, FilterMode::Plain);
    filter.write_str(strand, key.as_str(strand))?;
    filter.finish(strand)
}

#[derive(Clone, Copy)]
pub(crate) struct StyleKeys<'v> {
    fg: Sym<'v, 'v>,
    bg: Sym<'v, 'v>,
    attrs: [Sym<'v, 'v>; ATTR_COUNT],
    colors: ColorKeys<'v>,
    inherit: Sym<'v, 'v>,
}

#[derive(Clone, Copy)]
pub(crate) struct ColorKeys<'v> {
    values: [(Sym<'v, 'v>, Color); 16],
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

fn color<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<Slot<'v, '_>>,
    name: &str,
    colors: ColorKeys<'v>,
    inherit: Sym<'v, 'v>,
    base: Option<Color>,
) -> Result<'v, 's, Option<Color>> {
    let Some(value) = value else {
        return Ok(base);
    };
    match value.view(strand) {
        View::Sym(value) if value == inherit => Ok(None),
        View::Sym(value) => colors
            .get(value)
            .map(Some)
            .ok_or_else(|| Error::value(strand, format!("{name}: unknown color"))),
        View::Int(value) => u8::try_from(value)
            .map(Color::Indexed)
            .map(Some)
            .map_err(|_| Error::value(strand, format!("{name}: color index out of range"))),
        View::Array(value) => {
            if value.len(strand)? != 3 {
                return Err(Error::value(
                    strand,
                    format!("{name}: expected 3 color components"),
                ));
            }
            strand.with_slots_sync(|strand, [mut red, mut green, mut blue]| {
                value.get(strand, 0, &mut red)?;
                value.get(strand, 1, &mut green)?;
                value.get(strand, 2, &mut blue)?;
                parse_color_components(strand, name, [&red, &green, &blue]).map(Some)
            })
        }
        View::Tuple(value) => {
            if value.len() != 3 {
                return Err(Error::value(
                    strand,
                    format!("{name}: expected 3 color components"),
                ));
            }
            strand.with_slots_sync(|strand, [mut red, mut green, mut blue]| {
                value.get(strand, 0, &mut red)?;
                value.get(strand, 1, &mut green)?;
                value.get(strand, 2, &mut blue)?;
                parse_color_components(strand, name, [&red, &green, &blue]).map(Some)
            })
        }
        _ => Err(Error::type_error(
            strand,
            format!("{name}: expected Sym, int, array, or Tuple"),
        )),
    }
}

fn parse_color_components<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    values: [&Value<'v>; 3],
) -> Result<'v, 's, Color> {
    let mut components = [0; 3];
    for (out, value) in components.iter_mut().zip(values) {
        let value = value.as_int(strand).ok_or_else(|| {
            Error::type_error(strand, format!("{name}: RGB components must be int"))
        })?;
        *out = u8::try_from(value)
            .map_err(|_| Error::value(strand, format!("{name}: RGB component out of range")))?;
    }
    let [red, green, blue] = components;
    Ok(Color::Rgb(red, green, blue))
}

fn attr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<Slot<'v, '_>>,
    name: &'static str,
    inherit: Sym<'v, 'v>,
    base: bool,
) -> Result<'v, 's, bool> {
    match value {
        None => Ok(base),
        Some(value) if value.as_bool(strand) == Some(true) => Ok(true),
        Some(value) if value.as_sym(strand) == Some(inherit) => Ok(false),
        Some(_) => Err(Error::value(
            strand,
            format!("{name}: expected true or :INHERIT:"),
        )),
    }
}

fn create_text<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    text: StrEmbryo<'v>,
    mut out: Slot<'v, '_>,
) {
    global.types.text.create(strand, Text, &mut out);
    global
        .types
        .text
        .cast(&out)
        .unwrap()
        .enter_sync(strand, |strand, this| {
            text.finish(strand, Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()));
        });
}

pub(crate) fn create_preformatted_text<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &str,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut text = StrEmbryo::new();
    let mut filter = Filter::new(&mut text, FilterMode::Preformat);
    filter.write_str(strand, value)?;
    filter.finish(strand)?;
    create_text(strand, global, text, out);
    Ok(())
}

pub(crate) fn filter_preformatted<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &str,
    ansi: bool,
) -> Result<'v, 's, String> {
    let mut output = String::new();
    let mode = if ansi {
        FilterMode::Preformat
    } else {
        FilterMode::Plain
    };
    let mut filter = Filter::new(&mut output, mode);
    filter.write_str(strand, value)?;
    filter.finish(strand)?;
    Ok(output)
}

fn create_style<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    keys: StyleKeys<'v>,
    style: Style,
    mut out: Slot<'v, '_>,
) {
    global.types.style.create_with_annex(
        strand,
        StyleObject,
        StyleAnnex {
            global,
            keys,
            style,
        },
        &mut out,
    );
}

fn render_args<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    out: &mut dyn Format<'v>,
    style: Style,
    ansi: bool,
    args: impl Iterator<Item = Arg<'v, 'a>>,
) -> Result<'v, 's, ()>
where
    'v: 'a,
{
    if ansi {
        style.write(strand, out)?;
    }
    for arg in args {
        match arg {
            Arg::Pos(value) => append_value(strand, global, out, style, ansi, false, &value)?,
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        }
    }
    if ansi && style != Style::default() {
        out.write_str(strand, "\x1b[0m")?;
    }
    Ok(())
}

/// Resolves the style options in `args` against `base`, returning the
/// resolved style and the positional arguments left over.
fn parse_style<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    keys: StyleKeys<'v>,
    base: Style,
    args: Args<'v, 'a>,
) -> Result<'v, 's, (Style, Args<'v, 'a>)>
where
    'v: 'a,
{
    let StyleKeys {
        fg,
        bg,
        attrs:
            [
                bold,
                dim,
                italic,
                underline,
                blink,
                reverse,
                hidden,
                strikethrough,
            ],
        colors,
        inherit,
    } = keys;
    let (
        [],
        [
            fg_value,
            bg_value,
            bold_value,
            dim_value,
            italic_value,
            underline_value,
            blink_value,
            reverse_value,
            hidden_value,
            strikethrough_value,
        ],
        args,
    ) = unpack!(
        strand,
        args,
        0,
        0,
        fg = None,
        bg = None,
        bold = None,
        dim = None,
        italic = None,
        underline = None,
        blink = None,
        reverse = None,
        hidden = None,
        strikethrough = None,
        ...
    )?;
    let style = Style {
        fg: color(strand, fg_value, "fg", colors, inherit, base.fg)?,
        bg: color(strand, bg_value, "bg", colors, inherit, base.bg)?,
        attrs: [
            attr(strand, bold_value, "bold", inherit, base.attrs[BOLD])?,
            attr(strand, dim_value, "dim", inherit, base.attrs[DIM])?,
            attr(strand, italic_value, "italic", inherit, base.attrs[ITALIC])?,
            attr(
                strand,
                underline_value,
                "underline",
                inherit,
                base.attrs[UNDERLINE],
            )?,
            attr(strand, blink_value, "blink", inherit, base.attrs[BLINK])?,
            attr(
                strand,
                reverse_value,
                "reverse",
                inherit,
                base.attrs[REVERSE],
            )?,
            attr(strand, hidden_value, "hidden", inherit, base.attrs[HIDDEN])?,
            attr(
                strand,
                strikethrough_value,
                "strikethrough",
                inherit,
                base.attrs[STRIKETHROUGH],
            )?,
        ],
    };
    Ok((style, args))
}

/// Builds a [`Text`] from `args`, whether or not any styling is applied.
fn make_text<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    keys: StyleKeys<'v>,
    base: Style,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    'v: 'a,
{
    let (style, args) = parse_style(strand, keys, base, args)?;
    let mut text = StrEmbryo::new();
    render_args(strand, global, &mut text, style, true, args)?;
    create_text(strand, global, text, out);
    Ok(())
}

/// Builds a reusable [`Style`](StyleObject) from `args`, which are keywords
/// only: a positional argument is text to style, which is
/// [`text`](make_text)'s job.
fn make_style<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    keys: StyleKeys<'v>,
    base: Style,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    'v: 'a,
{
    let (style, args) = parse_style(strand, keys, base, args)?;
    if args.len() != 0 {
        return Err(Error::unexpected_positional(strand, 0));
    }
    create_style(strand, global, keys, style, out);
    Ok(())
}

/// Applies an existing style: to positional arguments, producing [`Text`], or
/// to nothing, deriving a new [`Style`](StyleObject) from it.
fn apply_style<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    keys: StyleKeys<'v>,
    base: Style,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    'v: 'a,
{
    let (style, args) = parse_style(strand, keys, base, args)?;
    if args.len() == 0 {
        create_style(strand, global, keys, style, out);
    } else {
        let mut text = StrEmbryo::new();
        render_args(strand, global, &mut text, style, true, args)?;
        create_text(strand, global, text, out);
    }
    Ok(())
}

/// Interns every symbol the style options are named by.
///
/// These live on the global rather than in a closure because the `Style`
/// constructor is a type-level hook with no captured environment.
pub(crate) fn style_keys<'v>(builder: &mut Builder<'v>) -> StyleKeys<'v> {
    let color_names = [
        "BLACK",
        "RED",
        "GREEN",
        "YELLOW",
        "BLUE",
        "MAGENTA",
        "CYAN",
        "WHITE",
        "BRIGHT_BLACK",
        "BRIGHT_RED",
        "BRIGHT_GREEN",
        "BRIGHT_YELLOW",
        "BRIGHT_BLUE",
        "BRIGHT_MAGENTA",
        "BRIGHT_CYAN",
        "BRIGHT_WHITE",
    ];
    let mut colors =
        std::array::from_fn(|index| (builder.sym(color_names[index]), Color::Ansi(index as u8)));
    colors.sort_unstable_by_key(|(symbol, _)| *symbol);
    StyleKeys {
        fg: builder.sym("fg"),
        bg: builder.sym("bg"),
        attrs: [
            builder.sym("bold"),
            builder.sym("dim"),
            builder.sym("italic"),
            builder.sym("underline"),
            builder.sym("blink"),
            builder.sym("reverse"),
            builder.sym("hidden"),
            builder.sym("strikethrough"),
        ],
        colors: ColorKeys { values: colors },
        inherit: builder.sym("INHERIT"),
    }
}

pub(crate) fn configure_compiler(config: &mut Config<'_>) {
    config
        .prelude()
        .import_items("term")
        .items(["echo", "print"])
        .commit();
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let keys = global.style_keys;
    let StyleKeys {
        fg,
        bg,
        attrs:
            [
                bold,
                dim,
                italic,
                underline,
                blink,
                reverse,
                hidden,
                strikethrough,
            ],
        colors,
        inherit,
    } = keys;
    let chomp_sym = builder.sym("chomp");
    let can_style = global.syms.can_style;

    builder
        .module("term")
        .value("Text", global.types.text)
        .value("Style", global.types.style)
        .value("Console", global.types.console)
        .value("SinkConsole", global.types.sink_console)
        .value("Geometry", global.types.geometry)
        .value("Default", global.types.default)
        // A getter: the host installs its console after this module commits.
        .get("console", move |strand, out| {
            console::host_or_nil(strand, out);
            Ok(())
        })
        .object("default", global.types.default, DefaultOutput)
        .function_with_slots("output", async move |strand, args, out, [mut console]| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            // The *ambient* console: whatever an enclosing `capture` installed,
            // else the host. `term.console` is a name, so it pins instead.
            console::ambient(strand, &mut console)?;
            Output::set(strand, out, &console);
            Ok(())
        })
        .function_with_slots(
            "capture",
            async move |strand, args, out, [mut console, mut line_ending, mut tmp]| {
                let mode_sym = global.syms.mode;
                let ([target, func], [mode], rest) =
                    unpack!(strand, args, 2, 0, mode_sym = None, ...)?;
                if target.is_instance_of(strand, global.types.console) {
                    if mode.is_some() {
                        return Err(Error::value(
                            strand,
                            "mode: applies only when capturing into a plain sink",
                        ));
                    }
                    Output::set(strand, &mut console, target);
                } else {
                    // Any ordinary sink works; the adapter supplies the rest of
                    // the console interface. A bare sink does not style — pass
                    // a `term.SinkConsole` built with `can_style: true` to say
                    // otherwise.
                    let mode = crate::util::parse_mode(strand, mode.as_deref())?;
                    console::create_sink_console(
                        strand,
                        &target,
                        false,
                        mode,
                        Slot::reborrow(&mut console),
                    )
                    .await?;
                }
                let can_style = console::can_style(strand, &console)?;
                console::line_ending(strand, &console, &mut line_ending)?;
                let result = with_capture(
                    strand,
                    global,
                    &console,
                    can_style,
                    &line_ending,
                    async move |strand| func.call(strand, rest, out).await,
                )
                .await;
                // An unterminated `print` is only visible once the partial line
                // is emitted, so the scope always ends with a flush.
                let flushed = method!(strand, &console, global.syms.flush, &mut tmp).await;
                result.and(flushed)
            },
        )
        .function_with_slots(
            "sub",
            async move |strand, args, out, [mut console, mut line_ending, mut tmp]| {
                let ([func], [chomp, can_style], rest) =
                    unpack!(strand, args, 1, 0, chomp_sym = None, can_style = None, ...)?;
                let chomp = chomp.map(|v| v.to_bool(strand)).unwrap_or(true);
                let can_style = can_style.is_some_and(|v| v.to_bool(strand));
                global
                    .types
                    .sub_console
                    .create(strand, SubConsole::new(can_style), &mut console);
                Output::set(strand, &mut line_ending, console::LINE_ENDING);
                with_capture(
                    strand,
                    global,
                    &console,
                    can_style,
                    &line_ending,
                    async move |strand| func.call(strand, rest, &mut tmp).await,
                )
                .await?;
                global.types.sub_console.cast(&console).unwrap().enter_sync(
                    strand,
                    |strand, inst| {
                        let sub = inst.borrow(strand)?;
                        let mut value = sub.text();
                        if chomp {
                            value = strip_line_ending(value);
                        }
                        Output::set(strand, out, value);
                        Ok(())
                    },
                )
            },
        )
        .function_with_slots(
            "mute",
            async move |strand, args, out, [mut console, mut scratch, mut line_ending]| {
                let ([func], [], rest) = unpack!(strand, args, 1, 0, ...)?;
                // A console over std.null discards everything written to it —
                // the same mechanism `capture` uses, just wired to a sink that
                // throws writes away instead of collecting them.
                Output::set(strand, &mut scratch, Singleton::Null);
                console::create_sink_console(
                    strand,
                    &scratch,
                    false,
                    Framing::Line,
                    Slot::reborrow(&mut console),
                )
                .await?;
                Output::set(strand, &mut line_ending, console::LINE_ENDING);
                // The strand's own implicit output only needs touching when it
                // is still `term.default` — the startup placeholder that
                // itself forwards through this same capture. Anything else
                // (an explicit `shell.stdout`, a file, a redirect the caller
                // set up) was asked for by name and is left alone.
                strand.output(&mut scratch);
                let target: &Slot<'v, '_> = if global.types.default.cast(&scratch).is_some() {
                    &console
                } else {
                    &scratch
                };
                Redirect::new(strand)
                    .output(target)
                    .enter(async move |strand| {
                        with_capture(
                            strand,
                            global,
                            &console,
                            false,
                            &line_ending,
                            async move |strand| func.call(strand, rest, out).await,
                        )
                        .await
                    })
                    .await
            },
        )
        .function_with_slots("echo", async move |strand, args, _, [mut line]| {
            let ansi = console::ansi(strand);
            let mut output = StrEmbryo::new();
            let mut space = false;
            for arg in args {
                if space {
                    output.write_str(strand, " ")?;
                }
                space = true;
                match arg {
                    Arg::Pos(value) => append_value(
                        strand,
                        global,
                        &mut output,
                        Style::default(),
                        ansi,
                        true,
                        &value,
                    )?,
                    Arg::Key(key, value) => {
                        append_key(strand, &mut output, key)?;
                        output.write_str(strand, ": ")?;
                        append_value(
                            strand,
                            global,
                            &mut output,
                            Style::default(),
                            ansi,
                            true,
                            &value,
                        )?;
                    }
                }
            }
            output.finish(strand, &mut line);
            // The console supplies the terminator: only it knows what its own
            // line ending is.
            console::write_line(strand, &line).await
        })
        .function_with_slots("print", async move |strand, args, _, [mut text]| {
            let (
                [],
                [
                    fg_value,
                    bg_value,
                    bold_value,
                    dim_value,
                    italic_value,
                    underline_value,
                    blink_value,
                    reverse_value,
                    hidden_value,
                    strikethrough_value,
                ],
                args,
            ) = unpack!(
                strand,
                args,
                0,
                0,
                fg = None,
                bg = None,
                bold = None,
                dim = None,
                italic = None,
                underline = None,
                blink = None,
                reverse = None,
                hidden = None,
                strikethrough = None,
                ...
            )?;
            let style = Style {
                fg: color(strand, fg_value, "fg", colors, inherit, None)?,
                bg: color(strand, bg_value, "bg", colors, inherit, None)?,
                attrs: [
                    attr(strand, bold_value, "bold", inherit, false)?,
                    attr(strand, dim_value, "dim", inherit, false)?,
                    attr(strand, italic_value, "italic", inherit, false)?,
                    attr(strand, underline_value, "underline", inherit, false)?,
                    attr(strand, blink_value, "blink", inherit, false)?,
                    attr(strand, reverse_value, "reverse", inherit, false)?,
                    attr(strand, hidden_value, "hidden", inherit, false)?,
                    attr(strand, strikethrough_value, "strikethrough", inherit, false)?,
                ],
            };
            let mut output = StrEmbryo::new();
            render_args(
                strand,
                global,
                &mut output,
                style,
                console::ansi(strand),
                args,
            )?;
            output.finish(strand, &mut text);
            console::write_value(strand, &text).await
        })
        .function("text", async move |strand, args, out| {
            make_text(strand, global, keys, Style::default(), args, out)
        })
        .function("preformat", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let value = value
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "preformat: expected Str"))?
                .pin();
            create_preformatted_text(strand, global, &value, out)
        })
        .commit();
}

#[cfg(test)]
mod tests {
    use super::{Filter, FilterMode, Style};
    use dolang::runtime::value::fmt::Format;

    fn filter(input: &str, mode: FilterMode) -> String {
        futures::executor::block_on(dolang::runtime::vm::Builder::build(async |builder| {
            builder
                .enter(async |strand| {
                    let mut output = String::new();
                    let mut filter = Filter::new(&mut output, mode);
                    filter.write_str(strand, input).unwrap();
                    filter.finish(strand).unwrap();
                    output
                })
                .await
        }))
    }

    #[test]
    fn strips_controls_and_non_sgr_sequences() {
        assert_eq!(
            filter(
                "a\x07b\x1b[2Jc\x1b]0;title\x07d\u{009b}31me\u{009d}title\x07f\n\tg",
                FilterMode::Plain,
            ),
            "abcdef\n\tg"
        );
    }

    #[test]
    fn ordinary_text_strips_sgr_sequences() {
        assert_eq!(
            filter("before\x1b[31mred\x1b[0mafter", FilterMode::Plain),
            "beforeredafter"
        );
    }

    #[test]
    fn canonicalizes_sgr_and_resets_at_end() {
        assert_eq!(
            filter("\x1b[1;31mred\x1b[39m bold", FilterMode::Preformat,),
            "\x1b[1m\x1b[31mred\x1b[39m bold\x1b[0m"
        );
    }

    #[test]
    fn supports_indexed_and_rgb_colors() {
        assert_eq!(
            filter("\x1b[38;5;123ma\x1b[48;2;1;2;3mb", FilterMode::Preformat,),
            "\x1b[38;5;123ma\x1b[48;2;1;2;3mb\x1b[0m"
        );
    }

    #[test]
    fn child_resets_restore_parent_style() {
        let parent = Style {
            attrs: [true, false, false, false, false, false, false, false],
            ..Style::default()
        };
        assert_eq!(
            filter("\x1b[31mred\x1b[0mparent", FilterMode::Child(parent),),
            "\x1b[31mred\x1b[0m\x1b[1mparent"
        );
    }

    #[test]
    fn scanner_preserves_state_across_writes() {
        futures::executor::block_on(dolang::runtime::vm::Builder::build(async |builder| {
            builder
                .enter(async |strand| {
                    let mut output = String::new();
                    let mut filter = Filter::new(&mut output, FilterMode::Preformat);
                    filter.write_str(strand, "a\x1b[").unwrap();
                    filter.write_str(strand, "31mred").unwrap();
                    filter.finish(strand).unwrap();
                    assert_eq!(output, "a\x1b[31mred\x1b[0m");
                })
                .await
        }));
    }
}
