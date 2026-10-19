// CPython as a WebAssembly reactor: the few exports the host drives it with,
// and the one host function Python code calls.
//
// The host calls py_init once, py_exec to run the bridge and a program, and
// py_call for each of the bridge's entry points. Between calls no Python frame
// is on the stack, so the interpreter's whole state is in linear memory.
#include <Python.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define EXPORT(name) __attribute__((export_name(#name)))

// host(operation, request) answers with the length of its answer, which
// host_read then copies out. Both are JSON strings.
__attribute__((import_module("durable"), import_name("host"))) int32_t
durable_host(const char *operation, int32_t operation_len, const char *request,
             int32_t request_len);
__attribute__((import_module("durable"), import_name("host_read"))) void
durable_host_read(char *answer);

// The last call's result (or error text), until the next call.
static char *result;
static int32_t result_len;

static void set_result(const char *text, Py_ssize_t len) {
  free(result);
  result = malloc(len);
  memcpy(result, text, len);
  result_len = (int32_t)len;
}

// Keep the pending exception's traceback as the result; -1.
static int32_t fail(void) {
  PyObject *exception = PyErr_GetRaisedException();
  PyObject *traceback = PyImport_ImportModule("traceback");
  PyObject *text = NULL;
  if (traceback && exception) {
    PyObject *lines = PyObject_CallMethod(traceback, "format_exception", "O",
                                          exception);
    PyObject *empty = PyUnicode_FromString("");
    if (lines && empty)
      text = PyUnicode_Join(empty, lines);
    Py_XDECREF(lines);
    Py_XDECREF(empty);
  }
  PyErr_Clear();
  Py_ssize_t len = 0;
  const char *utf8 = text ? PyUnicode_AsUTF8AndSize(text, &len) : NULL;
  if (utf8)
    set_result(utf8, len);
  else
    set_result("Python error", 12);
  PyErr_Clear();
  Py_XDECREF(text);
  Py_XDECREF(traceback);
  Py_XDECREF(exception);
  return -1;
}

static PyObject *host(PyObject *self, PyObject *args) {
  const char *operation, *request;
  Py_ssize_t operation_len, request_len;
  if (!PyArg_ParseTuple(args, "s#s#", &operation, &operation_len, &request,
                        &request_len))
    return NULL;
  int32_t len = durable_host(operation, (int32_t)operation_len, request,
                             (int32_t)request_len);
  char *answer = malloc(len ? len : 1);
  durable_host_read(answer);
  PyObject *value = PyUnicode_DecodeUTF8(answer, len, "strict");
  free(answer);
  return value;
}

static PyMethodDef methods[] = {
    {"host", host, METH_VARARGS,
     "host(operation, request): the host's JSON answer to a JSON request."},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef module = {
    PyModuleDef_HEAD_INIT, "_durable", NULL, -1, methods,
};

static PyObject *init_module(void) { return PyModule_Create(&module); }

EXPORT(py_alloc) void *py_alloc(int32_t len) { return malloc(len ? len : 1); }
EXPORT(py_free) void py_free(void *ptr) { free(ptr); }
EXPORT(py_result) char *py_result(void) { return result; }
EXPORT(py_result_len) int32_t py_result_len(void) { return result_len; }

// Start the interpreter: isolated, with the standard library at `/lib`, a
// fixed hash seed and no site packages. 0, or -1 with the error as the result.
EXPORT(py_init) int32_t py_init(void) {
  PyImport_AppendInittab("_durable", init_module);
  PyConfig config;
  PyConfig_InitIsolatedConfig(&config);
  config.use_hash_seed = 1;
  config.hash_seed = 0;
  config.site_import = 0;
  config.write_bytecode = 0;
  config.install_signal_handlers = 0;
  config.buffered_stdio = 0;
  PyStatus status = PyConfig_SetString(&config, &config.home, L"/");
  if (!PyStatus_Exception(status)) {
    config.module_search_paths_set = 1;
    status = PyWideStringList_Append(&config.module_search_paths,
                                     L"/lib/python3.15");
  }
  if (!PyStatus_Exception(status))
    status = Py_InitializeFromConfig(&config);
  PyConfig_Clear(&config);
  if (PyStatus_Exception(status)) {
    const char *message = status.err_msg ? status.err_msg : "Python init";
    set_result(message, strlen(message));
    return -1;
  }
  return 0;
}

// Run `source` in __main__, as the file `name`. 0, or -1 with the traceback.
EXPORT(py_exec) int32_t py_exec(const char *source, int32_t source_len,
                                const char *name, int32_t name_len) {
  PyObject *text = PyUnicode_DecodeUTF8(source, source_len, "strict");
  PyObject *file = PyUnicode_DecodeUTF8(name, name_len, "strict");
  PyObject *code =
      text && file ? Py_CompileStringObject(PyUnicode_AsUTF8(text), file,
                                            Py_file_input, NULL, -1)
                   : NULL;
  PyObject *globals = PyModule_GetDict(PyImport_AddModule("__main__"));
  PyObject *value = code ? PyEval_EvalCode(code, globals, globals) : NULL;
  Py_XDECREF(text);
  Py_XDECREF(file);
  Py_XDECREF(code);
  if (!value)
    return fail();
  Py_DECREF(value);
  return 0;
}

// Call __main__.__durable.<name>(argument), or with no argument when
// argument_len is -1. The result is the returned string ("" for None).
// 0, or -1 with the traceback.
EXPORT(py_call) int32_t py_call(const char *name, int32_t name_len,
                                const char *argument, int32_t argument_len) {
  PyObject *bridge = PyObject_GetAttrString(PyImport_AddModule("__main__"),
                                            "__durable");
  PyObject *attribute = PyUnicode_DecodeUTF8(name, name_len, "strict");
  PyObject *function =
      bridge && attribute ? PyObject_GetAttr(bridge, attribute) : NULL;
  PyObject *value = NULL;
  if (function) {
    if (argument_len < 0) {
      value = PyObject_CallNoArgs(function);
    } else {
      PyObject *text = PyUnicode_DecodeUTF8(argument, argument_len, "strict");
      value = text ? PyObject_CallOneArg(function, text) : NULL;
      Py_XDECREF(text);
    }
  }
  Py_XDECREF(bridge);
  Py_XDECREF(attribute);
  Py_XDECREF(function);
  if (!value)
    return fail();
  if (value == Py_None) {
    set_result("", 0);
  } else {
    Py_ssize_t len;
    const char *utf8 = PyUnicode_AsUTF8AndSize(value, &len);
    if (!utf8) {
      Py_DECREF(value);
      return fail();
    }
    set_result(utf8, len);
  }
  Py_DECREF(value);
  return 0;
}
