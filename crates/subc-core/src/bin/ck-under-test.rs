// The test-only `ck` target: the same program as the shipped `ck`, built only
// when integration tests ask for the fixture-key support. A separate source
// file exists so cargo does not see one file in two targets (it warns about
// that in every consumer's build log); `include!` keeps one program, which is
// why `ck.rs` carries no inner attributes or `//!` docs of its own.
include!("ck.rs");
