"""Builds CPython 3.13 for Windows as static libraries and lays it out the way build.rs reads an
extracted python-build-standalone archive: build/ objects and libraries, install/include,
install/Lib and a PYTHON.json naming every input the runner links.

    build_windows_cpython.py <rust-target> <sources dir> <work dir> <dist dir> [--openssl-only]

python-build-standalone ships libpython, OpenSSL and SQLite for Windows only as DLLs, so this
builds them. It follows the approach python-build-standalone used for its former static Windows
builds: CPython's own PCbuild projects, with the core and each linked extension changed from a
DLL to a static library, the static C runtime (/MT), and OpenSSL built from source as static
libraries. The sources dir holds the files windows-sources.lock pins, already checked.

Runs with any Python 3.10 or newer on the build machine; PCbuild uses the same one for its
build-time scripts. Every edit to CPython's build files is checked, so an upstream change that
moves an edited line stops the build by name instead of silently building something else.
"""
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

TARGETS = {
    "x86_64-pc-windows-msvc": {"platform": "x64", "arch": "amd64", "vcvars": "x64", "openssl": "VC-WIN64A"},
    "aarch64-pc-windows-msvc": {"platform": "ARM64", "arch": "arm64", "vcvars": "arm64", "openssl": "VC-WIN64-ARM"},
}

# CPython's own external sources, unpacked under externals/ with the names PCbuild expects.
EXTERNALS = {
    "bzip2": "bzip2-1.0.8",
    "mpdecimal": "mpdecimal-4.0.0",
    "sqlite": "sqlite-3.50.4.0",
    "xz": "xz-5.2.5",
    "zlib": "zlib-1.3.1",
}

# Extension modules that PCbuild builds as their own projects and the runner links. The others
# on the runner's list are already compiled into the core on Windows (PC/config.c).
EXTENSIONS = [
    "_asyncio", "_bz2", "_decimal", "_hashlib", "_lzma", "_overlapped", "_queue", "_socket",
    "_sqlite3", "_ssl", "_uuid", "_zoneinfo", "pyexpat", "select", "unicodedata",
]

# Static libraries that PCbuild builds for an extension, by the project that builds them.
EXTENSION_LIBRARIES = {"_lzma": ["liblzma"], "_sqlite3": ["sqlite3"]}

# What a static OpenSSL needs from Windows (its makefile's EX_LIBS for VC targets).
OPENSSL_SYSTEM_LIBRARIES = ["ws2_32", "gdi32", "advapi32", "crypt32", "user32"]

# OpenSSL never loads code or settings at run time: no loadable providers or engines, no DSO
# loader at all, and no automatic openssl.cnf (which can name modules to load).
OPENSSL_OPTIONS = [
    "no-shared", "no-module", "no-dso", "no-engine", "no-autoload-config",
    "no-tests", "no-docs", "no-idea", "no-mdc2",
]


def fail(message):
    sys.exit(f"build_windows_cpython.py: {message}")


def run(command, **kwargs):
    command = [str(part) for part in command]
    # Windows looks a bare program name up on this process's PATH, not the child's.
    if "env" in kwargs and not os.path.dirname(command[0]):
        found = shutil.which(command[0], path=kwargs["env"].get("PATH"))
        if not found:
            fail(f"{command[0]} is not on the build PATH")
        command[0] = found
    print("+", " ".join(command), flush=True)
    subprocess.run(command, check=True, **kwargs)


def unpack(archive, destination):
    """Unpacks an archive whose entries share one top folder, as destination."""
    if destination.exists():
        return destination
    staging = destination.with_name(destination.name + ".unpacking")
    shutil.rmtree(staging, ignore_errors=True)
    staging.mkdir(parents=True)
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as stream:
            stream.extractall(staging)
    else:
        with tarfile.open(archive) as stream:
            stream.extractall(staging, filter="data")
    entries = list(staging.iterdir())
    top = entries[0] if len(entries) == 1 and entries[0].is_dir() else staging
    top.rename(destination)
    shutil.rmtree(staging, ignore_errors=True)
    return destination


