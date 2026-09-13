mod analysis;
mod asserts;
mod host;

use dolang::{
    compile::Config,
    extension::Extension,
    runtime::{Bytecode, Frame, error::ErrorKind, vm::Builder},
};
use futures::future::{self, Either};
use serde::Serialize;
use std::{path::Path, pin::pin};
use wasm_bindgen::prelude::*;

pub use host::{AbortSignal, Host};

#[derive(Default, Serialize)]
struct RunResult {
    result: Option<String>,
    error: Option<String>,
    canceled: bool,
    diagnostics: Vec<analysis::Diagnostic>,
}

fn config() -> Config<'static> {
    let mut config = Config::new();
    dolang_ext_base64::Base64Ext
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_compile::CompileExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_digest::DigestExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_glob::GlobExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_http::HttpExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_load::LoadExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_rand::RandExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_regex::RegexExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_term::TermExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_time::TimeExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_toml::TomlExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_url::UrlExt.apply_compiler(&mut config).unwrap();
    dolang_ext_uuid::UuidExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_xml::XmlExt.apply_compiler(&mut config).unwrap();
    dolang_ext_json::JsonExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_yaml::YamlExt
        .apply_compiler(&mut config)
        .unwrap();
    config
}

async fn execute(source: String, host: Host, signal: AbortSignal) -> RunResult {
    let mut response = RunResult::default();
    let config = config();
    let unit = config.unit(Path::new("playground.dol"), source.as_bytes());
    response.diagnostics = analysis::diagnostics(&unit, &source);
    let mut bytes = Vec::new();
    if let Err(error) = unit.emit(&mut bytes) {
        response.error = Some(error.to_string());
        return response;
    }
    let result = Builder::build(async move |builder| {
        dolang_ext_base64::Base64Ext.apply_vm(builder).unwrap();
        dolang_ext_compile::CompileExt.apply_vm(builder).unwrap();
        dolang_ext_digest::DigestExt.apply_vm(builder).unwrap();
        dolang_ext_glob::GlobExt.apply_vm(builder).unwrap();
        dolang_ext_http::HttpExt.apply_vm(builder).unwrap();
        dolang_ext_load::LoadExt.apply_vm(builder).unwrap();
        dolang_ext_rand::RandExt.apply_vm(builder).unwrap();
        dolang_ext_regex::RegexExt.apply_vm(builder).unwrap();
        dolang_ext_term::TermExt.apply_vm(builder).unwrap();
        dolang_ext_time::TimeExt.apply_vm(builder).unwrap();
        dolang_ext_toml::TomlExt.apply_vm(builder).unwrap();
        dolang_ext_url::UrlExt.apply_vm(builder).unwrap();
        dolang_ext_uuid::UuidExt.apply_vm(builder).unwrap();
        dolang_ext_xml::XmlExt.apply_vm(builder).unwrap();
        dolang_ext_json::JsonExt.apply_vm(builder).unwrap();
        dolang_ext_yaml::YamlExt.apply_vm(builder).unwrap();
        host::configure(builder, host);
        asserts::configure(builder);
        builder
            .enter_with_slots(async move |strand, [mut out]| {
                let interrupt = strand.interrupt_token();
                let result = {
                    let run = pin!(async {
                        Bytecode::new(bytes)
                            .run(strand, &mut out)
                            .await
                            .and_then(|_| out.to_string(strand))
                    });
                    // Cancel on abort, then let the run unwind normally.
                    match future::select(run, host::aborted(&signal)).await {
                        Either::Left((result, _)) => result,
                        Either::Right((_, run)) => {
                            interrupt.cancel();
                            run.await
                        }
                    }
                };
                match result {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        let canceled = error.kind() == ErrorKind::Canceled;
                        let mut message = error.display(strand).to_string();
                        for frame in error.backtrace() {
                            message.push_str(&format!(
                                "\n  at {}::{}",
                                frame.module(),
                                frame.receiver()
                            ));
                            if let Some((path, line)) = frame.source() {
                                message.push_str(&format!(" ({path}:{})", line + 1));
                            }
                        }
                        Err((message, canceled))
                    }
                }
            })
            .await
    })
    .await;
    match result {
        Ok(value) => response.result = Some(value),
        Err((error, canceled)) => {
            response.error = Some(error);
            response.canceled = canceled;
        }
    }
    response
}

/// Runs `source` in a fresh VM. Aborting `signal` cancels the run.
#[wasm_bindgen]
pub async fn run(source: String, host: Host, signal: AbortSignal) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(&execute(source, host, signal).await).map_err(Into::into)
}

