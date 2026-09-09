use std::{
    cell::{Cell, RefCell},
    io,
    pin::Pin,
    rc::Rc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use dolang::runtime::value::fmt::Format;

use dolang::runtime::object::fmt;

use dolang::runtime::{
    Arg, Error, Instance, Object, Output, Result, State, Strand, Sym, Value, call, method,
    object::TypeBuilder,
    strand::{self, Local},
    unpack,
    value::{Empty, Singleton, Slot, View},
    vm::Builder,
};
use dolang_ext_shell::with_terminal;
use indicatif as ix;
use ix::MultiProgress;
use tokio::io::AsyncWrite;

use crate::{
    global::Global,
    plain,
    style::{self, Color, ColorKeys, DEFAULT_ICON, Mode, Style, StyleKeys, Units},
};

// --- Strand-local state ---

pub(crate) struct ProgressLocal {
    depth: Cell<u16>,
    parent_id: Cell<u64>,
    state: RefCell<Option<SharedState>>,
}

/// Shared state for the enclosing `progress.with` scope: either interactive
/// (indicatif `MultiProgress`) or plain (non-terminal, plain-text lines).
#[derive(Clone)]
enum SharedState {
    Interactive(Rc<RefCell<ProgressState>>),
    Plain(Rc<plain::PlainConfig>),
}

struct Widget {
    id: u64,
    depth: u16,
    bar: ix::ProgressBar,
    mode: Mode,
    units: Option<Units>,
}

struct ProgressState {
    multi: Option<MultiProgress>,
    style: Style,
    /// Whether ANSI styling should be embedded in the `status` field's own
    /// hand-rendered text (see `style::write_status_text`) — computed once
    /// for the scope, mirroring `PlainInfo::ansi`, rather than re-queried on
    /// every restyle, since none of the restyle call sites below have a
    /// `Strand` handy.
    ansi: bool,
    widgets: Vec<Widget>,
    next_id: u64,
}

impl ProgressState {
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn find_widget_idx(&self, id: u64) -> Option<usize> {
        self.widgets.iter().position(|w| w.id == id)
    }
}

impl<'v> Local<'v> for ProgressLocal {
    fn init() -> Self {
        Self {
            depth: Cell::new(0),
            parent_id: Cell::new(0),
            state: RefCell::new(None),
        }
    }

    fn inherit(&self, _strand: &strand::Strand<'v, '_>, _kind: strand::InheritKind) -> Self {
        Self {
            depth: Cell::new(self.depth.get()),
            parent_id: Cell::new(self.parent_id.get()),
            state: RefCell::new(self.state.borrow().clone()),
        }
    }
}

// --- Leaf detection ---

fn is_leaf(widgets: &[Widget], idx: usize) -> bool {
    widgets
        .get(idx + 1)
        .map(|w| w.depth <= widgets[idx].depth)
        .unwrap_or(true)
}

// --- Depth map operations ---

/// Find the insertion index in the widget list for a new child of `parent_id`.
fn find_insert_index(widgets: &[Widget], parent_id: u64) -> usize {
    let parent_idx = widgets
        .iter()
        .position(|w| w.id == parent_id)
        .expect("parent not in widget list");
    let parent_depth = widgets[parent_idx].depth;
    let mut idx = parent_idx + 1;
    while idx < widgets.len() && widgets[idx].depth > parent_depth {
        idx += 1;
    }
    idx
}

/// Insert a new progress bar into the MultiProgress at the correct position.
/// If the parent was a leaf spinner, hides its spinner animation.
fn do_insert_bar(
    state: &mut ProgressState,
    multi: &MultiProgress,
    parent_id: u64,
    depth: u16,
    pb: ix::ProgressBar,
    mode: Mode,
    units: Option<Units>,
) -> (ix::ProgressBar, u64) {
    let id = state.alloc_id();
    let insert_idx = find_insert_index(&state.widgets, parent_id);

    // Check if parent was a leaf before insertion
    let parent_idx = state
        .widgets
        .iter()
        .position(|w| w.id == parent_id)
        .unwrap();
    let parent_was_leaf = is_leaf(&state.widgets, parent_idx);

    let multi_pos = insert_idx - 1;
    let pb = multi.insert(multi_pos, pb);
    state.widgets.insert(
        insert_idx,
        Widget {
            id,
            depth: depth + 1,
            bar: pb.clone(),
            mode,
            units,
        },
    );

    // If parent was a leaf spinner, it's now non-leaf — hide its spinner
    if parent_was_leaf && state.widgets[parent_idx].mode == Mode::Spinner {
        let pw = &state.widgets[parent_idx];
        style::apply_spinner_style(
            &pw.bar,
            &state.style,
            pw.depth - 1,
            pw.units,
            false,
            state.ansi,
        );
    }

    (pb, id)
}

/// Remove a widget and all its transitive descendants from the widget list
/// and MultiProgress display. If the parent becomes a leaf spinner, restores
/// its spinner animation.
fn do_remove(state: &mut ProgressState, multi: &MultiProgress, widget_id: u64) {
    if let Some(idx) = state.widgets.iter().position(|w| w.id == widget_id) {
        let widget_depth = state.widgets[idx].depth;

        // Find parent (widget with depth < widget_depth, scanning backward)
        let parent_idx = (0..idx)
            .rev()
            .find(|&i| state.widgets[i].depth < widget_depth);

        multi.remove(&state.widgets[idx].bar);
        state.widgets.remove(idx);
        // Remove transitive descendants (entries immediately following with depth > widget_depth)
        while idx < state.widgets.len() && state.widgets[idx].depth > widget_depth {
            multi.remove(&state.widgets[idx].bar);
            state.widgets.remove(idx);
        }

        // If parent became a leaf spinner, restore its spinner
        if let Some(pi) = parent_idx
            && is_leaf(&state.widgets, pi)
            && state.widgets[pi].mode == Mode::Spinner
        {
            let pw = &state.widgets[pi];
            style::apply_spinner_style(
                &pw.bar,
                &state.style,
                pw.depth - 1,
                pw.units,
                true,
                state.ansi,
            );
        }
    }
}

// --- MultiProgressWriter ---

struct MultiProgressWriter {
    multi: MultiProgress,
    buf: Vec<u8>,
}

impl MultiProgressWriter {
    fn new(multi: MultiProgress) -> Self {
        Self {
            multi,
            buf: Vec::new(),
        }
    }

    fn flush_lines(&mut self) -> io::Result<()> {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[..pos]).into_owned();
            self.buf.drain(..=pos);
            self.multi.println(&line).map_err(io::Error::other)?;
        }
        Ok(())
    }
}

