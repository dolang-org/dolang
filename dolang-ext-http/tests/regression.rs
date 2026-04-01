#![deny(warnings)]

mod detail {
    use dolang::{
        compile::{self, Compiler, Mode},
        runtime::{Bytecode, Error, vm::Builder},
    };
    use std::{
        io,
        ops::ControlFlow,
        path::{Path, PathBuf},
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers};
    extern crate dolang_ext_http;

    const MOD_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mod");

    fn compile<'a>(
        path: &'a Path,
        content: &'a [u8],
        module: Option<&str>,
    ) -> (
        Result<Bytecode, compile::Error<io::Error>>,
        Vec<dolang::compile::Diag>,
        Vec<dolang_private_test::Directive>,
    ) {
        let mut compiler = Compiler::new(path, content);
        dolang_private_test::apply_compiler_extensions(&mut compiler);
        let directives = dolang_private_test::configure_compiler(&mut compiler, content);
        // Add test module with TEST_URL and HAVE_JSON to prelude
        compiler
            .prelude()
            .import_module("test")
            .import_items("test")
            .items(["TEST_URL", "HAVE_JSON"])
            .commit();
        if let Some(name) = module {
            compiler.mode(Mode::Module { name });
        }
        let mut out = Vec::new();
        let mut diags = Vec::new();
        let res = compiler.compile(&mut out, &mut |diag| {
            diags.push(diag);
            ControlFlow::Continue(())
        });
        (res.map(|_| Bytecode::new(out)), diags, directives)
    }

    async fn register_stubs(server: &MockServer, name: &str) {
        match name {
            "basic" | "get" | "post" | "headers" | "json" | "errors" | "url_object"
            | "error_types" | "status_errors" => {
                Mock::given(matchers::path("/get"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/post"))
                    .respond_with(
                        ResponseTemplate::new(201).set_body_string(r#"{"received":"ok"}"#),
                    )
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/echo"))
                    .respond_with(ResponseTemplate::new(200))
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/api/users"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string(r#"[{"id":1,"name":"alice"},{"id":2,"name":"bob"}]"#),
                    )
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/headers"))
                    .respond_with(
                        ResponseTemplate::new(200).insert_header("x-custom", "test-value"),
                    )
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/not-found"))
                    .respond_with(
                        ResponseTemplate::new(404)
                            .insert_header("x-error", "missing")
                            .set_body_string("missing"),
                    )
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/error"))
                    .respond_with(
                        ResponseTemplate::new(500)
                            .insert_header("x-error", "boom")
                            .set_body_string(r#"{"error":"boom"}"#),
                    )
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/not-modified"))
                    .respond_with(ResponseTemplate::new(304))
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/large-error"))
                    .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(70000)))
                    .mount(server)
                    .await;
                Mock::given(matchers::path("/redirect-loop"))
                    .respond_with(
                        ResponseTemplate::new(302).insert_header("location", "/redirect-loop"),
                    )
                    .mount(server)
                    .await;
            }
            "date_headers" => {
                use wiremock::{Request, Respond};

                struct DateHeaders;

                impl Respond for DateHeaders {
                    fn respond(&self, request: &Request) -> ResponseTemplate {
                        let actual = request
                            .headers
                            .get("if-modified-since")
                            .and_then(|value| value.to_str().ok());
                        if actual == Some("Thu, 01 Jan 1970 00:16:40 GMT") {
                            ResponseTemplate::new(200)
                                .insert_header("last-modified", "Thu, 01 Jan 1970 00:16:40 GMT")
                                .insert_header("x-test", "ok")
                        } else {
                            ResponseTemplate::new(400)
                        }
                    }
                }

                Mock::given(matchers::path("/date-headers"))
                    .respond_with(DateHeaders)
                    .mount(server)
                    .await;
            }
            "status_codes" => {
                for &code in &[200, 201, 204, 400, 404, 500] {
                    let template = ResponseTemplate::new(code);
                    Mock::given(matchers::path(format!("/status/{}", code)))
                        .respond_with(template)
                        .mount(server)
                        .await;
                }
            }
            "streaming_request" => {
                use wiremock::{Request, Respond};

                struct EchoBody;

                impl Respond for EchoBody {
                    fn respond(&self, request: &Request) -> ResponseTemplate {
                        let body = request.body.clone();
                        ResponseTemplate::new(201).set_body_bytes(body)
                    }
                }

                Mock::given(matchers::path("/post"))
                    .respond_with(EchoBody)
                    .mount(server)
                    .await;
            }
            "sse" => {
                Mock::given(matchers::path("/events"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("content-type", "text/event-stream")
                            .set_body_bytes(
                                b": keep-alive\r\n\
event: token\r\n\
id: msg-1\r\n\
retry: 1500\r\n\
data: hello\r\n\
data: world\r\n\
\r\n\
data: plain\n\
\n\
event:\n\
: comment\n\
ignored: nope\n\
data: defaulted\n\
\n\
data: tail"
                                    .to_vec(),
                            ),
                    )
                    .mount(server)
                    .await;

                Mock::given(matchers::path("/events-invalid"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("content-type", "text/event-stream")
                            .set_body_bytes(vec![b'd', b'a', b't', b'a', b':', b' ', 0xff, b'\n']),
                    )
                    .mount(server)
                    .await;
            }
            "cookies" => {
                use wiremock::{Request, Respond};

                struct CookieFlow;

                impl Respond for CookieFlow {
                    fn respond(&self, request: &Request) -> ResponseTemplate {
                        match request.url.path() {
                            "/login" => ResponseTemplate::new(200)
                                .insert_header("set-cookie", "session=abc; Path=/; HttpOnly"),
                            "/session" => {
                                let authorized = request
                                    .headers
                                    .get("cookie")
                                    .and_then(|value| value.to_str().ok())
                                    .is_some_and(|value| value.contains("session=abc"));
                                if authorized {
                                    ResponseTemplate::new(200).set_body_string("ok")
                                } else {
                                    ResponseTemplate::new(401).set_body_string("missing session")
                                }
                            }
                            _ => ResponseTemplate::new(404),
                        }
                    }
                }

                Mock::given(matchers::any())
                    .respond_with(CookieFlow)
                    .mount(server)
                    .await;
            }
            _ => {}
        }
    }

    pub(super) fn run(path: &Path) {
        let test_name = path.file_stem().unwrap().to_string_lossy().into_owned();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = MockServer::start().await;
            let url = format!("http://{}", server.address());
            register_stubs(&server, &test_name).await;

            Builder::build(async |vm| {
                let test_state = dolang_private_test::configure_vm(vm);

                dolang_private_test::apply_vm_extensions(vm);

                // Create test module with TEST_URL and HAVE_JSON
                vm.module("test")
                    .value("TEST_URL", url.as_str())
                    .value("HAVE_JSON", cfg!(feature = "json"))
                    .commit();

                vm.importer(async move |strand, name, out| {
                    let path = format!("{}.dol", name);
                    let path = Path::new(&path);
                    let path: PathBuf = [Path::new(MOD_DIR), path].into_iter().collect();
                    if !path.exists() {
                        return Err(Error::import(strand, name));
                    }
                    let content = dolang_private_test::read_file(&path);
                    let (bytecode, _, _) = compile(&path, &content, Some(name));
                    let bytecode = bytecode.unwrap();
                    bytecode.run(strand, out).await
                });

                vm.enter_with_slots(async move |strand, [retval]| {
                    let content = dolang_private_test::read_file(path);
                    let (res, diags, directives) = compile(path, &content, None);
                    dolang_private_test::vm_run(
                        strand,
                        path,
                        &content,
                        res.ok(),
                        diags,
                        directives,
                        &test_state,
                        retval,
                    )
                    .await
                })
                .await;
            })
            .await
        })
    }
}

use detail::run;

include!(concat!(env!("OUT_DIR"), "/generated_tests.rs"));
