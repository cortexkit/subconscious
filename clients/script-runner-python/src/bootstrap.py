"""Startup code of ck-script-runner-python, run by src/embed.c inside the embedded interpreter.

install() runs after the interpreter's core phase, when only built-in and frozen modules exist,
and puts the in-memory standard-library finder first on sys.meta_path, so the rest of startup
imports from memory. run() and check() run after the main phase: they remove every importer that
could read the disk, then run or check the script bytes the runner received.
"""

import _ck_runner
import _imp
import marshal
import sys

import _frozen_importlib as _bootstrap
import _frozen_importlib_external as _external

REFUSAL = "{} is not in the approved runner (no disk imports)"
MAIN = "ck-script:main"

# File names and magic numbers of native libraries. A bundle holding one is refused at load.
NATIVE_SUFFIXES = (".so", ".pyd", ".dll", ".dylib", ".sl")
MACHO_MAGICS = {
    b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca",
}


class Refused(Exception):
    """A script the runner won't run; the message is printed as is."""


class StdlibFinder:
    """Serves the standard library compiled into the runner (src/resources.rs)."""

    @classmethod
    def find_spec(cls, name, path=None, target=None):
        found = _ck_runner.resource(name)
        if found is None:
            return None
        return _bootstrap.ModuleSpec(name, cls, origin="ck-stdlib", is_package=found[0])

    @staticmethod
    def create_module(spec):
        return None

    @classmethod
    def exec_module(cls, module):
        exec(cls.get_code(module.__spec__.name), module.__dict__)

    @staticmethod
    def get_code(name):
        found = _ck_runner.resource(name)
        if found is None:
            raise ModuleNotFoundError(REFUSAL.format(name), name=name)
        return marshal.loads(found[1])

    @staticmethod
    def is_package(name):
        found = _ck_runner.resource(name)
        return bool(found and found[0])

    @staticmethod
    def get_source(name):
        return None


class RefusingFinder:
    """Last on sys.meta_path: a name nothing in memory provides is refused, never searched for."""

    @staticmethod
    def find_spec(name, path=None, target=None):
        raise ModuleNotFoundError(REFUSAL.format(name), name=name)


def is_native(path, data):
    base = path.rsplit("/", 1)[-1].lower()
    if base.endswith(NATIVE_SUFFIXES) or ".so." in base:
        return True
    if data[:4] == b"\x7fELF" or data[:4] in MACHO_MAGICS:
        return True
    if data[:2] == b"MZ" and len(data) >= 64:
        offset = int.from_bytes(data[60:64], "little")
        return data[offset:offset + 4] == b"PE\0\0"
    return False


def code_for(path, data, filename):
    if path.endswith(".pyc"):
        if data[:4] != _external.MAGIC_NUMBER:
            raise Refused(f"script_bundle_refused: {path} is bytecode for another Python version")
        return marshal.loads(data[16:])
    return compile(data, filename, "exec", dont_inherit=True)


class BundleFinder:
    """Serves the modules of an approved .pyz bundle from the piped bytes."""

    def __init__(self, entries):
        self.modules = {}
        for path, data in entries.items():
            parts = path.split("/")
            stem, _, suffix = parts[-1].rpartition(".")
            package = parts[:-1]
            if suffix not in ("py", "pyc") or "__pycache__" in package or not stem.isidentifier():
                continue
            if not all(part.isidentifier() for part in package):
                continue
            if stem == "__init__":
                if not package:
                    continue
                name, is_package = ".".join(package), True
            else:
                name, is_package = ".".join(package + [stem]), False
            # Source wins over sourceless bytecode for the same module.
            if name in self.modules and self.modules[name][1].endswith(".py"):
                continue
            self.modules[name] = (is_package, path, data)
        self.main = self.modules.pop("__main__", None)

    def find_spec(self, name, path=None, target=None):
        entry = self.modules.get(name)
        if entry is None:
            return None
        return _bootstrap.ModuleSpec(name, self, origin=f"ck-script:{entry[1]}", is_package=entry[0])

    def create_module(self, spec):
        return None

    def exec_module(self, module):
        exec(self.get_code(module.__spec__.name), module.__dict__)

    def get_code(self, name):
        _, path, data = self.modules[name]
        return code_for(path, data, f"ck-script:{path}")

    def is_package(self, name):
        return self.modules[name][0]

    def get_source(self, name):
        _, path, data = self.modules[name]
        return _external.decode_source(data) if path.endswith(".py") else None