impl AsyncWrite for MultiProgressWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.buf.extend_from_slice(buf);
        if let Err(e) = self.flush_lines() {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            if let Err(e) = self.multi.println(&line) {
                return Poll::Ready(Err(io::Error::other(e)));
            }
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

// --- Helpers ---

fn check_closed<'v, 's>(strand: &mut Strand<'v, 's>, closed: &Cell<bool>) -> Result<'v, 's, ()> {
    if closed.get() {
        Err(Error::state_error(strand, "closed"))
    } else {
        Ok(())
    }
}

fn parse_units<'v, 's>(
    strand: &mut Strand<'v, 's>,
    units_val: Option<&Value<'v>>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, Option<Units>> {
    match units_val {
        Some(v) => {
            if let Some(sym) = v.as_sym(strand) {
                if sym == global.units.count {
                    Ok(Some(Units::Count))
                } else if sym == global.units.bytes {
                    Ok(Some(Units::Bytes))
                } else if sym == global.units.percent {
                    Ok(Some(Units::Percent))
                } else {
                    Err(Error::value(
                        strand,
                        "units: expected :COUNT:, :BYTES:, or :PERCENT:",
                    ))
                }
            } else {
                Err(Error::type_error(strand, "units: expected `Sym`"))
            }
        }
        None => Ok(None),
    }
}

fn parse_icon<'v, 's>(
    strand: &mut Strand<'v, 's>,
    icon_val: Option<&Value<'v>>,
) -> Result<'v, 's, String> {
    match icon_val {
        Some(v) => Ok(v
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "icon: expected `Str`"))?
            .into()),
        None => Ok(DEFAULT_ICON.to_owned()),
    }
}

fn parse_message<'v, 's>(
    strand: &mut Strand<'v, 's>,
    message: Option<&Value<'v>>,
) -> Result<'v, 's, Option<String>> {
    message
        .map(|msg| {
            Ok(msg
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "message: expected `Str`"))?
                .to_string())
        })
        .transpose()
}

fn parse_duration_secs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    val: Option<&Value<'v>>,
    default: Duration,
) -> Result<'v, 's, Duration> {
    match val {
        Some(v) => {
            let secs = v
                .as_f64(strand)
                .ok_or_else(|| Error::type_error(strand, format!("{name}: expected `Float`")))?;
            Ok(Duration::from_secs_f64(secs))
        }
        None => Ok(default),
    }
}

