use std::env;
use std::fs::File;
use std::path::Path;

fn main() {
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("generated_tests.rs");
    let mut f = File::create(&dest_path).unwrap();

    let test_dir = Path::new("tests/regression");
    dolang_private_build::generate_tests(&mut f, test_dir);
}
