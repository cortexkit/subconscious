/*
 * Starts the embedded CPython for ck-script-runner-python.
 *
 * Startup is isolated: no environment variable, settings file or path search changes what runs.
 * Every path input and output is set explicitly (see set_paths), so CPython's path step keeps
 * them and never reads pyvenv.cfg, a ._pth file or pybuilddir.txt. Initialisation is split in two phases: after
 * the core phase only built-in and frozen modules exist, which is when the bootstrap code puts
 * the in-memory standard-library finder first on sys.meta_path. The main phase then imports
 * `encodings` and the rest of startup through that finder instead of the disk.
 */
#define PY_SSIZE_T_CLEAN
#include <Python.h>

/* Implemented in Rust (src/resources.rs): finds one compiled standard-library module. */
extern int ck_resource_lookup(const char *name, size_t name_len, const unsigned char **data,
                              size_t *data_len, int *is_package);

static const unsigned char *payload_bytes;
static size_t payload_length;

/* _ck_runner.resource(name) -> (is_package, code bytes as a read-only memoryview) or None */
static PyObject *runner_resource(PyObject *self, PyObject *name) {
    (void)self;
    Py_ssize_t size = 0;
    const char *text = PyUnicode_AsUTF8AndSize(name, &size);
    if (text == NULL) {
        return NULL;
    }
    const unsigned char *data = NULL;
    size_t length = 0;
    int is_package = 0;
    if (!ck_resource_lookup(text, (size_t)size, &data, &length, &is_package)) {
        Py_RETURN_NONE;
    }
    PyObject *view = PyMemoryView_FromMemory((char *)data, (Py_ssize_t)length, PyBUF_READ);
    if (view == NULL) {
        return NULL;
    }
    return Py_BuildValue("(NN)", PyBool_FromLong(is_package), view);
}

/* _ck_runner.payload() -> the bytes received on the daemon's channel (or the checked file) */
static PyObject *runner_payload(PyObject *self, PyObject *unused) {
    (void)self;
    (void)unused;
    return PyBytes_FromStringAndSize((const char *)payload_bytes, (Py_ssize_t)payload_length);
}

static PyMethodDef runner_methods[] = {
    {"resource", runner_resource, METH_O, "Find a compiled standard-library module."},
    {"payload", runner_payload, METH_NOARGS, "Return the script bytes."},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef runner_module = {
    PyModuleDef_HEAD_INIT, "_ck_runner", NULL, -1, runner_methods, NULL, NULL, NULL, NULL,
};

static PyObject *init_runner(void) { return PyModule_Create(&runner_module); }

static void exit_on_error(PyStatus status) {
    if (PyStatus_Exception(status)) {
        Py_ExitStatusException(status);
    }
}

/*
 * Sets every path input and output to the runner itself. CPython 3.13 still runs its frozen
 * getpath step, but it keeps values that are already set, and a set `home` makes it skip the
 * pyvenv.cfg, ._pth and pybuilddir.txt lookups beside the executable.
 */
static void set_paths(PyConfig *config, const char *executable) {
    wchar_t **fields[] = {
        &config->program_name, &config->home,        &config->executable,
        &config->base_executable, &config->prefix,   &config->exec_prefix,
        &config->base_prefix,  &config->base_exec_prefix,
    };
    for (size_t i = 0; i < sizeof(fields) / sizeof(fields[0]); i++) {
        exit_on_error(PyConfig_SetBytesString(config, fields[i], executable));
    }
    config->module_search_paths_set = 1;
}

static PyObject *call(PyObject *module, const char *name) {
    return PyObject_CallMethod(module, name, NULL);
}

/*
 * mode 0 runs the payload as the script; mode 1 checks it and prints warnings.
 * Returns the process exit code. A SystemExit raised by the script exits from PyErr_Print.
 */
int ck_python_main(const char *executable, const char *bootstrap, int mode,
                   const unsigned char *payload, size_t payload_len) {
    payload_bytes = payload;
    payload_length = payload_len;
    if (PyImport_AppendInittab("_ck_runner", init_runner) < 0) {
        fprintf(stderr, "ck-script-runner-python: cannot register the runner module\n");
        return 1;
    }

    PyPreConfig preconfig;
    PyPreConfig_InitIsolatedConfig(&preconfig);
    /* Fixed UTF-8 text handling: the locale must not change how the script's output is encoded. */
    preconfig.utf8_mode = 1;
    exit_on_error(Py_PreInitialize(&preconfig));

    PyConfig config;
    PyConfig_InitIsolatedConfig(&config);
    config.isolated = 1;
    config.use_environment = 0;
    config.safe_path = 1;
    config.site_import = 0;
    config.user_site_directory = 0;
    config.write_bytecode = 0;
    config.parse_argv = 0;
    /* Like the python command: SIGINT raises KeyboardInterrupt and SIGPIPE is ignored. */
    config.install_signal_handlers = 1;
    config.use_frozen_modules = 1;
    config._init_main = 0;
    char *argv[] = {"ck-script:main"};
    exit_on_error(PyConfig_SetBytesArgv(&config, 1, argv));
    set_paths(&config, executable);
    PyStatus status = Py_InitializeFromConfig(&config);
    PyConfig_Clear(&config);
    exit_on_error(status);

    PyObject *code = Py_CompileString(bootstrap, "<ck-runner bootstrap>", Py_file_input);
    PyObject *module = code ? PyModule_New("_ck_bootstrap") : NULL;
    PyObject *ran = NULL;
    if (module != NULL) {
        PyObject *globals = PyModule_GetDict(module);
        if (PyDict_SetItemString(globals, "__builtins__", PyEval_GetBuiltins()) == 0) {
            ran = PyEval_EvalCode(code, globals, globals);
        }
    }
    Py_XDECREF(code);
    PyObject *installed = ran ? call(module, "install") : NULL;
    Py_XDECREF(ran);
    if (installed == NULL) {
        PyErr_Print();
        fprintf(stderr, "ck-script-runner-python: the runner bootstrap failed\n");
        return 1;
    }
    Py_DECREF(installed);
    exit_on_error(_Py_InitializeMain());

    PyObject *result = call(module, mode == 1 ? "check" : "run");
    int exit_code = 0;
    if (result == NULL) {
        PyErr_Print();
        exit_code = 1;
    } else if (mode == 1) {
        exit_code = (int)PyLong_AsLong(result);
        if (PyErr_Occurred()) {
            PyErr_Print();
            exit_code = 1;
        }
    }
    Py_XDECREF(result);
    Py_DECREF(module);
    if (Py_FinalizeEx() < 0) {
        exit_code = 120;
    }
    return exit_code;
}