/// Get the multi from shared state, returning an error if the progress context
/// has been closed (e.g. background strand outlived progress.with).
fn get_multi<'v, 's>(
    strand: &mut Strand<'v, 's>,
    state: &RefCell<ProgressState>,
) -> Result<'v, 's, MultiProgress> {
    state
        .borrow()
        .multi
        .clone()
        .ok_or_else(|| Error::state_error(strand, "progress context closed"))
}

struct ShowOptions {
    total: Option<u64>,
    message: Option<String>,
    icon: String,
    units: Option<Units>,
    tick: Duration,
}

struct MultiState {
    multi: MultiProgress,
    state_rc: Rc<RefCell<ProgressState>>,
    widget_id: u64,
    prev_depth: u16,
    prev_parent_id: u64,
}

enum ShowKind {
    None,
    Interactive(MultiState),
    Plain {
        prev_depth: u16,
        prev_parent_id: u64,
        info: Rc<plain::PlainInfo>,
    },
}

struct ActiveIndicator {
    bar: ix::ProgressBar,
    kind: ShowKind,
}

async fn install_indicator<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    options: ShowOptions,
    mut slot: Slot<'v, '_>,
) -> Result<'v, 's, ActiveIndicator> {
    let mode = if options.total.is_some() {
        Mode::Bar
    } else {
        Mode::Spinner
    };
    let local = global.local.get(strand);
    let shared_state = local.state.borrow().clone();

    let (pb, kind) = match shared_state {
        None => {
            let pb = ix::ProgressBar::hidden();
            if let Some(n) = options.total {
                pb.set_length(n);
            }
            (pb, ShowKind::None)
        }
        Some(SharedState::Interactive(state_rc)) => {
            let multi = get_multi(strand, &state_rc)?;
            let local = global.local.get(strand);
            let depth = local.depth.get();
            let parent_id = local.parent_id.get();

            let pb_init = match options.total {
                Some(n) => ix::ProgressBar::new(n),
                None => ix::ProgressBar::new_spinner(),
            };

            let (pb, widget_id) = {
                let mut state = state_rc.borrow_mut();
                let (pb, widget_id) = do_insert_bar(
                    &mut state,
                    &multi,
                    parent_id,
                    depth,
                    pb_init,
                    mode,
                    options.units,
                );

                match mode {
                    Mode::Bar => {
                        style::apply_bar_style(&pb, &state.style, depth, options.units, state.ansi);
                    }
                    Mode::Spinner => {
                        style::apply_spinner_style(
                            &pb,
                            &state.style,
                            depth,
                            options.units,
                            true,
                            state.ansi,
                        );
                    }
                }
                drop(state);
                (pb, widget_id)
            };

            local.depth.set(depth + 1);
            local.parent_id.set(widget_id);

            (
                pb,
                ShowKind::Interactive(MultiState {
                    multi,
                    state_rc,
                    widget_id,
                    prev_depth: depth,
                    prev_parent_id: parent_id,
                }),
            )
        }
        Some(SharedState::Plain(config)) => {
            let pb = ix::ProgressBar::hidden();
            if let Some(n) = options.total {
                pb.set_length(n);
            }

            let local = global.local.get(strand);
            let depth = local.depth.get();
            let parent_id = local.parent_id.get();
            let info = Rc::new(plain::PlainInfo::new(
                depth,
                options.units,
                config.ansi,
                config,
                parent_id,
            ));

            local.depth.set(depth + 1);
            local.parent_id.set(info.id());

            (
                pb,
                ShowKind::Plain {
                    prev_depth: depth,
                    prev_parent_id: parent_id,
                    info,
                },
            )
        }
    };

    pb.set_prefix(options.icon);
    pb.enable_steady_tick(options.tick);
    if let Some(message) = options.message {
        pb.set_message(message);
    }

    let plain_info = match &kind {
        ShowKind::Plain { info, .. } => Some(info.clone()),
        _ => None,
    };
    let annex = IndicatorAnnex {
        global,
        bar: pb.clone(),
        state_rc: match &kind {
            ShowKind::Interactive(ms) => Some(ms.state_rc.clone()),
            _ => None,
        },
        widget_id: match &kind {
            ShowKind::Interactive(ms) => ms.widget_id,
            _ => 0,
        },
        plain: plain_info,
        tick: options.tick,
        closed: Cell::new(false),
        progressed: Cell::new(false),
    };
    global
        .types
        .indicator
        .create_with_annex(strand, Indicator, annex, &mut slot);

    if let ShowKind::Plain { info, .. } = &kind
        && let Some(line) = plain::maybe_format(&pb, info, true, plain::LineEvent::Start)
    {
        write_plain_line(strand, global, info, &line).await?;
    }

    Ok(ActiveIndicator { bar: pb, kind })
}

