#![forbid(unsafe_code)]

// Share the module behavior, but require test-support on this binary target so
// its privacy reporter cannot disappear when another test rebuilds fake-aft-stub.
include!("../../src/bin/fake_aft_stub/body.rs");
