use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, Strand,
    object::TypeBuilder,
    unpack,
    value::{Root, TypeObject, View},
    vm::{Builder, Stateful},
};
use js_sys::{Function, Object as JsObject, Promise, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen(typescript_custom_section)]
const HOST: &str = r#"
/**
 * Capabilities supplied to a run. Methods may return a `Promise`. `signal`
 * aborts when the VM abandons the call before it settles.
 */
export interface Host {
  /**
   * Receives console output: UTF-8 with ANSI SGR styling. A write may end
   * partway through a character or escape sequence.
   */
  write(data: Uint8Array, signal: AbortSignal): void | Promise<void>;
}
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type = "Host")]
    pub type Host;

    #[wasm_bindgen(method, catch)]
    fn write(
        this: &Host,
        data: &Uint8Array,
        signal: &AbortSignal,
    ) -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(typescript_type = "AbortSignal")]
    pub type AbortSignal;

    #[wasm_bindgen(method, getter)]
    fn aborted(this: &AbortSignal) -> bool;

    #[wasm_bindgen(method, js_name = addEventListener)]
    fn add_event_listener(this: &AbortSignal, kind: &str, listener: &Function, options: &JsObject);

    pub(crate) type AbortController;

    #[wasm_bindgen(constructor)]
    pub(crate) fn new() -> AbortController;

    #[wasm_bindgen(method, getter)]
    pub(crate) fn signal(this: &AbortController) -> AbortSignal;

    #[wasm_bindgen(method)]
    fn abort(this: &AbortController);
}

struct HostState {
    host: Host,
}

struct Tag;

impl<'v> Stateful<'v> for HostState {
    type Tag = Tag;
}

/// Aborts an upcall's signal unless the call settled first.
struct AbortOnDrop(Option<AbortController>);

impl AbortOnDrop {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(controller) = self.0.take() {
            controller.abort();
        }
    }
}

fn js_error<'v, 's>(strand: &mut Strand<'v, 's>, error: JsValue) -> Error<'v, 's> {
    let message = match error.dyn_ref::<js_sys::Error>() {
        Some(error) => String::from(error.message()),
        None => error.as_string().unwrap_or_else(|| format!("{error:?}")),
    };
    Error::runtime(strand, message)
}

/// Calls a host method and awaits its result. Interrupting the strand drops
/// this future, which aborts the signal passed to the host.
async fn upcall<'v, 's>(
    strand: &mut Strand<'v, 's>,
    call: impl FnOnce(&Host, &AbortSignal) -> std::result::Result<JsValue, JsValue>,
) -> Result<'v, 's, JsValue> {
    let controller = AbortController::new();
    let signal = controller.signal();
    let guard = AbortOnDrop(Some(controller));
    let state = strand.state::<HostState>();
    let result = match call(&state.host, &signal) {
        Ok(value) => JsFuture::from(Promise::resolve(&value)).await,
        Err(error) => Err(error),
    };
    guard.disarm();
    result.map_err(|error| js_error(strand, error))
}

/// Resolves once `signal` aborts.
pub(crate) fn aborted(signal: &AbortSignal) -> JsFuture {
    JsFuture::from(Promise::new(&mut |resolve, _| {
        if signal.aborted() {
            let _ = resolve.call0(&JsValue::UNDEFINED);
        } else {
            let options = JsObject::new();
            let _ = Reflect::set(&options, &"once".into(), &JsValue::TRUE);
            signal.add_event_listener("abort", &resolve, &options);
        }
    }))
}

/// The host console, reachable as `term.console`. Output goes to the host's
/// `write`, which renders it on the page.
struct PlaygroundConsole;

impl<'v> Object<'v> for PlaygroundConsole {
    const NAME: &'v str = "Console";
    const MODULE: &'v str = "playground";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("write", async move |_this, strand, args, out| {
                let bytes = dolang_ext_term::write_data(strand, args)?;
                write_host(strand, &bytes).await?;
                Output::set(strand, out, bytes.len());
                Ok(())
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Ok(())
            })
            .get("line_ending", |_this, strand, out| {
                Output::set(strand, out, LINE_ENDING);
                Ok(())
            })
            // The page renders SGR styling.
            .get("can_style", |_this, strand, out| {
                Output::set(strand, out, true);
                Ok(())
            })
            // The page is not a terminal: it has no cursor to move.
            .get("is_tty", |_this, strand, out| {
                Output::set(strand, out, false);
                Ok(())
            })
            .method("geometry", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Ok(())
            })
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let bytes = match value.view(strand) {
            View::Str(value) => value.pin().as_bytes().to_vec(),
            View::Bin(value) => value.pin().to_vec(),
            _ => value.to_string(strand)?.into_bytes(),
        };
        write_host(strand, &bytes).await
    }
}

const LINE_ENDING: &str = "\n";

async fn write_host<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, ()> {
    // A copy, not a view of Wasm memory, which the host may keep past the call.
    let data = Uint8Array::from(bytes);
    upcall(strand, |host, signal| host.write(&data, signal)).await?;
    Ok(())
}

/// Registers the host and installs its console as `term`'s. `TermExt` must
/// already be applied.
pub(crate) fn configure(builder: &mut Builder<'_>, host: Host) {
    builder.register_state(HostState { host });
    let console_type = dolang_ext_term::console_type(builder);
    let console = builder
        .build_type::<PlaygroundConsole>((), ())
        .nominal_supertype(console_type)
        .build();
    let mut root = Root::new(builder);
    console.create(builder, PlaygroundConsole, &mut root);
    dolang_ext_term::install_console(builder, &*root, true, LINE_ENDING);
}
