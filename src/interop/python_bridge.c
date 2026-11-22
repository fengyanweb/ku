/* The generated configuration above this file pins the CPython embedding ABI. */
#if !defined(_WIN32) && !defined(_GNU_SOURCE)
#define _GNU_SOURCE 1
#endif
#if defined(__APPLE__) && !defined(_DARWIN_C_SOURCE)
#define _DARWIN_C_SOURCE 1
#endif

#include <Python.h>

#include "abi.h"
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
static SRWLOCK ku_py_lock = SRWLOCK_INIT;
#define KU_PY_LOCK() AcquireSRWLockExclusive(&ku_py_lock)
#define KU_PY_UNLOCK() ReleaseSRWLockExclusive(&ku_py_lock)
#else
#include <dlfcn.h>
#include <pthread.h>
static pthread_mutex_t ku_py_lock = PTHREAD_MUTEX_INITIALIZER;
#define KU_PY_LOCK() pthread_mutex_lock(&ku_py_lock)
#define KU_PY_UNLOCK() pthread_mutex_unlock(&ku_py_lock)
#endif

#define KU_PY_MAX_PAYLOAD (16u * 1024u * 1024u)
#define KU_PY_ENV_KEY "_ku_python_environment_v1"
#define KU_PY_DISTS_KEY "_ku_python_distributions_v1"

static int ku_py_bytes(KuInteropValue *result, uint32_t tag, const char *data, size_t length) {
    if (length > KU_PY_MAX_PAYLOAD) return 0;
    uint8_t *owned = length ? malloc(length) : NULL;
    if (length && !owned) return 0;
    if (length) memcpy(owned, data, length);
    result->tag = tag;
    result->data = owned;
    result->length = length;
    return 1;
}

static int ku_py_error(KuInteropValue *result, const char *message) {
    result->tag = KU_INTEROP_STR;
    (void)ku_py_bytes(result, KU_INTEROP_STR, message, strlen(message));
    return 1;
}

static int ku_py_exception(KuInteropValue *result, const char *operation) {
    PyObject *type = NULL, *value = NULL, *trace = NULL;
    PyObject *name = NULL, *details = NULL, *message = NULL;
    PyErr_Fetch(&type, &value, &trace);
    PyErr_NormalizeException(&type, &value, &trace);
    if (type) name = PyObject_GetAttrString(type, "__name__");
    if (value) details = PyObject_Str(value);
    if (name && details) {
        message = PyUnicode_FromFormat("%s.%s: %U: %U", KU_PY_MODULE, operation, name, details);
    }
    int copied = 0;
    if (message) {
        Py_ssize_t length;
        const char *bytes = PyUnicode_AsUTF8AndSize(message, &length);
        if (bytes) copied = ku_py_bytes(result, KU_INTEROP_STR, bytes, (size_t)length);
    }
    Py_XDECREF(message);
    Py_XDECREF(details);
    Py_XDECREF(name);
    Py_XDECREF(trace);
    Py_XDECREF(value);
    Py_XDECREF(type);
    PyErr_Clear();
    return copied ? 1 : ku_py_error(result, "CPython call failed");
}

#ifdef _WIN32
static int ku_py_sidecar_wide(const char *file, wchar_t *path, DWORD capacity) {
    HMODULE owner;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                              GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            (LPCWSTR)&ku_interop_call_v1, &owner)) return 0;
    DWORD size = GetModuleFileNameW(owner, path, capacity);
    if (!size || size >= capacity) return 0;
    wchar_t *slash = wcsrchr(path, L'\\');
    if (!slash) return 0;
    int remaining = (int)(capacity - (size_t)(slash + 1 - path));
    return MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, file, -1, slash + 1, remaining) != 0;
}
#endif

#if !defined(_WIN32) && (KU_PY_LOCAL || KU_PY_WHEELS || KU_PY_PORTABLE)
static char *ku_py_sidecar_narrow(const char *file) {
    Dl_info info;
    if (!dladdr((const void *)&ku_interop_call_v1, &info) || !info.dli_fname) return NULL;
    const char *slash = strrchr(info.dli_fname, '/');
    if (!slash) return NULL;
    size_t prefix = (size_t)(slash + 1 - info.dli_fname);
    size_t suffix = strlen(file);
    if (prefix > 65536 || suffix > 1024) return NULL;
    char *path = malloc(prefix + suffix + 1);
    if (!path) return NULL;
    memcpy(path, info.dli_fname, prefix);
    memcpy(path + prefix, file, suffix + 1);
    return path;
}
#endif

