#![forbid(unsafe_code)]

// Both CLI binaries use the same implementation and forbid unsafe code.
#[path = "ck.rs"]
mod ck;

fn main() {
    ck::main();
}
