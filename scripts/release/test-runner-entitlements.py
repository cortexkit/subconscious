"""Check the allowlist against the entitlements extracted from a signed image."""
import importlib.util
from pathlib import Path
import plistlib
import unittest

spec = importlib.util.spec_from_file_location(
    "entitlements", Path(__file__).with_name("check-runner-entitlements.py")
)
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class RunnerEntitlements(unittest.TestCase):
    def test_jit_only(self):
        checker.check_entitlements(plistlib.dumps({"com.apple.security.cs.allow-jit": True}))

    def test_extra_entitlement_is_refused(self):
        for extra in ("disable-library-validation", "allow-dyld-environment-variables", "unknown"):
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                checker.check_entitlements(plistlib.dumps({
                    "com.apple.security.cs.allow-jit": True,
                    f"com.apple.security.cs.{extra}": True,
                }))

    def test_missing_false_or_malformed_is_refused(self):
        for value in ({}, {"com.apple.security.cs.allow-jit": False}, {"com.apple.security.cs.allow-jit": 1}):
            with self.subTest(value=value), self.assertRaises(ValueError):
                checker.check_entitlements(plistlib.dumps(value))
        with self.assertRaises(plistlib.InvalidFileException):
            checker.check_entitlements(b"not a plist")


if __name__ == "__main__":
    unittest.main()
