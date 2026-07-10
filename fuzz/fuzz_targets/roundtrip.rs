#![no_main]

//! Coverage-guided round-trip harness for the container writers.
//!
//! Contract under test: for ANY input that `parse_nsf` accepts,
//!
//! * `write_nsfe` must succeed (nothing a parsed header carries is
//!   unrepresentable in the chunk container), its output must
//!   re-parse, and a second write must be byte-identical (the writer
//!   emits a canonical encoding, so `write ∘ parse` is idempotent on
//!   writer output);
//! * `write_nsf` may reject exactly two lenient-parse artefacts — a
//!   fixed-header version outside 1..=2 and a string that does not
//!   fit the 31-byte v1 field (e.g. hoisted from an unbounded NSFe
//!   `auth` chunk) — and must otherwise succeed with the same
//!   re-parse + byte-idempotence guarantees.

use libfuzzer_sys::fuzz_target;
use oxideav_nsf::{parse_nsf, NsfWriteError};

fuzz_target!(|data: &[u8]| {
    let Ok(header) = parse_nsf(data) else {
        return;
    };

    let w1 = header
        .write_nsfe()
        .expect("write_nsfe must succeed for any parsed header");
    let h2 = parse_nsf(&w1).expect("write_nsfe output must re-parse");
    let w2 = h2.write_nsfe().expect("second write_nsfe must succeed");
    assert_eq!(w1, w2, "write_nsfe must be byte-idempotent");

    match header.write_nsf() {
        Ok(w1) => {
            let h2 = parse_nsf(&w1).expect("write_nsf output must re-parse");
            let w2 = h2.write_nsf().expect("second write_nsf must succeed");
            assert_eq!(w1, w2, "write_nsf must be byte-idempotent");
        }
        Err(NsfWriteError::BadVersion(_)) | Err(NsfWriteError::StringTooLong { .. }) => {}
        Err(e) => panic!("unexpected write_nsf failure on a parsed header: {e}"),
    }
});
