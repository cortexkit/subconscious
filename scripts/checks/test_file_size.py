#!/usr/bin/env python3
"""Focused tests for the source size checker."""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

# Loading the checker must not leave a __pycache__ folder in the checkout: CI fails
# when a build or test step adds untracked files to the tree.
sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("file_size", Path(__file__).with_name("check-file-size.py"))
assert spec is not None and spec.loader is not None
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class CounterTests(unittest.TestCase):
    def test_excludes_test_module_not_test_function(self):
        source = "fn production() {}\n#[cfg(test)]\nmod tests {\n fn test() {}\n}\n#[cfg(test)]\nfn helper() {}\n"
        self.assertEqual(checker.counted_lines(source, rust=True), 3)

    def test_braces_in_strings_and_nested_comments(self):
        source = '''fn before() {}
#[cfg(test)]
mod tests {
    let s = "} { \\\" }";
    /* } /* { */ } */
    fn test() { let c = '}'; }
}
fn after() {}
'''
        self.assertEqual(checker.counted_lines(source, rust=True), 2)

    def test_raw_strings_and_lifetimes(self):
        source = '''fn before<'a>(s: &'a str) {}
#[cfg(test)]
mod tests {
    let raw = r##"} "# {"##;
    let bytes = br#" } { "#;
}
fn after() {}
'''
        self.assertEqual(checker.counted_lines(source, rust=True), 2)

    def test_inline_module_keeps_production_line(self):
        self.assertEqual(checker.counted_lines("fn a() {} #[cfg(test)] mod tests {} fn b() {}\n", True), 1)

    def test_unclosed_module_fails(self):
        with self.assertRaises(ValueError):
            checker.counted_lines("#[cfg(test)] mod tests {\n", True)

    def test_source_selection(self):
        for path in ("crates/foo/src/lib.rs", "clients/x/src/index.ts", "scripts/gate.sh", "crates/foo/src/attestation.rs"):
            self.assertTrue(checker.is_source(path), path)
        for path in ("vectors/foo/generate.py", "crates/foo/fixtures/example.rs", "vendor/x.go", "foo.generated.ts", "Cargo.lock"):
            self.assertFalse(checker.is_source(path), path)

    def test_test_files_are_exempt(self):
        for path in ("crates/foo/tests/integration.rs", "crates/foo/src/config/tests.rs",
                     "crates/foo/src/admission_tests.rs", "clients/x/src/client.test.ts",
                     "clients/swift/Tests/FooTests/BarTests.swift", "scripts/checks/test_file_size.py"):
            self.assertFalse(checker.is_source(path), path)


class PinTests(unittest.TestCase):
    def test_growth_shrink_and_dead_pins(self):
        counts = {"growth.rs": 5002, "shrink.rs": 5001, "small.rs": 4999, "new.rs": 5000, "exact.rs": 5000}
        pins = {name: (count, "reason") for name, count in {
            "growth.rs": 5001, "shrink.rs": 5002, "small.rs": 5000, "gone.rs": 5000, "exact.rs": 5000
        }.items()}
        errors = "\n".join(checker.violations(counts, pins))
        for expected in ("growth.rs: 5002 lines exceeds pin 5001", "shrink.rs: 5001 lines below pin 5002",
                         "small.rs: 4999 lines, below 5000", "gone.rs: file absent or excluded",
                         "new.rs: 5000 lines, limit is under 5000"):
            self.assertIn(expected, errors)
        self.assertNotIn("exact.rs", errors)

    def test_pin_requires_reason_and_rejects_duplicates(self):
        with tempfile.TemporaryDirectory() as directory:
            pins = Path(directory) / "pins.txt"
            pins.write_text("a.rs 5000\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                checker.read_pins(pins)
            pins.write_text("a.rs 5000 reason\na.rs 5001 other reason\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                checker.read_pins(pins)


if __name__ == "__main__":
    unittest.main()