async fn finish_indicator<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    active: ActiveIndicator,
    slot: &Value<'v>,
) -> Result<'v, 's, ()> {
    global
        .types
        .indicator
        .cast(slot)
        .unwrap()
        .enter_sync(strand, |_strand, inst| {
            inst.annex().closed.set(true);
        });

    match active.kind {
        ShowKind::None => Ok(()),
        ShowKind::Interactive(ms) => {
            let local = global.local.get(strand);
            local.depth.set(ms.prev_depth);
            local.parent_id.set(ms.prev_parent_id);

            if !active.bar.is_finished() {
                active.bar.finish_and_clear();
            }
            let mut state = ms.state_rc.borrow_mut();
            do_remove(&mut state, &ms.multi, ms.widget_id);
            Ok(())
        }
        ShowKind::Plain {
            prev_depth,
            prev_parent_id,
            info,
        } => {
            let local = global.local.get(strand);
            local.depth.set(prev_depth);
            local.parent_id.set(prev_parent_id);

            match plain::maybe_format(&active.bar, &info, true, plain::LineEvent::End) {
                Some(line) => write_plain_line(strand, global, &info, &line).await,
                None => Ok(()),
            }
        }
    }
}

async fn write_plain_line<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    info: &plain::PlainInfo,
    line: &str,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut output]| {
            {
                let root = global.output.borrow();
                Output::set(strand, &mut output, &*root);
            }
            dolang_ext_shell::write_terminal_line(strand, &output, info.line_ending(), line).await
        })
        .await
}

struct StepMetadata {
    name: Option<String>,
    icon: Option<String>,
}

fn parse_step<'v, 's>(
    strand: &mut Strand<'v, 's>,
    step: &Value<'v>,
    name_sym: Sym<'v, '_>,
    icon_sym: Sym<'v, '_>,
    mut callable: Slot<'v, '_>,
    mut key: Slot<'v, '_>,
    mut value: Slot<'v, '_>,
) -> Result<'v, 's, StepMetadata> {
    let Some(dict) = step.as_dict(strand) else {
        Output::set(strand, callable, step);
        return Ok(StepMetadata {
            name: None,
            icon: None,
        });
    };

    if !dict.get(strand, 0i64, None, &mut callable)? {
        return Err(Error::missing_positional(strand, 0));
    }

    let name = if dict.get(strand, name_sym, None, &mut value)? {
        Some(
            value
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "step name: expected `Str`"))?
                .to_string(),
        )
    } else {
        None
    };
    let icon = if dict.get(strand, icon_sym, None, &mut value)? {
        Some(
            value
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "step icon: expected `Str`"))?
                .to_string(),
        )
    } else {
        None
    };

    let mut pairs = dict.pairs();
    while pairs.next(strand, Slot::reborrow(&mut key), Slot::reborrow(&mut value))? {
        match key.view(strand) {
            View::Int(0) => {}
            View::Int(index) if index >= 0 => {
                let index = usize::try_from(index).unwrap_or(usize::MAX);
                return Err(Error::unexpected_positional(strand, index));
            }
            View::Sym(sym) if sym == name_sym || sym == icon_sym => {}
            _ => return Err(Error::unexpected_key(strand, &key)),
        }
    }

    Ok(StepMetadata { name, icon })
}

fn step_message(overall: Option<&str>, name: Option<&str>) -> String {
    match (overall, name) {
        (Some(overall), Some(name)) => format!("{overall}: {name}"),
        (Some(overall), None) => overall.to_owned(),
        (None, Some(name)) => name.to_owned(),
        (None, None) => String::new(),
    }
}

