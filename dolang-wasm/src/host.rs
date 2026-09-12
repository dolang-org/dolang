use dolang::runtime::{
    Arg, Error, Result, Strand,
    vm::{Builder, Stateful},
};
use js_sys::{Function, Object, Promise, Reflect};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen(typescript_custom_section)]
const HOST: &str = r#"
/**
 * Capabilities supplied to a run. Methods may return a `Promise`. `signal`
 * aborts when the VM abandons the call before it settles.
 */
export interface Host {
  echo(text: string, signal: AbortSignal): void | Promise<void>;
}
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type = "Host")]
    pub type Host;

    #[wasm_bindgen(method, catch)]
    fn echo(this: &Host, text: &str, signal: &AbortSignal)
    -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(typescript_type = "AbortSignal")]
    pub type AbortSignal;

    #[wasm_bindgen(method, getter)]
    fn aborted(this: &AbortSignal) -> bool;

    #[wasm_bindgen(method, js_name = addEventListener)]
    fn add_event_listener(this: &AbortSignal, kind: &str, listener: &Function, options: &Object);

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
            let options = Object::new();
            let _ = Reflect::set(&options, &"once".into(), &JsValue::TRUE);
            signal.add_event_listener("abort", &resolve, &options);
        }
    }))
}

pub(crate) fn configure(builder: &mut Builder<'_>, host: Host) {
    builder.register_state(HostState { host });
    builder
        .module("playground")
        .function("echo", async move |strand, args, _| {
            let mut line = Vec::new();
            for arg in args {
                match arg {
                    Arg::Pos(value) => line.push(value.to_verbatim(strand)?),
                    Arg::Key(key, value) => {
                        let key = key.as_str(strand).to_owned();
                        line.push(format!("{key}: {}", value.to_verbatim(strand)?));
                    }
                }
            }
            let mut text = line.join(" ");
            text.push('\n');
            upcall(strand, |host, signal| host.echo(&text, signal)).await?;
            Ok(())
        })
        .commit();
}