class MainLoader:
    """Gives tracebacks the script's own lines; the script has no file to read them from."""

    def __init__(self, source):
        self.source = source

    def get_source(self, name):
        if name != "__main__" or self.source is None:
            return None
        return _external.decode_source(self.source)


def install():
    sys.meta_path.insert(0, StdlibFinder)


def lockdown():
    """Leaves only in-memory importers, ending with the one that refuses everything else."""
    kept = (StdlibFinder, _bootstrap.BuiltinImporter, _bootstrap.FrozenImporter)
    sys.meta_path[:] = [finder for finder in sys.meta_path if finder in kept] + [RefusingFinder]
    sys.path_hooks.clear()
    sys.path_importer_cache.clear()
    sys.path.clear()


def is_bundle(payload):
    # A zip archive ends with an end-of-central-directory record; Python source never does.
    if b"PK\x05\x06" not in payload[-(65535 + 22):]:
        return False
    import io
    import zipfile
    return zipfile.is_zipfile(io.BytesIO(payload))


def load_bundle(payload):
    import io
    import zipfile
    entries = {}
    try:
        with zipfile.ZipFile(io.BytesIO(payload)) as archive:
            for info in archive.infolist():
                if info.is_dir():
                    continue
                if is_native(info.filename, b""):
                    raise Refused(f"script_bundle_refused: native file {info.filename}")
                data = archive.read(info)
                if is_native(info.filename, data):
                    raise Refused(f"script_bundle_refused: native file {info.filename}")
                entries[info.filename] = data
    except (zipfile.BadZipFile, NotImplementedError, RuntimeError, ValueError) as error:
        raise Refused(f"script_bundle_refused: {error}") from None
    bundle = BundleFinder(entries)
    if bundle.main is None:
        raise Refused("script_bundle_refused: no __main__.py")
    return bundle


def run():
    lockdown()
    payload = _ck_runner.payload()
    try:
        if is_bundle(payload):
            bundle = load_bundle(payload)
            sys.meta_path.insert(1, bundle)
            _, path, data = bundle.main
            code = code_for(path, data, MAIN)
            source = data if path.endswith(".py") else None
        else:
            code = compile(payload, MAIN, "exec", dont_inherit=True)
            source = payload
    except Refused as refused:
        print(refused, file=sys.stderr)
        raise SystemExit(1) from None
    main = sys.modules["__main__"]
    main.__dict__.update(__file__=MAIN, __loader__=MainLoader(source), __spec__=None, __package__=None)
    exec(code, main.__dict__)


# ---- check mode -------------------------------------------------------------------------------

EXEC_REASON = "runs code that is not in the approved script"
LOADER_REASONS = {
    "spec_from_file_location": "loads a module from a file outside the approved script",
    "run_path": "runs a file outside the approved script",
    "enable_load_extension": "lets SQLite load native extensions from files outside the approved script",
}
NATIVE_MODULES = {"ctypes", "_ctypes", "cffi", "_cffi_backend"}
NATIVE_REASON = "loads native code; it is not available in this runner"
CATCHES_IMPORT_ERROR = {"ImportError", "ModuleNotFoundError", "Exception", "BaseException"}