#if KU_PY_LOCAL || KU_PY_WHEELS
static PyObject *ku_py_sidecar_path(const char *file) {
#ifdef _WIN32
    wchar_t path[32768];
    if (!ku_py_sidecar_wide(file, path, (DWORD)(sizeof(path) / sizeof(path[0])))) return NULL;
    return PyUnicode_FromWideChar(path, -1);
#else
    char *path = ku_py_sidecar_narrow(file);
    if (!path) return NULL;
    PyObject *value = PyUnicode_DecodeFSDefault(path);
    free(path);
    return value;
#endif
}
#endif

static int ku_py_pin_library(void) {
    const size_t version_size = sizeof(KU_PY_RELEASE) - 1;
    const char *version = Py_GetVersion();
    if (strncmp(version, KU_PY_RELEASE, version_size) || version[version_size] != ' ') return 0;
#ifdef _WIN32
    HMODULE python;
    wchar_t actual[32768], expected[32768];
    DWORD length;
    return GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                 GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                              (LPCWSTR)&Py_GetVersion, &python)
        && (length = GetModuleFileNameW(python, actual, (DWORD)(sizeof(actual) / sizeof(actual[0]))))
        && length < sizeof(actual) / sizeof(actual[0])
        && ku_py_sidecar_wide(KU_PY_LIBRARY, expected, (DWORD)(sizeof(expected) / sizeof(expected[0])))
        && _wcsicmp(actual, expected) == 0;
#else
    /* CPython's native extension modules must resolve symbols from the selected
       runtime even when Ku loads this provider with RTLD_LOCAL. */
    void *visible = dlsym(RTLD_DEFAULT, "Py_GetVersion");
    if (visible && visible != (void *)&Py_GetVersion) return 0;
#if KU_PY_PORTABLE
    char *library = ku_py_sidecar_narrow(KU_PY_LIBRARY);
    if (!library) return 0;
    void *python = dlopen(library, RTLD_NOW | RTLD_GLOBAL | RTLD_NOLOAD);
    free(library);
#else
    void *python = dlopen(KU_PY_LIBRARY, RTLD_NOW | RTLD_GLOBAL | RTLD_NOLOAD);
#endif
    if (!python) return 0;
    int matches = dlsym(python, "Py_GetVersion") == (void *)&Py_GetVersion;
    dlclose(python);
    return matches;
#endif
}

static PyStatus ku_py_config_string(PyConfig *config, wchar_t **slot, const char *value) {
#ifdef _WIN32
    int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, value, -1, NULL, 0);
    if (length <= 0) return PyStatus_Error("Invalid UTF-8 in CPython path");
    wchar_t *wide = malloc((size_t)length * sizeof(*wide));
    if (!wide) return PyStatus_NoMemory();
    int result = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, value, -1, wide, length);
    PyStatus status = result ? PyConfig_SetString(config, slot, wide) : PyStatus_Error("CPython path conversion failed");
    free(wide);
    return status;
#else
    return PyConfig_SetBytesString(config, slot, value);
#endif
}