// --- VM configuration ---

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let style_kw = builder.sym("style");
    let interval_kw = builder.sym("interval");
    let total_kw = builder.sym("total");
    let message_kw = builder.sym("message");
    let icon_kw = builder.sym("icon");
    let units_kw = builder.sym("units");
    let tick_kw = builder.sym("tick");
    let name_sym = builder.sym("name");
    let update_sym = builder.sym("update");
    let delta_sym = builder.sym("delta");
    let mut colors = [
        ("BLACK", Color::Black),
        ("RED", Color::Red),
        ("GREEN", Color::Green),
        ("YELLOW", Color::Yellow),
        ("BLUE", Color::Blue),
        ("MAGENTA", Color::Magenta),
        ("CYAN", Color::Cyan),
        ("WHITE", Color::White),
        ("BRIGHT_BLACK", Color::BrightBlack),
        ("BRIGHT_RED", Color::BrightRed),
        ("BRIGHT_GREEN", Color::BrightGreen),
        ("BRIGHT_YELLOW", Color::BrightYellow),
        ("BRIGHT_BLUE", Color::BrightBlue),
        ("BRIGHT_MAGENTA", Color::BrightMagenta),
        ("BRIGHT_CYAN", Color::BrightCyan),
        ("BRIGHT_WHITE", Color::BrightWhite),
        ("BRIGHT", Color::Bright),
    ]
    .map(|(name, color)| (builder.sym(name), color));
    colors.sort_unstable_by_key(|(symbol, _)| *symbol);
    let style_keys = StyleKeys {
        bar: builder.sym("bar"),
        spinner: builder.sym("spinner"),
        message: message_kw,
        icon: icon_kw,
        elapsed: builder.sym("elapsed"),
        position: builder.sym("position"),
        total: total_kw,
        status: builder.sym("status"),
        width: builder.sym("width"),
        fg: builder.sym("fg"),
        bg: builder.sym("bg"),
        attrs: builder.sym("attrs"),
        alt: builder.sym("alt"),
        colors: ColorKeys { values: colors },
    };

    builder
        .module("progress")
        .value("Indicator", global.types.indicator)
        .function("with", async move |strand, args, mut out| {
            let ([func], [style_val, interval_val]) =
                unpack!(strand, args, 1, 0, style_kw = None, interval_kw = None)?;

            let style = match style_val {
                Some(sv) => style::parse_style(strand, &sv, &style_keys)?,
                // No explicit `style:` kwarg: size the message column to
                // the terminal's actual width instead of the fixed
                // built-in default, so the common case makes good use of
                // whatever room is available.
                None => Style::default_for_cols(dolang_ext_shell::stderr_cols(strand)),
            };

            // If stderr is not a terminal, use the plain (non-interactive)
            // rendering path instead of indicatif's MultiProgress.
            if !dolang_ext_shell::stderr_is_tty(strand) {
                let interval = parse_duration_secs(
                    strand,
                    "interval",
                    interval_val.as_deref(),
                    plain::DEFAULT_INTERVAL,
                )?;
                if global.plain_active.replace(true) {
                    return Err(Error::state_error(
                        strand,
                        "progress context already active",
                    ));
                }
                let line_ending = dolang_ext_shell::terminal_line_ending(strand)?;
                let ansi = dolang_ext_shell::ansi_enabled(strand);
                {
                    let mut output = global.output.borrow_mut();
                    dolang_ext_shell::terminal_output(strand, &mut *output);
                }
                let config = plain::PlainConfig::new(style, interval, line_ending, ansi);

                let local = global.local.get(strand);
                let prev_depth = local.depth.replace(0);
                let prev_parent_id = local.parent_id.replace(0);
                let prev_state = local
                    .state
                    .replace(Some(SharedState::Plain(Rc::new(config))));

                let result = call!(strand, &func, &mut out).await;

                let local = global.local.get(strand);
                local.depth.replace(prev_depth);
                local.parent_id.replace(prev_parent_id);
                local.state.replace(prev_state);
                Output::set(strand, &mut *global.output.borrow_mut(), Singleton::Null);
                global.plain_active.set(false);

                return result;
            }

            let multi = MultiProgress::new();
            let ansi = dolang_ext_shell::ansi_enabled(strand);
            let state_rc = Rc::new(RefCell::new(ProgressState {
                multi: Some(multi.clone()),
                style,
                ansi,
                widgets: vec![Widget {
                    id: 0,
                    depth: 0,
                    bar: ix::ProgressBar::hidden(),
                    mode: Mode::Bar,
                    units: None,
                }],
                next_id: 1,
            }));

            let local = global.local.get(strand);
            let prev_depth = local.depth.replace(0);
            let prev_parent_id = local.parent_id.replace(0);
            let prev_state = local
                .state
                .replace(Some(SharedState::Interactive(state_rc.clone())));

            let writer: Pin<Box<dyn AsyncWrite>> =
                Box::pin(MultiProgressWriter::new(multi.clone()));
            let result = with_terminal(strand, writer, async |strand| {
                let res = call!(strand, &func, &mut out).await;
                let _ = multi.clear();
                res
            })
            .await;

            // Invalidate shared state so background strands see the closure
            state_rc.borrow_mut().multi = None;

            // Restore previous local state
            let local = global.local.get(strand);
            local.depth.replace(prev_depth);
            local.parent_id.replace(prev_parent_id);
            local.state.replace(prev_state);

            result
        })
        .function_with_slots("show", async move |strand, args, mut out, [mut slot]| {
            let ([func], [total_val, msg_val, icon_val, units_val, tick_ms]) = unpack!(
                strand,
                args,
                1,
                0,
                total_kw = None,
                message_kw = None,
                icon_kw = None,
                units_kw = None,
                tick_kw = None
            )?;

            let options = ShowOptions {
                total: total_val
                    .as_deref()
                    .map(|value| value.to_u64(strand))
                    .transpose()?,
                message: parse_message(strand, msg_val.as_deref())?,
                icon: parse_icon(strand, icon_val.as_deref())?,
                units: parse_units(strand, units_val.as_deref(), global)?,
                tick: parse_duration_secs(
                    strand,
                    "tick",
                    tick_ms.as_deref(),
                    Duration::from_secs_f64(0.08),
                )?,
            };
            let active =
                install_indicator(strand, global, options, Slot::reborrow(&mut slot)).await?;
            let res = call!(strand, &func, &mut out, &slot).await;
            res.and(finish_indicator(strand, global, active, &slot).await)
        })
        .function_with_slots(
            "steps",
            async move |strand,
                        args,
                        mut out,
                        [
                mut indicator,
                mut callable,
                mut key,
                mut value,
                mut result,
                mut tmp,
            ]| {
                let ([], [message_val, icon_val], mut steps) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    message_kw = None,
                    icon_kw = None,
                    ...
                )?;

                let overall_message = parse_message(strand, message_val.as_deref())?;
                let default_icon = parse_icon(strand, icon_val.as_deref())?;
                if steps.len() == 0 {
                    Output::set(strand, out, Empty::Array);
                    return Ok(());
                }

                let total = u64::try_from(steps.len()).expect("argument count fits in u64");
                let options = ShowOptions {
                    total: Some(total),
                    message: overall_message.clone(),
                    icon: default_icon.clone(),
                    units: None,
                    tick: Duration::from_secs_f64(0.08),
                };
                let active =
                    install_indicator(strand, global, options, Slot::reborrow(&mut indicator))
                        .await?;

                let res = async {
                    Output::set(strand, &mut out, Empty::Array);
                    for arg in &mut steps {
                        let step = match arg {
                            Arg::Pos(step) => step,
                            Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                        };
                        let metadata = parse_step(
                            strand,
                            &step,
                            name_sym,
                            icon_kw,
                            Slot::reborrow(&mut callable),
                            Slot::reborrow(&mut key),
                            Slot::reborrow(&mut value),
                        )?;
                        let message =
                            step_message(overall_message.as_deref(), metadata.name.as_deref());
                        let icon = metadata.icon.as_deref().unwrap_or(&default_icon);

                        method!(
                            strand,
                            &indicator,
                            update_sym,
                            &mut tmp,
                            message_kw: message.as_str(),
                            icon_kw: icon
                        )
                        .await?;
                        call!(strand, &callable, &mut result).await?;
                        out.as_array(strand)
                            .expect("steps output is an array")
                            .push(strand, &result)?;
                        method!(strand, &indicator, delta_sym, &mut tmp).await?;
                    }
                    Ok(())
                }
                .await;

                res.and(finish_indicator(strand, global, active, &indicator).await)
            },
        )
        .commit();
}