def guarded_imports(tree):
    """Imports inside a try whose handler catches their ImportError: they may fail on purpose."""
    import ast
    guarded = set()
    for node in ast.walk(tree):
        if not isinstance(node, (ast.Try, getattr(ast, "TryStar", ast.Try))):
            continue
        catches = False
        for handler in node.handlers:
            kinds = handler.type.elts if isinstance(handler.type, ast.Tuple) else [handler.type]
            for kind in kinds:
                name = kind.id if isinstance(kind, ast.Name) else getattr(kind, "attr", None)
                catches |= kind is None or name in CATCHES_IMPORT_ERROR
        if catches:
            for statement in node.body:
                for inner in ast.walk(statement):
                    if isinstance(inner, (ast.Import, ast.ImportFrom)):
                        guarded.add(inner)
    return guarded


def analyze(tree, where, available):
    """Returns (line, warning) pairs and refused module names for one parsed source."""
    import ast
    warnings, refused = [], []
    guarded = guarded_imports(tree)

    def at(node):
        return f"line {node.lineno}" if where is None else f"{where} line {node.lineno}"

    def imported(node, module):
        top = module.split(".")[0]
        if top in NATIVE_MODULES:
            warnings.append((node.lineno, f"script_load_warning: {top} at {at(node)}: {NATIVE_REASON}"))
        if available(top):
            return
        if node in guarded:
            warnings.append((node.lineno, f"script_import_warning: {module} at {at(node)}: "
                             "not in the approved runner; this import fails at run time"))
        else:
            refused.append(module)

    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                imported(node, alias.name)
        elif isinstance(node, ast.ImportFrom):
            if node.level == 0 and node.module:
                imported(node, node.module)
            for alias in node.names:
                if alias.name in LOADER_REASONS:
                    warnings.append((node.lineno, f"script_load_warning: {alias.name} at {at(node)}: {LOADER_REASONS[alias.name]}"))
        elif isinstance(node, ast.Attribute) and node.attr in LOADER_REASONS:
            warnings.append((node.lineno, f"script_load_warning: {node.attr} at {at(node)}: {LOADER_REASONS[node.attr]}"))
        elif isinstance(node, ast.Call):
            func = node.func
            name = func.id if isinstance(func, ast.Name) else None
            if isinstance(func, ast.Attribute) and isinstance(func.value, ast.Name) and func.value.id == "builtins":
                name = func.attr
            if name in ("exec", "eval"):
                literal = node.args and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, (str, bytes))
                if not literal:
                    warnings.append((node.lineno, f"script_load_warning: {name} at {at(node)}: {EXEC_REASON}"))
    return warnings, refused


def check():
    """Prints warnings and refusals for the payload without running any of it; returns the exit code."""
    lockdown()
    import ast
    payload = _ck_runner.payload()
    sources, local, warnings, refused = [], set(), [], []
    try:
        if is_bundle(payload):
            bundle = load_bundle(payload)
            local = {name.split(".")[0] for name in bundle.modules}
            for _, path, data in [bundle.main, *bundle.modules.values()]:
                if path.endswith(".py"):
                    sources.append((path, data))
                else:
                    warnings.append(f"script_check_warning: {path} is bytecode and was not checked")
        else:
            sources.append((None, payload))
    except Refused as error:
        print(error, file=sys.stderr)
        return 1

    def available(top):
        return (top in local or top in sys.builtin_module_names or _imp.is_frozen(top)
                or _ck_runner.resource(top) is not None)

    syntax_errors = []
    for where, source in sources:
        try:
            tree = ast.parse(source, MAIN if where is None else f"ck-script:{where}")
        except (SyntaxError, ValueError) as error:
            place = f"line {getattr(error, 'lineno', None)}" if where is None else f"{where} line {getattr(error, 'lineno', None)}"
            syntax_errors.append(f"script_syntax_error: {place}: {getattr(error, 'msg', error)}")
            continue
        found, names = analyze(tree, where, available)
        warnings.extend(message for _, message in sorted(found, key=lambda item: item[0]))
        refused.extend(name for name in names if name not in refused)
    for message in warnings + syntax_errors:
        print(message, file=sys.stderr)
    for name in refused:
        print(f"script_import_refused: {name}", file=sys.stderr)
    return 1 if refused or syntax_errors else 0