static int ku_py_initialize(KuInteropValue *result) {
    KU_PY_LOCK();
    if (Py_IsInitialized()) {
        KU_PY_UNLOCK();
        return 0;
    }
    if (!ku_py_pin_library()) {
        KU_PY_UNLOCK();
        return ku_py_error(result, "Loaded CPython library does not match the prepared embedding ABI");
    }

    PyConfig config;
    PyConfig_InitIsolatedConfig(&config);
    config.install_signal_handlers = 0;
    config.parse_argv = 0;
    config.site_import = 0;
    config.user_site_directory = 0;
    config.write_bytecode = 0;
    config.buffered_stdio = 0;

#if KU_PY_PORTABLE && !defined(_WIN32)
    char *home = ku_py_sidecar_narrow(KU_PY_HOME);
    char *executable = ku_py_sidecar_narrow(KU_PY_EXECUTABLE);
    PyStatus status = home && executable
        ? ku_py_config_string(&config, &config.home, home)
        : PyStatus_Error("Cannot locate the bundled CPython runtime");
    if (!PyStatus_Exception(status)) {
        status = ku_py_config_string(&config, &config.program_name, executable);
    }
    free(executable);
    free(home);
#else
    PyStatus status = ku_py_config_string(&config, &config.home, KU_PY_HOME);
    if (!PyStatus_Exception(status)) {
        status = ku_py_config_string(&config, &config.program_name, KU_PY_EXECUTABLE);
    }
#endif
    if (!PyStatus_Exception(status)) status = Py_InitializeFromConfig(&config);
    PyConfig_Clear(&config);
    if (PyStatus_Exception(status)) {
        int error = ku_py_error(result, status.err_msg ? status.err_msg : "Cannot initialize selected CPython");
        KU_PY_UNLOCK();
        return error;
    }

    PyObject *identity = PyUnicode_FromString(KU_PY_ENV);
    if (!identity || PySys_SetObject(KU_PY_ENV_KEY, identity) < 0) {
        Py_XDECREF(identity);
        int error = ku_py_exception(result, "initialize");
        (void)PyEval_SaveThread();
        KU_PY_UNLOCK();
        return error;
    }
    Py_DECREF(identity);
    (void)PyEval_SaveThread();
    KU_PY_UNLOCK();
    return 0;
}

static int ku_py_environment(KuInteropValue *result) {
    static int registered;
    if (registered) return 0;
    PyObject *identity = PySys_GetObject(KU_PY_ENV_KEY);
    if (!identity || !PyUnicode_Check(identity)
        || PyUnicode_CompareWithASCIIString(identity, KU_PY_ENV) != 0) {
        PyErr_Clear();
        return ku_py_error(result, "Conflicting CPython libraries in one application");
    }
#if KU_PY_WHEELS
    static const char *const declarations[][2] = { KU_PY_DISTS };
    PyObject *dists = PySys_GetObject(KU_PY_DISTS_KEY);
    if (!dists) {
        PyObject *created = PyDict_New();
        if (!created) return ku_py_exception(result, "prepare");
        int status = PySys_SetObject(KU_PY_DISTS_KEY, created);
        Py_DECREF(created);
        if (status != 0) return ku_py_exception(result, "prepare");
        dists = PySys_GetObject(KU_PY_DISTS_KEY);
    }
    if (!dists || !PyDict_Check(dists)) return ku_py_error(result, "Invalid embedded CPython dependency state");
    for (size_t i = 0; i < sizeof(declarations) / sizeof(declarations[0]); ++i) {
        const char *name = declarations[i][0];
        const char *requested = declarations[i][1];
        PyObject *existing = PyDict_GetItemString(dists, name);
        if (!existing || (PyUnicode_Check(existing)
                && PyUnicode_CompareWithASCIIString(existing, requested) == 0)) continue;
        const char *current = PyUnicode_Check(existing) ? PyUnicode_AsUTF8(existing) : NULL;
        char detail[1024];
        (void)snprintf(detail, sizeof(detail), "Python package %s conflict: installed %s; %s requires %s",
                      name, current ? current : "unknown", KU_PY_MODULE, requested);
        PyErr_Clear();
        return ku_py_error(result, detail);
    }
    for (size_t i = 0; i < sizeof(declarations) / sizeof(declarations[0]); ++i) {
        PyObject *version = PyUnicode_FromString(declarations[i][1]);
        if (!version) return ku_py_exception(result, "prepare");
        int status = PyDict_SetItemString(dists, declarations[i][0], version);
        Py_DECREF(version);
        if (status != 0) return ku_py_exception(result, "prepare");
    }
#endif
    registered = 1;
    return 0;
}

