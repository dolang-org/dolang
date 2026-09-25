#![deny(warnings)]

//! Build script helpers for Do language.
//!
//! This crate provides utilities for build scripts to generate test code.
//! It is intended to be used as a build-dependency.

use std::{fs, io::Write, path::Path};

fn munge(s: String) -> String {
    s.replace(|c: char| !c.is_alphanumeric(), "_")
}

/// Check if a file should skip Miri by looking for `# skip: miri` in first 10 lines.
fn should_skip_miri(path: &Path) -> bool {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    for line in content.lines().take(10) {
        if line.contains("# skip: miri") {
            return true;
        }
    }
    false
}

/// Process a test directory and generate test code.
///
/// This recursively scans the directory for `.dol` files and generates
/// test functions for each one. It also handles subdirectories as modules.
///
/// Tests are skipped under Miri if:
/// - The test file contains `# skip: miri` in the first 10 lines, or
/// - A parent directory contains a `.directive` file with `# skip: miri` in the first 10 lines
pub fn generate_tests(out: &mut dyn Write, dir: &Path) {
    println!("cargo::rerun-if-changed={}", dir.display());
    process(out, dir, false);
}

fn process(out: &mut dyn Write, dir: &Path, parent_skip_miri: bool) {
    // Check if this directory has a .directive file that sets skip_miri
    let directive_path = dir.join(".directive");
    let skip_miri = parent_skip_miri || should_skip_miri(&directive_path);

    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let path_str = path.display().to_string().replace('\\', "/");
        if path.is_dir() {
            let mod_name = munge(path.file_name().unwrap().display().to_string());
            writeln!(out, "mod r#{mod_name} {{").unwrap();
            writeln!(out, "#[allow(unused_imports)]").unwrap();
            writeln!(out, "use super::run;").unwrap();
            process(out, &path, skip_miri);
            writeln!(out, "}}").unwrap();
        } else if path.extension() == Some("dol".as_ref()) {
            let test_name = munge(path.file_stem().unwrap().display().to_string());
            let file_skip_miri = skip_miri || should_skip_miri(&path);

            if file_skip_miri {
                writeln!(out, "#[cfg_attr(miri, ignore)]").unwrap();
            }
            writeln!(out, "#[test]").unwrap();
            writeln!(out, "fn r#{}() {{", test_name).unwrap();
            writeln!(out, "    run(std::path::Path::new(\"{path_str}\"));").unwrap();
            writeln!(out, "}}").unwrap();
            println!("cargo::rerun-if-changed={path_str}");
        }
    }
}

/// Generate a test for each case directory under `dir`.
///
/// A directory that directly contains `.dol` files is a case, and its test calls
/// `run` with the directory's path. Any other directory is a group of cases and
/// becomes a module. `skip` names top-level directories to leave out.
///
/// A `.test` file in a case or group directory holds `key: value` lines, and a
/// deeper file overrides a shallower one key by key. Cases with `skip: miri` are
/// skipped under Miri.
pub fn generate_case_tests(out: &mut dyn Write, dir: &Path, skip: &[&str]) {
    println!("cargo::rerun-if-changed={}", dir.display());
    for entry in sorted_entries(dir) {
        let name = entry.file_name().unwrap().display().to_string();
        if entry.is_dir() && !skip.contains(&name.as_str()) {
            process_case(out, &entry, test_setting(dir, "skip"));
        }
    }
}

fn process_case(out: &mut dyn Write, dir: &Path, skip: Option<String>) {
    let skip = test_setting(dir, "skip").or(skip);
    let name = munge(dir.file_name().unwrap().display().to_string());
    let entries = sorted_entries(dir);
    if entries
        .iter()
        .any(|path| path.extension() == Some("dol".as_ref()))
    {
        let path_str = dir.display().to_string().replace('\\', "/");
        if skip.as_deref() == Some("miri") {
            writeln!(out, "#[cfg_attr(miri, ignore)]").unwrap();
        }
        writeln!(out, "#[test]").unwrap();
        writeln!(out, "fn r#{name}() {{").unwrap();
        writeln!(out, "    run(std::path::Path::new(\"{path_str}\"));").unwrap();
        writeln!(out, "}}").unwrap();
    } else {
        writeln!(out, "mod r#{name} {{").unwrap();
        writeln!(out, "#[allow(unused_imports)]").unwrap();
        writeln!(out, "use super::run;").unwrap();
        for entry in entries.iter().filter(|path| path.is_dir()) {
            process_case(out, entry, skip.clone());
        }
        writeln!(out, "}}").unwrap();
    }
}

fn sorted_entries(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    entries
}

/// The value of `key` in a directory's `.test` file, if it sets one.
fn test_setting(dir: &Path, key: &str) -> Option<String> {
    let content = fs::read_to_string(dir.join(".test")).ok()?;
    content.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_owned())
    })
}
