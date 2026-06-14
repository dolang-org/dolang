use std::{
    convert::Infallible,
    fs,
    ops::ControlFlow,
    path::{Path, PathBuf},
};

use dolang_compile::{Compiler, diag::Diag};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fuzz_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_dir = fuzz_dir
        .parent()
        .ok_or("fuzz crate should be inside the repository root")?;
    let seed_dir = fuzz_dir.join("seeds/bytecode");
    let corpus_dir = repo_dir.join("target/fuzz-corpus/bytecode_deserialize");

    fs::create_dir_all(&corpus_dir)?;

    let mut seed_paths = fs::read_dir(&seed_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    seed_paths.retain(|path| path.extension().is_some_and(|ext| ext == "dol"));
    seed_paths.sort();

    for seed_path in seed_paths {
        let source = fs::read_to_string(&seed_path)?;
        let bytecode = compile_seed(&seed_path, &source)?;
        let seed_name = seed_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or("seed file should have a UTF-8 stem")?;

        write_seed(&corpus_dir, seed_name, "valid", &bytecode)?;
        write_invalid_header_seed(&corpus_dir, seed_name, &bytecode)?;
        write_trailing_junk_seed(&corpus_dir, seed_name, &bytecode)?;
    }

    Ok(())
}

fn compile_seed(path: &Path, source: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytecode = Vec::new();
    let mut diagnostics = Vec::new();
    let compiler = Compiler::new(path, source.as_bytes());

    compiler
        .compile(&mut bytecode, &mut |diag: Diag| {
            diagnostics.push(diag);
            ControlFlow::<Infallible>::Continue(())
        })
        .map_err(|err| {
            let mut message = format!("failed to compile {}: {err}", path.display());
            for diag in &diagnostics {
                message.push_str(&format!("\n{}: {}", diag.severity(), diag.message()));
            }
            message
        })?;

    Ok(bytecode)
}

fn write_invalid_header_seed(
    corpus_dir: &Path,
    seed_name: &str,
    bytecode: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    if bytecode.is_empty() {
        return Ok(());
    }

    let mut invalid = bytecode.to_vec();
    invalid[0] ^= 0xff;
    write_seed(corpus_dir, seed_name, "invalid-header", &invalid)
}

fn write_trailing_junk_seed(
    corpus_dir: &Path,
    seed_name: &str,
    bytecode: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut invalid = bytecode.to_vec();
    invalid.extend_from_slice(b"junk");
    write_seed(corpus_dir, seed_name, "trailing-junk", &invalid)
}

fn write_seed(
    corpus_dir: &Path,
    seed_name: &str,
    variant: &str,
    bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(corpus_dir.join(format!("{seed_name}-{variant}")), bytes)?;
    Ok(())
}