// --- Indicator ---

pub(crate) struct Indicator;

pub(crate) struct IndicatorAnnex<'v> {
    global: State<'v, Global<'v>>,
    bar: ix::ProgressBar,
    state_rc: Option<Rc<RefCell<ProgressState>>>,
    widget_id: u64,
    plain: Option<Rc<plain::PlainInfo>>,
    /// Steady-tick interval this indicator's bar was installed with, so
    /// [`set_position`](IndicatorAnnex::set_position) can put the ticker
    /// back after re-baselining. Zero means none was installed, which
    /// `enable_steady_tick` treats as a no-op.
    tick: Duration,
    closed: Cell<bool>,
    /// Whether this indicator has been given any progress yet — see
    /// [`set_position`](IndicatorAnnex::set_position).
    progressed: Cell<bool>,
}

impl IndicatorAnnex<'_> {
    /// Moves the indicator to an absolute position.
    ///
    /// The first position an indicator is given is where it *starts*, not
    /// ground it just covered: a resumed download that opens at 5 MB did
    /// not transfer those 5 MB in the millisecond since its bar appeared.
    /// indicatif cannot tell the two apart — every forward jump goes into
    /// its throughput estimator as `delta_steps / delta_t`, and against a
    /// `delta_t` of microseconds that reads as an enormous rate, which then
    /// decays away over the following seconds instead of being simply
    /// absent. Re-baselining the estimator here drops the jump while
    /// leaving the position, elapsed time, and total alone.
    ///
    /// Taking the steady ticker down around the reset is what makes it
    /// stick. `set_position` only stores the position — the estimator is
    /// fed from a tick, and `ProgressBar::tick` is ignored outright while a
    /// steady ticker is installed. So the jump would be recorded by the
    /// ticker thread on its own schedule, *after* the reset here and
    /// against a stale baseline, which is the artifact all over again.
    /// With the ticker off, the explicit tick records it now, the reset
    /// discards that sample, and the estimator's reference point is left at
    /// `pos` for the ticker to resume from.
    ///
    /// Later position changes are recorded normally: by then the indicator
    /// has a history, and a caller driving one with absolute positions
    /// rather than `delta` is reporting real progress.
    fn set_position(&self, pos: u64) {
        self.bar.set_position(pos);
        if !self.progressed.replace(true) {
            self.bar.disable_steady_tick();
            self.bar.tick();
            self.bar.reset_eta();
            self.bar.enable_steady_tick(self.tick);
            if let Some(info) = &self.plain {
                info.reset_rate();
            }
        }
    }

    /// Advances the indicator by `n` steps, which may be negative. Unlike
    /// [`set_position`](Self::set_position), this is always real observed
    /// progress.
    fn advance(&self, n: i64) {
        if n >= 0 {
            self.bar.inc(n as u64); // safe: n >= 0
        } else {
            self.bar.dec(n.unsigned_abs());
        }
        self.progressed.set(true);
    }
}

