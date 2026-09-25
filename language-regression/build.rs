use std::env;
use std::fs::File;
use std::path::Path;

fn main() {
    let out_dir = Path::new(&env::var_os("OUT_DIR").unwrap()).to_owned();

    let mut f = File::create(out_dir.join("generated_tests.rs")).unwrap();
    dolang_private_build::generate_tests(&mut f, Path::new("tests/regression"));

    let mut f = File::create(out_dir.join("generated_token_tests.rs")).unwrap();
    dolang_private_build::generate_tests(&mut f, Path::new("tests/tokens"));

    let mut f = File::create(out_dir.join("generated_typeck_tests.rs")).unwrap();
    dolang_private_build::generate_case_tests(&mut f, Path::new("tests/typeck"), &["stub"]);
}
