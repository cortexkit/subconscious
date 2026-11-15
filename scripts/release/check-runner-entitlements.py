#!/usr/bin/env python3
"""Refuse a signed runner whose entitlements exceed the JIT permission."""
import plistlib
import sys


def check_entitlements(data):
    entitlements = plistlib.loads(data)
    key = "com.apple.security.cs.allow-jit"
    if entitlements != {key: True} or type(entitlements[key]) is not bool:
        raise ValueError("runner must carry exactly com.apple.security.cs.allow-jit=true")


if __name__ == "__main__":
    try:
        check_entitlements(sys.stdin.buffer.read())
    except (ValueError, plistlib.InvalidFileException) as error:
        sys.exit(f"refusing runner entitlements: {error}")