/// Plain-mode emit checkpoint shared by `update` and `delta`: writes a
/// rate-limited (or, if `force`, unconditional) status line if this
/// indicator belongs to a non-interactive `progress.with` scope.
async fn maybe_emit_plain<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    bar: &ix::ProgressBar,
    plain_info: &Option<Rc<plain::PlainInfo>>,
    force: bool,
) -> Result<'v, 's, ()> {
    let Some(info) = plain_info else {
        return Ok(());
    };
    // Skip a routine (non-forced) update that just completed the bar — the
    // indicator's scope is almost certainly about to exit, which prints a
    // `finished` line anyway, so a "100%" line right before that would just
    // be redundant. A forced update (icon/message change) still goes
    // through even at completion, since that's a real content change.
    let completed = matches!(bar.length(), Some(len) if bar.position() >= len);
    if !force && completed {
        return Ok(());
    }
    if let Some(line) = plain::maybe_format(bar, info, force, plain::LineEvent::Update) {
        write_plain_line(strand, global, info, &line).await?;
    }
    Ok(())
}

impl<'v> Object<'v> for Indicator {
    const NAME: &'v str = "Indicator";
    const MODULE: &'v str = "progress";
    type Annex = IndicatorAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<progress.Indicator>")
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let icon_kw = builder.sym("icon");
        let message_kw = builder.sym("message");
        let total_kw = builder.sym("total");
        let position_kw = builder.sym("position");
        let delta_kw = builder.sym("delta");
        let units_kw = builder.sym("units");
        builder
            // --- Getters (read-only; see `update` for writes) ---
            .get("message", |this, strand, out| {
                check_closed(strand, &this.annex().closed)?;
                Output::set(strand, out, this.annex().bar.message().as_str());
                Ok(())
            })
            .get("icon", |this, strand, out| {
                check_closed(strand, &this.annex().closed)?;
                Output::set(strand, out, this.annex().bar.prefix().as_str());
                Ok(())
            })
            .get("total", |this, strand, out| {
                check_closed(strand, &this.annex().closed)?;
                if let Some(n) = this.annex().bar.length() {
                    Output::set(strand, out, n);
                }
                Ok(())
            })
            .get("position", |this, strand, out| {
                check_closed(strand, &this.annex().closed)?;
                Output::set(strand, out, this.annex().bar.position());
                Ok(())
            })
            // --- Methods ---
            .method("delta", async move |this, strand, args, _out| {
                check_closed(strand, &this.annex().closed)?;
                let ([], [amount]) = unpack!(strand, args, 0, 1)?;
                let n = match amount {
                    Some(v) => v
                        .to_i64(strand)
                        .map_err(|_| Error::type_error(strand, "expected `Int`"))?,
                    None => 1,
                };
                let annex = this.annex();
                annex.advance(n);
                maybe_emit_plain(strand, annex.global, &annex.bar, &annex.plain, false).await
            })
            .method("update", async move |this, strand, args, _out| {
                check_closed(strand, &this.annex().closed)?;
                let ([], [icon, message, total, position, delta, units]) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    icon_kw = None,
                    message_kw = None,
                    total_kw = None,
                    position_kw = None,
                    delta_kw = None,
                    units_kw = None
                )?;

                if position.is_some() && delta.is_some() {
                    return Err(Error::value(
                        strand,
                        "update: position: and delta: are exclusive",
                    ));
                }

                // Message/icon/units changes always go through immediately
                // in plain mode, even mid-debounce — they change what is
                // being displayed, not just its progress. Position/delta/
                // total stay rate-limited.
                let force = icon.is_some() || message.is_some() || units.is_some();

                if let Some(v) = icon {
                    let icon = v
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "icon: expected `Str`"))?
                        .to_string();
                    this.annex().bar.set_prefix(icon);
                }
                if let Some(v) = message {
                    let msg = v
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "message: expected `Str`"))?
                        .to_string();
                    this.annex().bar.set_message(msg);
                }
                if let Some(v) = total {
                    let annex = this.annex();
                    if v.is_nil() {
                        // Switch to spinner mode
                        annex.bar.unset_length();
                        if let Some(state_rc) = &annex.state_rc {
                            let mut state = state_rc.borrow_mut();
                            if let Some(idx) = state.find_widget_idx(annex.widget_id)
                                && state.widgets[idx].mode != Mode::Spinner
                            {
                                state.widgets[idx].mode = Mode::Spinner;
                                let leaf = is_leaf(&state.widgets, idx);
                                let w = &state.widgets[idx];
                                style::apply_spinner_style(
                                    &w.bar,
                                    &state.style,
                                    w.depth - 1,
                                    w.units,
                                    leaf,
                                    state.ansi,
                                );
                            }
                        }
                    } else {
                        annex.bar.set_length(v.to_u64(strand)?);
                        if let Some(state_rc) = &annex.state_rc {
                            let mut state = state_rc.borrow_mut();
                            if let Some(idx) = state.find_widget_idx(annex.widget_id)
                                && state.widgets[idx].mode != Mode::Bar
                            {
                                state.widgets[idx].mode = Mode::Bar;
                                let w = &state.widgets[idx];
                                style::apply_bar_style(
                                    &w.bar,
                                    &state.style,
                                    w.depth - 1,
                                    w.units,
                                    state.ansi,
                                );
                            }
                        }
                    }
                }
                if let Some(v) = position {
                    let pos = v.to_u64(strand)?;
                    this.annex().set_position(pos);
                }
                if let Some(v) = delta {
                    let n = v
                        .to_i64(strand)
                        .map_err(|_| Error::type_error(strand, "delta: expected `Int`"))?;
                    this.annex().advance(n);
                }
                if let Some(v) = units {
                    let units = parse_units(strand, Some(&v), this.annex().global)?;
                    let annex = this.annex();
                    if let Some(state_rc) = &annex.state_rc {
                        let mut state = state_rc.borrow_mut();
                        if let Some(idx) = state.find_widget_idx(annex.widget_id) {
                            let leaf = is_leaf(&state.widgets, idx);
                            let style = state.style.clone();
                            let ansi = state.ansi;
                            let widget = &mut state.widgets[idx];
                            widget.units = units;
                            match widget.mode {
                                Mode::Bar => style::apply_bar_style(
                                    &widget.bar,
                                    &style,
                                    widget.depth - 1,
                                    units,
                                    ansi,
                                ),
                                Mode::Spinner => style::apply_spinner_style(
                                    &widget.bar,
                                    &style,
                                    widget.depth - 1,
                                    units,
                                    leaf,
                                    ansi,
                                ),
                            }
                        }
                    }
                    if let Some(info) = &annex.plain {
                        info.set_units(units);
                    }
                }

                let annex = this.annex();
                maybe_emit_plain(strand, annex.global, &annex.bar, &annex.plain, force).await
            })
    }
}
