use std::{
    collections::HashMap,
    fs,
    io::{Cursor, Read},
    sync::OnceLock,
};

const BUNDLE_MAGIC: &[u8; 8] = b"DZLBUNDL";
const BUNDLE_TRAILER_LEN: usize = 16;
const MODULE_PREFIX: &str = "module/";
const MODULE_SUFFIX: &str = ".dolc";

static MODULES: OnceLock<HashMap<String, Vec<u8>>> = OnceLock::new();

fn entry_module_name(name: &str) -> Option<&str> {
    name.strip_prefix(MODULE_PREFIX)
        .and_then(|name| name.strip_suffix(MODULE_SUFFIX))
}

fn parse_bundle() -> HashMap<String, Vec<u8>> {
    let exe = std::env::current_exe().expect("failed to locate current executable");
    let bytes = fs::read(exe).expect("failed to read current executable");
    let mut modules = HashMap::new();

    if bytes.len() < BUNDLE_TRAILER_LEN {
        return modules;
    }

    let trailer = &bytes[bytes.len() - BUNDLE_TRAILER_LEN..];
    if &trailer[8..] != BUNDLE_MAGIC {
        return modules;
    }

    let offset = u64::from_le_bytes(trailer[..8].try_into().unwrap());
    let offset = usize::try_from(offset).unwrap_or_else(|_| {
        panic!("invalid bundled stdlib offset in executable trailer: {offset}")
    });
    let zip_end = bytes.len() - BUNDLE_TRAILER_LEN;
    if offset > zip_end {
        panic!("invalid bundled stdlib offset in executable trailer: {offset}");
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[offset..zip_end]))
        .unwrap_or_else(|err| panic!("failed to open bundled stdlib ZIP: {err}"));

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .unwrap_or_else(|err| panic!("failed to read bundled stdlib entry #{index}: {err}"));
        let file_name = file.name().to_owned();
        let Some(module_name) = entry_module_name(&file_name) else {
            continue;
        };
        let mut entry = Vec::new();
        file.read_to_end(&mut entry)
            .unwrap_or_else(|err| panic!("failed to load bundled stdlib entry {file_name}: {err}"));
        modules.insert(module_name.to_owned(), entry);
    }

    modules
}

pub(crate) fn module(name: &str) -> Option<&'static [u8]> {
    MODULES
        .get_or_init(parse_bundle)
        .get(name)
        .map(|x| x.as_slice())
}
