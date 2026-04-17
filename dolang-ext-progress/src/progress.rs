use std::cell::Cell;
use std::cell::RefCell;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use dolang::runtime::{
    Error, Instance, Object, Output, Result, State, Strand, Value, call,
    error::ResultExt,
    object::TypeBuilder,
    strand::{self, Local},
    unpack,
    vm::Builder,
};
use dolang_ext_shell::with_terminal;
use indicatif as ix;
use ix::MultiProgress;
use tokio::io::AsyncWrite;

use crate::global::Global;
use crate::style::{self, DEFAULT_ICON, Mode, Style, StyleKeys, Units};

// --- Strand-local state ---

pub(crate) struct ProgressLocal {
    depth: Cell<u16>,
    parent_id: Cell<u64>,
    state: RefCell<Option<Rc<RefCell<ProgressState>>>>,
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

    fn inherit(&self, _strand: &strand::Strand<'v, '_>) -> Self {
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
        style::apply_spinner_style(&pw.bar, &state.style, pw.depth - 1, pw.units, false);
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
            style::apply_spinner_style(&pw.bar, &state.style, pw.depth - 1, pw.units, true);
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
) -> Result<'v, 's, Option<Units>> {
    match units_val {
        Some(v) => {
            if let Some(sym) = v.as_sym(strand) {
                match sym.as_str(strand) {
                    "count" => Ok(Some(Units::Count)),
                    "bytes" => Ok(Some(Units::Bytes)),
                    _ => Err(Error::value(strand, "units: expected :count: or :bytes:")),
                }
            } else if let Some(s) = v.as_str(strand) {
                match s {
                    "count" => Ok(Some(Units::Count)),
                    "bytes" => Ok(Some(Units::Bytes)),
                    _ => Err(Error::value(
                        strand,
                        "units: expected \"count\" or \"bytes\"",
                    )),
                }
            } else {
                Err(Error::type_error(strand, "units: expected `sym` or `str`"))
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
            .ok_or_else(|| Error::type_error(strand, "icon: expected `str`"))?
            .to_owned()),
        None => Ok(DEFAULT_ICON.to_owned()),
    }
}

fn apply_message<'v, 's>(
    strand: &mut Strand<'v, 's>,
    pb: &ix::ProgressBar,
    message: Option<&Value<'v>>,
) -> Result<'v, 's, ()> {
    if let Some(msg) = message {
        let msg = msg
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "message: expected `str`"))?;
        pb.set_message(msg.to_owned());
    }
    Ok(())
}

fn to_u64<'v, 's>(strand: &mut Strand<'v, 's>, v: i64) -> Result<'v, 's, u64> {
    u64::try_from(v).map_err(|_| Error::overflow(strand))
}