def source(sources, name):
    found = [path for path in sources.iterdir() if path.name.startswith(name + ".")]
    if len(found) != 1:
        fail(f"expected one {name} archive in {sources}, found {[path.name for path in found]}")
    return found[0]


def patch(path, old, new, count=1):
    text = path.read_text(encoding="utf-8")
    if text.count(old) < count:
        fail(f"{path}: expected {old!r} {count} time(s); CPython's build files changed")
    path.write_text(text.replace(old, new), encoding="utf-8")


def add_definitions(path, xml):
    """Adds item definitions after the project's own, just before the C++ targets import."""
    patch(path, '<Import Project="$(VCTargetsPath)\\Microsoft.Cpp.targets" />',
          f'<ItemDefinitionGroup>{xml}</ItemDefinitionGroup>\n'
          '  <Import Project="$(VCTargetsPath)\\Microsoft.Cpp.targets" />')


def make_static(pcbuild):
    """Edits PCbuild so the core and the linked extensions build as static libraries."""
    props = pcbuild / "pyproject.props"
    patch(props, "<RuntimeLibrary>MultiThreadedDLL</RuntimeLibrary>", "<RuntimeLibrary>MultiThreaded</RuntimeLibrary>")
    patch(props, "<RuntimeLibrary>MultiThreadedDebugDLL</RuntimeLibrary>", "<RuntimeLibrary>MultiThreadedDebug</RuntimeLibrary>")
    # Every project compiles against a static core: no dllimport/dllexport of the C API.
    # Plain objects instead of whole-program (/GL) ones: the runner's final link runs under
    # rustc, and link-time code generation there failed (C1081, LNK1257). It would also tie the
    # objects to the exact MSVC version that compiled them.
    patch(props, "<WholeProgramOptimization>true</WholeProgramOptimization>",
          "<WholeProgramOptimization>false</WholeProgramOptimization>")
    patch(props, "<LinkTimeCodeGeneration Condition=\"$(Configuration) != 'Debug'\">UseLinkTimeCodeGeneration</LinkTimeCodeGeneration>",
          "")
    patch(props, "<LinkTimeCodeGeneration Condition=\"$(Configuration) != 'Debug'\">true</LinkTimeCodeGeneration>",
          "")
    patch(props, "<PreprocessorDefinitions>WIN32;$(_Py3NamePreprocessorDefinition)",
          "<PreprocessorDefinitions>WIN32;Py_NO_ENABLE_SHARED;$(_Py3NamePreprocessorDefinition)")
    core = pcbuild / "pythoncore.vcxproj"
    patch(core, "<ConfigurationType>DynamicLibrary</ConfigurationType>", "<ConfigurationType>StaticLibrary</ConfigurationType>")
    patch(core, "Py_ENABLE_SHARED;", "")
    for project in EXTENSIONS + ["sqlite3"]:
        path = pcbuild / f"{project}.vcxproj"
        patch(path, "<ConfigurationType>DynamicLibrary</ConfigurationType>", "<ConfigurationType>StaticLibrary</ConfigurationType>")
        if project != "sqlite3":
            # A built-in module, not a .pyd: PyInit_* is neither exported nor imported.
            add_definitions(path, "<ClCompile><PreprocessorDefinitions>Py_BUILD_CORE_BUILTIN;"
                                  "%(PreprocessorDefinitions)</PreprocessorDefinitions></ClCompile>")
    patch(pcbuild / "sqlite3.vcxproj", "SQLITE_API=__declspec(dllexport);", "")
    # sys.dllhandle and sys.winver are only set when the core is a DLL, but site and sysconfig
    # read sys.winver on every Windows build. A static core has no DLL handle, and its winver
    # is the X.Y version a 64-bit DLL build reports.
    source = pcbuild.parent
    patch(source / "Python/sysmodule.c",
          "#ifdef MS_COREDLL\nextern void *PyWin_DLLhModule;\n"
          "/* A string loaded from the DLL at startup: */\nextern const char *PyWin_DLLVersionString;\n#endif",
          "#ifdef MS_COREDLL\nextern void *PyWin_DLLhModule;\n"
          "/* A string loaded from the DLL at startup: */\nextern const char *PyWin_DLLVersionString;\n"
          "#elif defined(MS_WINDOWS)\nstatic void *PyWin_DLLhModule = NULL;\n"
          "static const char *PyWin_DLLVersionString = Py_STRINGIFY(PY_MAJOR_VERSION) \".\" "
          "Py_STRINGIFY(PY_MINOR_VERSION);\n#endif")
    patch(source / "Python/sysmodule.c", '#ifdef MS_COREDLL\n    SET_SYS("dllhandle"',
          '#ifdef MS_WINDOWS\n    SET_SYS("dllhandle"')
    # python.exe (used only to compile the standard library at build time) links the static
    # core, so it needs the core's own Windows libraries too.
    add_definitions(pcbuild / "python.vcxproj", "<Link><AdditionalDependencies>"
                    "version.lib;ws2_32.lib;pathcch.lib;bcrypt.lib;%(AdditionalDependencies)"
                    "</AdditionalDependencies></Link>")