#[wasm_bindgen]
pub fn analyze(source: &str) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(&analysis::analyze(source)).map_err(Into::into)
}

#[cfg(all(test, target_family = "wasm"))]
pub mod tests {
    use super::*;
    use js_sys::{Array, Function, Reflect};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::wasm_bindgen_test;

    /// Evaluates a JavaScript function body.
    fn js<T: JsCast>(body: &str) -> T {
        Function::new_no_args(body)
            .call0(&JsValue::NULL)
            .unwrap()
            .unchecked_into()
    }

    fn field(object: &JsValue, name: &str) -> JsValue {
        Reflect::get(object, &name.into()).unwrap()
    }

    /// A host whose `write` records decoded chunks synchronously.
    fn recording_host() -> Host {
        js(
            "const chunks = []; const decoder = new TextDecoder(); return { chunks, write(data) { chunks.push(decoder.decode(data, { stream: true })); } };",
        )
    }

    fn chunks(host: &Host) -> Vec<String> {
        Array::from(&field(host, "chunks"))
            .iter()
            .map(|chunk| chunk.as_string().unwrap())
            .collect()
    }

    fn never_aborted() -> AbortSignal {
        host::AbortController::new().signal()
    }

    async fn run_with(source: &str, host: &Host, signal: AbortSignal) -> (RunResult, String) {
        let owned = host.unchecked_ref::<JsValue>().clone().unchecked_into();
        let result = execute(source.into(), owned, signal).await;
        (result, chunks(host).concat())
    }

    async fn run_source(source: &str) -> (RunResult, String) {
        run_with(source, &recording_host(), never_aborted()).await
    }

    #[wasm_bindgen_test]
    async fn results_and_fresh_state() {
        assert_eq!(run_source("(1 + 2)").await.0.result.as_deref(), Some("3"));
        assert_eq!(
            run_source("let x = 9\nx").await.0.result.as_deref(),
            Some("9")
        );
        assert!(run_source("x").await.0.error.is_some());
        assert_eq!(
            run_source("(1.25 + 2.5)").await.0.result.as_deref(),
            Some("3.75")
        );
        assert_eq!(
            run_source("(1099511627776 + 1)").await.0.result.as_deref(),
            Some("1099511627777")
        );
    }

    #[wasm_bindgen_test]
    async fn echo_prints_key_arguments() {
        let (result, output) = run_source("echo status: ready count: 3").await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(output, "status: ready count: 3\n");
    }

    #[wasm_bindgen_test]
    async fn echo_calls_host_per_line() {
        let host = recording_host();
        let (result, _) = run_with("echo one\necho two", &host, never_aborted()).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(chunks(&host), ["one\n", "two\n"]);
    }

