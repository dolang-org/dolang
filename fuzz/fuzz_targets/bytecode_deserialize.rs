#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(content) = dolang_bytecode::file::deserialize(data) {
        let index = &content.index;
        assert!(!index.functab.content.is_empty());
        let main = content.main(data);
        assert!(index.functab.content.iter().all(|entry| {
            main.get(entry.func.bytecode.clone())
                .is_some_and(|b| !b.is_empty())
        }));
        assert!(std::str::from_utf8(content.debug(data)).is_ok());
        assert!(
            index.funcdebugtab.content.is_empty()
                || index.funcdebugtab.content.len() == index.functab.content.len()
        );
    }
});