def msvc_environment(vcvars):
    vswhere = Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Microsoft Visual Studio/Installer/vswhere.exe"
    install = subprocess.run([str(vswhere), "-latest", "-products", "*", "-requires",
                              "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"],
                             check=True, capture_output=True, text=True).stdout.strip()
    if not install:
        fail("no Visual Studio installation with the MSVC tools was found")
    vcvarsall = Path(install) / "VC/Auxiliary/Build/vcvarsall.bat"
    output = subprocess.run(f'cmd /d /s /c ""{vcvarsall}" {vcvars} >nul && set"',
                            check=True, capture_output=True, text=True, shell=True).stdout
    environment = dict(line.split("=", 1) for line in output.splitlines() if "=" in line)
    return Path(install), environment


def host_vcvars(target):
    host = "arm64" if platform.machine().upper() == "ARM64" else "x64"
    wanted = TARGETS[target]["vcvars"]
    return host, wanted if host == wanted else f"{host}_{wanted}"


def build_openssl(target, sources, work, tools, environment):
    """Builds OpenSSL's static libraries once per source and options. They live beside the
    CPython work folder, so a CPython rebuild doesn't repeat this, the slowest step."""
    info = TARGETS[target]
    archive = source(sources, "openssl")
    prefix = work.parent / f"{target}-openssl"
    stamp = hashlib.sha256(archive.read_bytes() + " ".join(OPENSSL_OPTIONS).encode()).hexdigest()
    stamp_file = prefix / ".build-stamp"
    if stamp_file.exists() and stamp_file.read_text() == stamp:
        return prefix
    shutil.rmtree(prefix, ignore_errors=True)
    tree = work.parent / f"{target}-openssl-src"
    shutil.rmtree(tree, ignore_errors=True)
    unpack(archive, tree)
    perl = tools / "strawberry-perl"
    path = [str(perl / "perl/bin")]
    if info["arch"] == "amd64":
        path.append(str(tools / "nasm"))
    env = dict(environment, PATH=os.pathsep.join(path + [environment["PATH"]]))
    run([perl / "perl/bin/perl.exe", "Configure", info["openssl"], *OPENSSL_OPTIONS,
         f"--prefix={prefix}", f"--openssldir={prefix / 'ssl'}"], cwd=tree, env=env)
    makefile = tree / "makefile"
    text = makefile.read_text(encoding="utf-8")
    # The static C runtime, matching CPython and the Rust code it is linked with. Without any
    # /M option cl.exe already uses the static runtime, which is what a no-shared OpenSSL
    # build asks for; any /MD left in its makefile is switched.
    text = re.sub(r"(?<![\w-])/MD(?![\w])", "/MT", text)
    makefile.write_text(text, encoding="utf-8")
    run(["nmake", "/nologo", "build_libs"], cwd=tree, env=env)
    run(["nmake", "/nologo", "install_dev"], cwd=tree, env=env)
    stamp_file.write_text(stamp)
    return prefix