#if KU_PY_LOCAL
static PyObject *ku_py_import_source(void) {
    PyObject *modules = PyImport_GetModuleDict();
    PyObject *existing = PyDict_GetItemString(modules, KU_PY_MODULE_KEY);
    if (existing) {
        Py_INCREF(existing);
        return existing;
    }

    PyObject *path = ku_py_sidecar_path("bridge.py");
    if (!path) {
        if (!PyErr_Occurred()) PyErr_SetString(PyExc_ImportError, "Cannot locate sidecar bridge.py");
        return NULL;
    }
    PyObject *util = PyImport_ImportModule("importlib.util");
    PyObject *spec = util ? PyObject_CallMethod(util, "spec_from_file_location", "sO", KU_PY_MODULE_KEY, path) : NULL;
    PyObject *module = spec ? PyObject_CallMethod(util, "module_from_spec", "O", spec) : NULL;
    PyObject *loader = spec ? PyObject_GetAttrString(spec, "loader") : NULL;
    if (module && loader && PyDict_SetItemString(modules, KU_PY_MODULE_KEY, module) == 0) {
        PyObject *executed = PyObject_CallMethod(loader, "exec_module", "O", module);
        if (executed) {
            Py_DECREF(executed);
        } else {
            PyObject *type, *value, *trace;
            PyErr_Fetch(&type, &value, &trace);
            if (PyDict_DelItemString(modules, KU_PY_MODULE_KEY) < 0) PyErr_Clear();
            PyErr_Restore(type, value, trace);
            Py_CLEAR(module);
        }
    } else {
        Py_CLEAR(module);
    }
    Py_XDECREF(loader);
    Py_XDECREF(spec);
    Py_XDECREF(util);
    Py_DECREF(path);
    return module;
}

#endif

static PyObject *ku_py_import(void) {
#if KU_PY_WHEELS
    PyObject *paths = PySys_GetObject("path");
    PyObject *site = ku_py_sidecar_path("site");
    if (!paths || !site) {
        Py_XDECREF(site);
        if (!PyErr_Occurred()) PyErr_SetString(PyExc_ImportError, "Cannot locate prepared Python wheel site");
        return NULL;
    }
    int present = PySequence_Contains(paths, site);
    if (present < 0 || (!present && PyList_Append(paths, site) < 0)) {
        Py_DECREF(site);
        return NULL;
    }
    Py_DECREF(site);
#endif
#if KU_PY_LOCAL
    return ku_py_import_source();
#else
    if (!KU_PY_MODULE[0]) {
        PyErr_SetString(PyExc_ImportError, "Python distribution needs an explicit module or author-owned adapter");
        return NULL;
    }
    return PyImport_ImportModule(KU_PY_MODULE);
#endif
}

static PyObject *ku_py_input(const KuInteropValue *value) {
    if (value->length > KU_PY_MAX_PAYLOAD || (value->length && !value->data)) {
        PyErr_SetString(PyExc_ValueError, "Ku Python argument exceeds its payload bounds");
        return NULL;
    }
    switch (value->tag) {
    case KU_INTEROP_NULL: Py_INCREF(Py_None); return Py_None;
    case KU_INTEROP_INT: return PyLong_FromLongLong(value->integer);
    case KU_INTEROP_BOOL: return PyBool_FromLong(value->integer != 0);
    case KU_INTEROP_STR:
        return PyUnicode_DecodeUTF8(value->length ? (const char *)value->data : "",
                                    (Py_ssize_t)value->length, "strict");
    default:
        PyErr_SetString(PyExc_TypeError, "Python bridge accepts only str, int, bool or null");
        return NULL;
    }
}

static int ku_py_output(PyObject *value, KuInteropValue *result) {
    if (value == Py_None) {
        result->tag = KU_INTEROP_NULL;
    } else if (PyBool_Check(value)) {
        result->tag = KU_INTEROP_BOOL;
        result->integer = value == Py_True;
    } else if (PyLong_Check(value)) {
        long long number = PyLong_AsLongLong(value);
        if (number == -1 && PyErr_Occurred()) return 0;
        result->tag = KU_INTEROP_INT;
        result->integer = number;
    } else if (PyUnicode_Check(value)) {
        Py_ssize_t length;
        const char *text = PyUnicode_AsUTF8AndSize(value, &length);
        if (!text) return 0;
        if (!ku_py_bytes(result, KU_INTEROP_STR, text, (size_t)length)) {
            PyErr_SetString(PyExc_ValueError, "Python str result exceeds Ku's allocation budget");
            return 0;
        }
    } else {
        PyErr_SetString(PyExc_TypeError, "Cannot return a Python object across Ku's primitive bridge; convert to str, int, bool or None in an adapter");
        return 0;
    }
    return 1;
}

