use dolang::runtime::object::fmt;
use dolang::{
    compile::Compiler,
    runtime::{
        Arg, Args, Error, Format, Instance, Object, Output, Result, Slot, State, Strand, Sym,
        Value,
        object::{Mut, Ref, TypeBuilder},
        unpack,
        value::{StrEmbryo, View},
        vm::Builder,
    },
};
use tokio::io::AsyncWriteExt;

use crate::{error::ErrorExt as _, global::Global};

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

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
        out.write_str(strand, &text)
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

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method("indent", async move |this, strand, args, out| {
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

fn append_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    out: &mut dyn Format<'v>,
    parent: Style,
    ansi: bool,
    argument: bool,
    value: &Value<'v>,
) -> Result<'v, 's, ()> {
    if let Some(text) = global.types.text.downcast(value) {
        let borrow = text.borrow(strand)?;
        let text = Ref::slot::<0>(&borrow).as_str(strand).unwrap().pin();
        let mode = if ansi {
            FilterMode::Child(parent)
        } else {
            FilterMode::Plain
        };
        let mut filter = Filter::new(out, mode);
        filter.write_str(strand, &text)?;
        filter.finish(strand)
    } else {
        let mut filter = Filter::new(out, FilterMode::Plain);
        if argument {
            value.display_arg(strand, &mut filter)?;
        } else {
            value.display(strand, &mut filter)?;
        }
        filter.finish(strand)
    }
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
struct StyleKeys<'v> {
    fg: Sym<'v, 'v>,
    bg: Sym<'v, 'v>,
    attrs: [Sym<'v, 'v>; ATTR_COUNT],
    colors: ColorKeys<'v>,
    inherit: Sym<'v, 'v>,
}

#[derive(Clone, Copy)]
struct ColorKeys<'v> {
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
            format!("{name}: expected sym, int, array, or tuple"),
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
    let this = global.types.text.downcast(&out).unwrap();
    text.finish(strand, Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()));
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
    if args.len() == 0 {
        create_style(strand, global, keys, style, out);
    } else {
        let mut text = StrEmbryo::new();
        render_args(strand, global, &mut text, style, true, args)?;
        create_text(strand, global, text, out);
    }
    Ok(())
}

pub(crate) fn configure_compiler(compiler: &mut Compiler<'_>) {
    compiler
        .prelude()
        .import_module("term")
        .import_items("term")
        .items(["echo", "print"])
        .commit();
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
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
    let keys = StyleKeys {
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
    };
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

    builder
        .module("term")
        .value("Text", global.types.text)
        .value("Style", global.types.style)
        .function("echo", async move |strand, args, _| {
            let ansi = global.terminal.stderr_is_terminal;
            let mut output = String::new();
            let mut space = false;
            for arg in args {
                if space {
                    output.push(' ');
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
                        output.push_str(": ");
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
            output.push('\n');
            global
                .terminal
                .writer
                .lock()
                .await
                .write_all(output.as_bytes())
                .await
                .map_err(|error| error.into_sys(strand))
        })
        .function("print", async move |strand, args, _| {
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
            let mut output = String::new();
            render_args(
                strand,
                global,
                &mut output,
                style,
                global.terminal.stderr_is_terminal,
                args,
            )?;
            global
                .terminal
                .writer
                .lock()
                .await
                .write_all(output.as_bytes())
                .await
                .map_err(|error| error.into_sys(strand))
        })
        .function("style", async move |strand, args, out| {
            apply_style(strand, global, keys, Style::default(), args, out)
        })
        .function("preformat", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let value = value
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "preformat: expected str"))?
                .pin();
            let mut text = StrEmbryo::new();
            let mut filter = Filter::new(&mut text, FilterMode::Preformat);
            filter.write_str(strand, &value)?;
            filter.finish(strand)?;
            create_text(strand, global, text, out);
            Ok(())
        })
        .commit();
}

#[cfg(test)]
mod tests {
    use super::{Filter, FilterMode, Style};
    use dolang::runtime::Format;

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
