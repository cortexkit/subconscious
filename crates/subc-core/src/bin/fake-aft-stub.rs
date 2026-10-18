#![forbid(unsafe_code)]

// The body is shared with the privacy-aft-stub test fixture, which includes the
// same file under its own binary target. `include!` cannot carry inner
// attributes, so each wrapper declares them.
include!("fake_aft_stub/body.rs");