fn parse_tick<'v, 's>(
    strand: &mut Strand<'v, 's>,
    tick_val: Option<&Value<'v>>,
) -> Result<'v, 's, Duration> {
    match tick_val {
        Some(v) => {
            let secs = v
                .as_f64(strand)
                .ok_or_else(|| Error::type_error(strand, "tick: expected `float`"))?;
            Ok(Duration::from_secs_f64(secs))
        }
        None => Ok(Duration::from_secs_f64(0.08)),
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

// --- VM configuration ---

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let style_kw = builder.sym("style");
    let total_kw = builder.sym("total");
    let message_kw = builder.sym("message");
    let icon_kw = builder.sym("icon");
    let units_kw = builder.sym("units");
    let tick_kw = builder.sym("tick");
    let style_keys = StyleKeys {
        bar: builder.sym("bar"),
        spinner: builder.sym("spinner"),
        message: message_kw,
        icon: icon_kw,
        elapsed: builder.sym("elapsed"),
        position: builder.sym("position"),
        total: total_kw,
        width: builder.sym("width"),
        fg: builder.sym("fg"),
        bg: builder.sym("bg"),
        attrs: builder.sym("attrs"),
        alt: builder.sym("alt"),
    };

    builder
        .module("progress")
        .function("with", async move |strand, args, mut out| {
            let ([func], [style_val]) = unpack!(strand, args, 1, 0, style_kw = None)?;

            // If stderr is not a terminal, skip progress setup entirely
            if !dolang_ext_shell::is_terminal() {
                return call!(strand, &func, &mut out).await;
            }

            let style = match style_val {
                Some(sv) => style::parse_style(strand, &sv, &style_keys)?,
                None => Style::default(),
            };

            let multi = MultiProgress::new();
            let state_rc = Rc::new(RefCell::new(ProgressState {
                multi: Some(multi.clone()),
                style,
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
            let prev_state = local.state.replace(Some(state_rc.clone()));

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

            let units = parse_units(strand, units_val.as_deref())?;
            let icon_str = parse_icon(strand, icon_val.as_deref())?;
            let tick_interval = parse_tick(strand, tick_ms.as_deref())?;

            // Determine mode and total from the total kwarg
            let (mode, total_n) = match &total_val {
                Some(v) => {
                    let n = v
                        .as_i64(strand)
                        .ok_or_else(|| Error::type_error(strand, "total: expected `int`"))?;
                    (Mode::Bar, Some(to_u64(strand, n)?))
                }
                None => (Mode::Spinner, None),
            };

            let local = global.local.get(strand);
            let state_rc = local.state.borrow().clone();

            // Set up the progress bar and optional multi-progress tracking
            struct MultiState {
                multi: MultiProgress,
                state_rc: Rc<RefCell<ProgressState>>,
                widget_id: u64,
                prev_depth: u16,
                prev_parent_id: u64,
            }

            let (pb, multi_state) = match state_rc {
                None => {
                    // Outside progress.with: dummy hidden indicator
                    let pb = ix::ProgressBar::hidden();
                    if let Some(n) = total_n {
                        pb.set_length(n);
                    }
                    (pb, None)
                }
                Some(state_rc) => {
                    let multi = get_multi(strand, &state_rc)?;
                    let local = global.local.get(strand);
                    let depth = local.depth.get();
                    let parent_id = local.parent_id.get();

                    let pb_init = match total_n {
                        Some(n) => ix::ProgressBar::new(n),
                        None => ix::ProgressBar::new_spinner(),
                    };

                    let (pb, widget_id) = {
                        let mut state = state_rc.borrow_mut();
                        let (pb, widget_id) = do_insert_bar(
                            &mut state, &multi, parent_id, depth, pb_init, mode, units,
                        );

                        // Apply initial style (new indicator is always a leaf)
                        match mode {
                            Mode::Bar => style::apply_bar_style(&pb, &state.style, depth, units),
                            Mode::Spinner => {
                                style::apply_spinner_style(&pb, &state.style, depth, units, true);
                            }
                        }
                        drop(state);
                        (pb, widget_id)
                    };

                    // Update strand-local nesting for the callback scope
                    local.depth.set(depth + 1);
                    local.parent_id.set(widget_id);

                    let ms = MultiState {
                        multi,
                        state_rc,
                        widget_id,
                        prev_depth: depth,
                        prev_parent_id: parent_id,
                    };
                    (pb, Some(ms))
                }
            };

            pb.set_prefix(icon_str);
            pb.enable_steady_tick(tick_interval);
            apply_message(strand, &pb, msg_val.as_deref())?;

            let annex = IndicatorAnnex {
                bar: pb.clone(),
                state_rc: multi_state.as_ref().map(|ms| ms.state_rc.clone()),
                widget_id: multi_state.as_ref().map(|ms| ms.widget_id).unwrap_or(0),
                closed: Cell::new(false),
            };

            global
                .types
                .indicator
                .create_with_annex(strand, Indicator, annex, &mut slot);

            let res = call!(strand, &func, &mut out, &slot).await;

            // Mark closed
            global
                .types
                .indicator
                .downcast(&slot)
                .unwrap()
                .annex()
                .closed
                .set(true);

            // Clean up multi-progress state if we were inside progress.with
            if let Some(ms) = multi_state {
                let local = global.local.get(strand);
                local.depth.set(ms.prev_depth);
                local.parent_id.set(ms.prev_parent_id);

                if !pb.is_finished() {
                    pb.finish_and_clear();
                }
                let mut state = ms.state_rc.borrow_mut();
                do_remove(&mut state, &ms.multi, ms.widget_id);
            }

            res
        })
        .commit();
}

// --- Indicator ---

pub(crate) struct Indicator;

pub(crate) struct IndicatorAnnex {
    bar: ix::ProgressBar,
    state_rc: Option<Rc<RefCell<ProgressState>>>,
    widget_id: u64,
    closed: Cell<bool>,
}

impl<'v> Object<'v> for Indicator {
    const NAME: &'v str = "Indicator";
    const MODULE: &'v str = "progress";
    type Annex = IndicatorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<progress.Indicator>").into_do(strand)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            // --- Getters ---
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
                    Output::set(strand, out, n as i64);
                }
                Ok(())
            })
            .get("position", |this, strand, out| {
                check_closed(strand, &this.annex().closed)?;
                Output::set(strand, out, this.annex().bar.position() as i64);
                Ok(())
            })
            // --- Setters ---
            .set("message", |this, strand, value| {
                check_closed(strand, &this.annex().closed)?;
                let msg = value
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "message: expected `str`"))?;
                this.annex().bar.set_message(msg.to_owned());
                Ok(())
            })
            .set("icon", |this, strand, value| {
                check_closed(strand, &this.annex().closed)?;
                let icon = value
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "icon: expected `str`"))?;
                this.annex().bar.set_prefix(icon.to_owned());
                Ok(())
            })
            .set("total", |this, strand, value| {
                check_closed(strand, &this.annex().closed)?;
                let annex = this.annex();
                if value.is_nil() {
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
                            );
                        }
                    }
                } else {
                    let n = value.as_i64(strand).ok_or_else(|| {
                        Error::type_error(strand, "total: expected `int` or `nil`")
                    })?;
                    let n = to_u64(strand, n)?;
                    annex.bar.set_length(n);
                    if let Some(state_rc) = &annex.state_rc {
                        let mut state = state_rc.borrow_mut();
                        if let Some(idx) = state.find_widget_idx(annex.widget_id)
                            && state.widgets[idx].mode != Mode::Bar
                        {
                            state.widgets[idx].mode = Mode::Bar;
                            let w = &state.widgets[idx];
                            style::apply_bar_style(&w.bar, &state.style, w.depth - 1, w.units);
                        }
                    }
                }
                Ok(())
            })
            .set("position", |this, strand, value| {
                check_closed(strand, &this.annex().closed)?;
                let pos = value
                    .as_i64(strand)
                    .ok_or_else(|| Error::type_error(strand, "position: expected `int`"))?;
                let pos = to_u64(strand, pos)?;
                this.annex().bar.set_position(pos);
                Ok(())
            })
            // --- Methods ---
            .method("delta", async move |this, strand, args, _out| {
                check_closed(strand, &this.annex().closed)?;
                let ([], [amount]) = unpack!(strand, args, 0, 1)?;
                let n = match amount {
                    Some(v) => v
                        .as_i64(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected `int`"))?,
                    None => 1,
                };
                if n >= 0 {
                    this.annex().bar.inc(n as u64); // safe: n >= 0
                } else {
                    this.annex().bar.dec(n.unsigned_abs());
                }
                Ok(())
            })
    }
}