    #[wasm_bindgen_test]
    async fn term_console_styles_output() {
        let (result, output) = run_source(
            r#"import term
let warning = term.text warning bold: true
echo $warning
print a b
term.console.write c b"d\n"
[term.console.can_style, term.console.is_tty, term.console.geometry(), term.output() == term.console]"#,
        )
        .await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.result.as_deref(), Some("[true, false, nil, true]"));
        assert_eq!(output, "\x1b[1mwarning\x1b[0m\nabcd\n");
    }

    #[wasm_bindgen_test]
    async fn echo_awaits_host_promise() {
        // The first call settles last; awaiting keeps the output in order.
        let host: Host = js(r#"
            const chunks = [];
            let delay = 20;
            return {
              chunks,
              write(data) {
                const text = new TextDecoder().decode(data);
                const ms = delay;
                delay = 0;
                return new Promise(resolve => setTimeout(() => { chunks.push(text); resolve(); }, ms));
              },
            };
        "#);
        let (result, output) = run_with("echo one\necho two", &host, never_aborted()).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(output, "one\ntwo\n");
    }

    #[wasm_bindgen_test]
    async fn host_rejection_is_do_error() {
        let host: Host = js(
            "return { chunks: [], write() { return Promise.reject(new Error('host refused')); } };",
        );
        let (result, _) = run_with("echo hi", &host, never_aborted()).await;
        assert!(result.error.unwrap().contains("host refused"));
        let (result, _) = run_with(
            "try\n  echo hi\n  false\ncatch _\n  true",
            &host,
            never_aborted(),
        )
        .await;
        assert_eq!(result.result.as_deref(), Some("true"));
    }

    #[wasm_bindgen_test]
    async fn errors_preserve_output() {
        let (result, output) = run_source("echo hello 42\nthrow std.RuntimeError \"oops\"").await;
        assert_eq!(output, "hello 42\n");
        assert!(result.error.unwrap().contains("oops"));
        assert!(!result.canceled);
        let (result, _) = run_source("let =").await;
        assert!(result.error.is_some());
        assert!(!result.diagnostics.is_empty());
    }

    #[wasm_bindgen_test]
    async fn abort_cancels_and_unwinds() {
        let host = recording_host();
        let (result, output) = run_with(
            "import time\ntry\n  time.sleep 10000\nfinally\n  echo cleanup",
            &host,
            js("const c = new AbortController(); setTimeout(() => c.abort(), 50); return c.signal;"),
        )
        .await;
        assert!(result.canceled, "{:?}", result.error);
        assert!(result.error.unwrap().contains("canceled"));
        assert_eq!(output, "cleanup\n");
    }

    #[wasm_bindgen_test]
    async fn abort_signals_pending_upcall() {
        let host: Host = js(r#"
            const state = { chunks: [], aborted: false };
            state.write = (data, signal) => {
              signal.addEventListener('abort', () => { state.aborted = true; });
              return new Promise(() => {});
            };
            return state;
        "#);
        let (result, _) = run_with("echo hang", &host, js("const c = new AbortController(); setTimeout(() => c.abort(), 50); return c.signal;")).await;
        assert!(result.canceled, "{:?}", result.error);
        assert_eq!(field(&host, "aborted"), JsValue::TRUE);
    }

    #[wasm_bindgen_test]
    async fn browser_extensions() {
        let (result, _) = run_source(include_str!("../../playground/tests/extensions.dol")).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.result.as_deref(), Some("extensions passed"));
    }

    #[wasm_bindgen_test]
    async fn http_rejects_unsupported_client_options() {
        for option in [
            "unix_socket",
            "proxy",
            "cookies",
            "ca_cert",
            "identity",
            "password",
            "invalid_certs",
        ] {
            let (result, _) = run_source(&format!("import http\nhttp.Client {option}: nil")).await;
            let error = result.error.expect(option);
            assert!(
                error.contains(&format!("{option} is not supported in the browser")),
                "{error}"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn test_module_asserts() {
        let (result, _) = run_source(
            "import test\ntest.assert_ne 1 2\ntest.assert_type $std.Int 1\ntest.assert_throws $std.RuntimeError str: \"assertion failed: 1 == 2\" do\n  test.assert_eq 1 2\n\"ok\"",
        )
        .await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.result.as_deref(), Some("ok"));
        let (result, _) = run_source("import test\ntest.assert_eq 1 2 oops").await;
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("assertion failed: 1 == 2: oops")),
            "{:?}",
            result.error
        );
    }

    #[wasm_bindgen_test]
    async fn time_extension() {
        let (result, _) = run_source(
            r#"import time
def check ok message
  if (!ok)
    throw std.RuntimeError $message
let start = time.DateTime.now()
time.sleep 0.05
let elapsed = (time.DateTime.now() - start)
check (elapsed.nanos >= 40000000) "sleep returned after $elapsed"
check (time.timeout(1, do :done:) == :done:) "timeout did not return the block result"
let timed_out = try
  time.timeout 0.01 do
    time.sleep 10000
    false
catch std.TimedOutError: _
  true
check $timed_out "timeout did not interrupt sleep"
check (time.DateTime.from_unix(1).rfc() == "1970-01-01T00:00:01Z") "from_unix"
check (time.DateTime.now().unix_secs > 1700000000) "DateTime.now"
check (time.Date.today().year >= 2024) "Date.today"
check (time.Date.from_ymd(2024, 2, 29).rfc() == "2024-02-29") "Date.from_ymd"
"time passed""#,
        )
        .await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.result.as_deref(), Some("time passed"));
    }

    #[wasm_bindgen_test]
    async fn extensions_and_pipeline() {
        for module in ["json", "yaml"] {
            let (result, _) = run_source(&format!(
                "import {module}\n{module}.decode ({module}.encode [1, 2, 3])"
            ))
            .await;
            assert!(result.error.is_none(), "{:?}", result.error);
            assert_eq!(result.result.as_deref(), Some("[1, 2, 3]"));
        }
        let (result, _) = run_source(
            "import strand:\n  - from\n  - each\n  - collect\npipeline\n  do from [1, 2, 3]\n  do each do |x| (x * 2)\n  do collect()",
        ).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.result.as_deref(), Some("[2, 4, 6]"));
    }
}