static PyObject *ku_py_arguments(const KuInteropValue *args, size_t nargs) {
    PyObject *tuple = PyTuple_New((Py_ssize_t)nargs);
    if (!tuple) return NULL;
    for (size_t i = 0; i < nargs; ++i) {
        PyObject *value = ku_py_input(&args[i]);
        if (!value) {
            Py_DECREF(tuple);
            return NULL;
        }
        PyTuple_SET_ITEM(tuple, (Py_ssize_t)i, value);
    }
    return tuple;
}

static PyObject *ku_py_json_arguments(PyObject *json, const KuInteropValue *args, size_t nargs) {
    if (nargs != 1 || args[0].tag != KU_INTEROP_STR) {
        PyErr_SetString(PyExc_TypeError, "Dynamic Python arguments must be encoded as one JSON array");
        return NULL;
    }
    PyObject *argument = ku_py_input(args);
    PyObject *list = argument ? PyObject_CallMethod(json, "loads", "O", argument) : NULL;
    PyObject *tuple = NULL;
    if (list && PyList_Check(list) && PyList_GET_SIZE(list) <= 32) {
        tuple = PyList_AsTuple(list);
    } else if (list) {
        PyErr_SetString(PyExc_TypeError, "Dynamic Python arguments require a JSON array of at most 32 items");
    }
    Py_XDECREF(list);
    Py_XDECREF(argument);
    return tuple;
}

static PyObject *ku_py_json_result(PyObject *json, PyObject *value) {
    PyObject *dumps = PyObject_GetAttrString(json, "dumps");
    PyObject *arguments = dumps ? PyTuple_Pack(1, value) : NULL;
    PyObject *keywords = arguments ? Py_BuildValue("{s:O,s:O}", "ensure_ascii", Py_False, "allow_nan", Py_False) : NULL;
    PyObject *result = keywords ? PyObject_Call(dumps, arguments, keywords) : NULL;
    Py_XDECREF(keywords);
    Py_XDECREF(arguments);
    Py_XDECREF(dumps);
    return result;
}

KU_INTEROP_EXPORT int ku_interop_call_v1(const char *operation, const KuInteropValue *args,
                                         size_t nargs, KuInteropValue *result) {
    memset(result, 0, sizeof(*result));
    if (!operation || nargs > 32 || (nargs && !args)) return ku_py_error(result, "Invalid Python call frame");
    if (ku_py_initialize(result)) return 1;
    PyGILState_STATE gil = PyGILState_Ensure();
    if (ku_py_environment(result)) {
        PyGILState_Release(gil);
        return 1;
    }

    int dynamic = strncmp(operation, "json__", 6) == 0;
    if (dynamic) operation += 6;
    PyObject *module = ku_py_import();
    PyObject *callable = module ? PyObject_GetAttrString(module, operation) : NULL;
    PyObject *json = callable && dynamic ? PyImport_ImportModule("json") : NULL;
    PyObject *tuple = NULL;
    if (callable && !dynamic) tuple = ku_py_arguments(args, nargs);
    if (json) tuple = ku_py_json_arguments(json, args, nargs);
    PyObject *value = tuple ? PyObject_CallObject(callable, tuple) : NULL;
    if (value && dynamic) {
        PyObject *encoded = ku_py_json_result(json, value);
        Py_DECREF(value);
        value = encoded;
    }
    int error = value && ku_py_output(value, result) ? 0 : ku_py_exception(result, operation);
    Py_XDECREF(value);
    Py_XDECREF(json);
    Py_XDECREF(tuple);
    Py_XDECREF(callable);
    Py_XDECREF(module);
    PyGILState_Release(gil);
    return error;
}

KU_INTEROP_EXPORT void ku_interop_free_v1(KuInteropValue *result) {
    free((void *)result->data);
    memset(result, 0, sizeof(*result));
}
