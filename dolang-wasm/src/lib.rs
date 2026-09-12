mod analysis;

use dolang::{
    compile::Config,
    extension::Extension,
    runtime::{Arg, Bytecode, Frame, vm::Builder},
};
use js_sys::Function;
use serde::Serialize;
use std::{cell::RefCell, path::Path, rc::Rc};
use wasm_bindgen::prelude::*;

#[derive(Default, Serialize)]
struct RunResult {
    output: String,
    result: Option<String>,
    error: Option<String>,
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
    dolang_ext_load::LoadExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_rand::RandExt
        .apply_compiler(&mut config)
        .unwrap();
    dolang_ext_regex::RegexExt
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
        .prelude()
        .import_items("playground")
        .items(["echo"])
        .commit();
    config
}

async fn execute(source: String, on_output: &Function) -> RunResult {
    let mut response = RunResult::default();
    let config = config();
    let unit = config.unit(Path::new("playground.dol"), source.as_bytes());
    response.diagnostics = analysis::diagnostics(&unit, &source);
    let mut bytes = Vec::new();
    if let Err(error) = unit.emit(&mut bytes) {
        response.error = Some(error.to_string());
        return response;
    }
    let output = Rc::new(RefCell::new(String::new()));
    let captured = output.clone();
    let on_output = on_output.clone();
    let result = Builder::build(async move |builder| {
        dolang_ext_base64::Base64Ext.apply_vm(builder).unwrap();
        dolang_ext_compile::CompileExt.apply_vm(builder).unwrap();
        dolang_ext_digest::DigestExt.apply_vm(builder).unwrap();
        dolang_ext_glob::GlobExt.apply_vm(builder).unwrap();
        dolang_ext_load::LoadExt.apply_vm(builder).unwrap();
        dolang_ext_rand::RandExt.apply_vm(builder).unwrap();
        dolang_ext_regex::RegexExt.apply_vm(builder).unwrap();
        dolang_ext_toml::TomlExt.apply_vm(builder).unwrap();
        dolang_ext_url::UrlExt.apply_vm(builder).unwrap();
        dolang_ext_uuid::UuidExt.apply_vm(builder).unwrap();
        dolang_ext_xml::XmlExt.apply_vm(builder).unwrap();
        dolang_ext_json::JsonExt.apply_vm(builder).unwrap();
        dolang_ext_yaml::YamlExt.apply_vm(builder).unwrap();
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
                captured.borrow_mut().push_str(&text);
                let _ = on_output.call1(&JsValue::NULL, &JsValue::from_str(&text));
                Ok(())
            })
            .commit();
        builder
            .enter_with_slots(async move |strand, [mut out]| {
                match Bytecode::new(bytes)
                    .run(strand, &mut out)
                    .await
                    .and_then(|_| out.to_string(strand))
                {
                    Ok(value) => Ok(value),
                    Err(error) => {
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
                        Err(message)
                    }
                }
            })
            .await
    })
    .await;
    response.output = output.borrow().clone();
    match result {
        Ok(value) => response.result = Some(value),
        Err(error) => response.error = Some(error),
    }
    response
}

#[wasm_bindgen]
pub async fn run(source: String, on_output: Function) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(&execute(source, &on_output).await).map_err(Into::into)
}

#[wasm_bindgen]
pub fn analyze(source: &str) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(&analysis::analyze(source)).map_err(Into::into)
}

#[cfg(all(test, target_family = "wasm"))]
pub mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    async fn run_source(source: &str) -> RunResult {
        execute(source.into(), &Function::new_no_args("")).await
    }

    #[wasm_bindgen_test]
    async fn results_and_fresh_state() {
        assert_eq!(run_source("(1 + 2)").await.result.as_deref(), Some("3"));
        assert_eq!(
            run_source("let x = 9\nx").await.result.as_deref(),
            Some("9")
        );
        assert!(run_source("x").await.error.is_some());
        assert_eq!(
            run_source("(1.25 + 2.5)").await.result.as_deref(),
            Some("3.75")
        );
        assert_eq!(
            run_source("(1099511627776 + 1)").await.result.as_deref(),
            Some("1099511627777")
        );
    }

    #[wasm_bindgen_test]
    async fn echo_prints_key_arguments() {
        let result = run_source("echo status: ready count: 3").await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.output, "status: ready count: 3\n");
    }

    #[wasm_bindgen_test]
    async fn output_streams_incrementally() {
        use wasm_bindgen::{JsCast, closure::Closure};

        let chunks = Rc::new(RefCell::new(Vec::new()));
        let captured = chunks.clone();
        let on_output = Closure::wrap(Box::new(move |chunk: JsValue| {
            captured.borrow_mut().push(chunk.as_string().unwrap());
        }) as Box<dyn FnMut(JsValue)>);
        let result = execute(
            "echo one\necho two".into(),
            on_output.as_ref().unchecked_ref(),
        )
        .await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.output, "one\ntwo\n");
        assert_eq!(
            *chunks.borrow(),
            vec!["one\n".to_string(), "two\n".to_string()]
        );
    }

    #[wasm_bindgen_test]
    async fn errors_preserve_output() {
        let result = run_source("echo hello 42\nthrow std.RuntimeError \"oops\"").await;
        assert_eq!(result.output, "hello 42\n");
        assert!(result.error.unwrap().contains("oops"));
        let result = run_source("let =").await;
        assert!(result.error.is_some());
        assert!(!result.diagnostics.is_empty());
    }

    #[wasm_bindgen_test]
    async fn browser_extensions() {
        let result = run_source(include_str!("../../playground/tests/extensions.dol")).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.result.as_deref(), Some("extensions passed"));
    }

    #[wasm_bindgen_test]
    async fn extensions_and_pipeline() {
        for module in ["json", "yaml"] {
            let result = run_source(&format!(
                "import {module}\n{module}.decode ({module}.encode [1, 2, 3])"
            ))
            .await;
            assert!(result.error.is_none(), "{:?}", result.error);
            assert_eq!(result.result.as_deref(), Some("[1, 2, 3]"));
        }
        let result = run_source(
            "import strand:\n  - from\n  - each\n  - collect\npipeline\n  do from [1, 2, 3]\n  do each do |x| (x * 2)\n  do collect()",
        ).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.result.as_deref(), Some("[2, 4, 6]"));
    }
}
