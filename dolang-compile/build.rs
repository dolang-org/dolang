use std::{env, fs::File, path::Path};

fn main() {
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("generated_token_tests.rs");
    let mut file = File::create(dest_path).unwrap();

    dolang_private_build::generate_tests(&mut file, Path::new("tests/tokens"));
}