def msbuild(install, project, platform_name, properties, environment):
    command = [Path(install) / "MSBuild/Current/Bin/MSBuild.exe", project, "/nologo", "/m", "/v:minimal",
               "/p:Configuration=Release", f"/p:Platform={platform_name}"]
    command += [f"/p:{key}={value}" for key, value in properties.items()]
    run(command, env=environment)


def system_links(project_file):
    text = project_file.read_text(encoding="utf-8")
    names = []
    for group in re.findall(r"<Link>.*?<AdditionalDependencies>(.*?)</AdditionalDependencies>", text, re.S):
        for entry in group.split(";"):
            entry = entry.strip()
            if entry.lower().endswith(".lib") and "$(" not in entry and "\\" not in entry:
                names.append(entry[:-4])
    return names


def copy_objects(source_dir, destination, dist):
    objects = []
    for path in sorted(source_dir.rglob("*.obj")):
        target = destination / path.relative_to(source_dir)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, target)
        objects.append(target.relative_to(dist).as_posix())
    if not objects:
        fail(f"no object files in {source_dir}")
    return objects


def main():
    target = sys.argv[1]
    sources, work, dist = (Path(argument).resolve() for argument in sys.argv[2:5])
    if target not in TARGETS:
        fail(f"unsupported target {target}")
    info = TARGETS[target]
    work.mkdir(parents=True, exist_ok=True)
    tools = work.parent / "tools"
    tools.mkdir(parents=True, exist_ok=True)
    unpack(source(sources, "strawberry-perl"), tools / "strawberry-perl")
    unpack(source(sources, "nasm"), tools / "nasm")

    cpython = work / "cpython"
    # The marker names this script's own hash, so changed edits re-unpack the sources.
    recipe = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    marker = cpython / ".static-edits-done"
    if not marker.exists() or marker.read_text() != recipe:
        shutil.rmtree(cpython, ignore_errors=True)
        unpack(source(sources, "cpython"), cpython)
        for name, folder in EXTERNALS.items():
            unpack(source(sources, name), cpython / "externals" / folder)
        make_static(cpython / "PCbuild")
        marker.write_text(recipe)

    host, vcvars = host_vcvars(target)
    install, environment = msvc_environment(vcvars)
    openssl = build_openssl(target, sources, work, tools, environment)
    if "--openssl-only" in sys.argv[5:]:
        print(f"OpenSSL for {target} is in {openssl}")
        return

    pcbuild = cpython / "PCbuild"
    build_env = dict(os.environ, PYTHON=sys.executable, PYTHON_FOR_BUILD=sys.executable)
    properties = {
        "IncludeExternals": "true", "IncludeSSL": "true", "IncludeTkinter": "false", "IncludeCTypes": "false",
        "ExternalsDir": f"{cpython / 'externals'}\\", "opensslIncludeDir": openssl / "include",
        "opensslOutDir": f"{openssl / 'lib'}\\", "SkipCopySSLDLL": "true",
        # PCbuild's build-time scripts use this Python instead of looking for one online.
        "PythonForBuild": sys.executable,
    }
    # The freezer runs during the build, so it is always built for the build machine.
    msbuild(install, pcbuild / "_freeze_module.vcxproj", "ARM64" if host == "arm64" else "x64", properties, build_env)
    msbuild(install, pcbuild / "pythoncore.vcxproj", info["platform"], properties, build_env)
    for project in EXTENSIONS:
        msbuild(install, pcbuild / f"{project}.vcxproj", info["platform"], properties, build_env)
    native = (host == "arm64") == (info["arch"] == "arm64")
    if native:
        msbuild(install, pcbuild / "python.vcxproj", info["platform"], properties, build_env)

    obj = pcbuild / "obj" / f"313{info['arch']}_Release"
    out = pcbuild / info["arch"]
    shutil.rmtree(dist, ignore_errors=True)
    (dist / "build").mkdir(parents=True)

    core_objects = copy_objects(obj / "pythoncore", dist / "build/core", dist)
    core_links = [{"name": name, "system": True} for name in
                  system_links(pcbuild / "pythoncore.vcxproj") + ["advapi32", "shell32", "ole32", "oleaut32"]]
    extensions = {}
    config = (cpython / "PC/config.c").read_text(encoding="utf-8")
    for name, init in re.findall(r'\{"(\w+)", (\w+)\}', config):
        extensions[name] = [{"in_core": True, "init_fn": init, "objs": [], "links": [], "variant": "default"}]
    (dist / "build/lib").mkdir()
    for library in ("libcrypto", "libssl"):
        shutil.copy2(openssl / "lib" / f"{library}.lib", dist / "build/lib")
    for project in EXTENSIONS:
        objects = copy_objects(obj / project, dist / "build/extensions" / project, dist)
        links = [{"name": name, "system": True} for name in system_links(pcbuild / f"{project}.vcxproj")]
        for library in EXTENSION_LIBRARIES.get(project, []):
            shutil.copy2(out / f"{library}.lib", dist / "build/lib")
            links.append({"name": library, "path_static": f"build/lib/{library}.lib"})
        if project in ("_ssl", "_hashlib"):
            links += [{"name": "libssl", "path_static": "build/lib/libssl.lib"},
                      {"name": "libcrypto", "path_static": "build/lib/libcrypto.lib"}]
            links += [{"name": name, "system": True} for name in OPENSSL_SYSTEM_LIBRARIES]
        extensions[project] = [{"in_core": False, "init_fn": f"PyInit_{project}", "objs": objects,
                                "links": links, "variant": "default"}]

    include = dist / "install/include"
    shutil.copytree(cpython / "Include", include)
    shutil.copy2(obj / "pythoncore/pyconfig.h", include / "pyconfig.h")
    shutil.copytree(cpython / "Lib", dist / "install/Lib",
                    ignore=shutil.ignore_patterns("__pycache__", "test", "idlelib", "tkinter", "turtledemo"))

    patchlevel = (cpython / "Include/patchlevel.h").read_text(encoding="utf-8")
    version = re.search(r'#define PY_VERSION\s+"([^"]+)"', patchlevel)[1]
    bootstrap = (cpython / "Lib/importlib/_bootstrap_external.py").read_text(encoding="utf-8")
    magic = int(re.search(r"MAGIC_NUMBER = \((\d+)\)\.to_bytes\(2, 'little'\) \+ b'\\r\\n'", bootstrap)[1])
    manifest = {
        "version": "8",
        "target_triple": target,
        "python_version": version,
        "python_major_minor_version": ".".join(version.split(".")[:2]),
        "python_bytecode_magic_number": (magic.to_bytes(2, "little") + b"\r\n").hex(),
        "libpython_link_mode": "static",
        "python_paths": {"include": "install/include", "stdlib": "install/Lib"},
        "build_info": {
            "object_file_format": "coff",
            "inittab_object": "build/core/config.obj",
            "core": {"objs": core_objects, "links": core_links},
            "extensions": extensions,
        },
    }
    if "build/core/config.obj" not in core_objects:
        fail("the core has no config.obj; the init table moved")
    (dist / "PYTHON.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"CPython {version} for {target} is in {dist}")


if __name__ == "__main__":
    main()
