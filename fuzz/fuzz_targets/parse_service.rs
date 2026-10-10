#![no_main]

use libfuzzer_sys::fuzz_target;

// Any byte sequence must parse or refuse — never panic, never hang. The
// name is fixed (names are validated separately and deterministically);
// the fuzzer owns only the file body. `%i` expansion, drop-in merging and
// the 1 MiB text ceiling all run inside `parse_service`, so they are all
// under fuzz. Both a plain and an instanced name, because the instance
// path is a second parser of the same bytes.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = core::str::from_utf8(data) {
        let _ = zconfig::parse_service("fuzz", text);
        let _ = zconfig::parse_service("fuzz@inst", text);
    }
});
