"""Compiles a python-build-standalone archive's pure-Python standard library into the runner's
resource table. build.rs runs it with CK_PYTHON_HOST, a CPython of the same minor version.

    pack_stdlib.py <extracted archive's python/ dir> <output table>

The table format is described in src/resources.rs. Modules the interpreter already has frozen
are left out, since its own frozen copies serve them, as are packages a script never needs.
"""
import _imp
import importlib.util
import json
import marshal
import os
import struct
import sys

# Packages the runner leaves out: tests, GUI and IDE code, and installers.
DROPPED = {"test", "idlelib", "tkinter", "turtledemo", "ensurepip", "lib2to3", "venv", "pydoc_data"}


def fail(message):
    sys.exit(f"pack_stdlib.py: {message}")


def main():
    dist, output = sys.argv[1:]
    with open(os.path.join(dist, "PYTHON.json"), encoding="utf-8") as stream:
        info = json.load(stream)
    version = f"{sys.version_info[0]}.{sys.version_info[1]}"
    if version != info["python_major_minor_version"]:
        fail(f"CK_PYTHON_HOST is Python {version}; the archive is {info['python_major_minor_version']}")
    if importlib.util.MAGIC_NUMBER.hex() != info["python_bytecode_magic_number"]:
        fail("CK_PYTHON_HOST writes bytecode the archive's interpreter can't read")
    stdlib = os.path.join(dist, info["python_paths"]["stdlib"])
    entries = {}
    for directory, folders, files in os.walk(stdlib):
        relative = os.path.relpath(directory, stdlib)
        package = [] if relative == "." else relative.split(os.sep)
        # Only real packages (with __init__.py) below the top level; site-packages,
        # lib-dynload and config-* folders are not packages and are skipped with them.
        folders[:] = sorted(
            folder for folder in folders
            if folder.isidentifier() and not (not package and folder in DROPPED)
            and os.path.exists(os.path.join(directory, folder, "__init__.py"))
        )
        for file in sorted(files):
            stem, suffix = os.path.splitext(file)
            if suffix != ".py" or not stem.isidentifier():
                continue
            is_package = stem == "__init__"
            if is_package and not package:
                continue
            name = ".".join(package if is_package else package + [stem])
            if _imp.is_frozen(name):
                continue
            path = os.path.join(directory, file)
            with open(path, "rb") as stream:
                source = stream.read()
            shown = os.path.relpath(path, stdlib).replace(os.sep, "/")
            code = compile(source, f"<ck-stdlib:{shown}>", "exec", dont_inherit=True, optimize=0)
            entries[name] = (is_package, marshal.dumps(code))
    table = [b"CKPYLIB1", struct.pack("<I", len(entries))]
    for name in sorted(entries):
        is_package, data = entries[name]
        encoded = name.encode()
        table.append(struct.pack("<BHI", int(is_package), len(encoded), len(data)))
        table.append(encoded)
        table.append(data)
    with open(output, "wb") as stream:
        stream.write(b"".join(table))


if __name__ == "__main__":
    main()
