#![no_main]

//! Coverage-guided fuzz harness for `oxideav_nsf::parse_nsf`.
//!
//! `parse_nsf` dispatches an arbitrary byte slice to the NSF v1 / NSF v2
//! / NSFe decoder by sniffing the magic, then walks attacker-controlled
//! length + address + chunk-size fields. The in-tree
//! `tests/parse_fuzz.rs` covers a hand-enumerated battery; this target
//! adds coverage-guided exploration so the corpus minimiser reaches the
//! dispatch branches the fixed battery doesn't.
//!
//! Contract under test: every byte slice produces either
//! `Ok(NsfHeader)` or `Err(NsfError)`. Panics, debug-mode integer
//! overflows, allocator-overflowing length arithmetic, and
//! index-out-of-bounds are all bugs.

use libfuzzer_sys::fuzz_target;
use oxideav_nsf::parse_nsf;

fuzz_target!(|data: &[u8]| {
    let _ = parse_nsf(data);
});
