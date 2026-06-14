#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(content) = dolang_bytecode::file::deserialize(data) {
        assert!(!content.functab.content.is_empty());
        assert!(
            content
                .functab
                .content
                .iter()
                .all(|entry| !entry.func.bytecode.is_empty())
        );
        assert!(std::str::from_utf8(content.debugbintab.content).is_ok());
        assert!(
            content.funcdebugtab.content.is_empty()
                || content.funcdebugtab.content.len() == content.functab.content.len()
        );
    }
});
