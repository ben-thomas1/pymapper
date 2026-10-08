//! Static name tables of the call resolver: the `builtins` module and the method names of
//! builtin types (used to keep the unique-method-name fallback off `x.append()` and friends).

use crate::intern::{Interner, Sym};

/// Every name of the `builtins` module of CPython 3.14 (`dir(builtins)`, dunders other than
/// `__import__` / `__build_class__` excluded). 3.14 adds no builtin names over 3.13.
pub const BUILTINS: &[&str] = &[
    "ArithmeticError", "AssertionError", "AttributeError", "BaseException", "BaseExceptionGroup",
    "BlockingIOError", "BrokenPipeError", "BufferError", "BytesWarning", "ChildProcessError",
    "ConnectionAbortedError", "ConnectionError", "ConnectionRefusedError", "ConnectionResetError",
    "DeprecationWarning", "EOFError", "Ellipsis", "EncodingWarning", "EnvironmentError", "Exception",
    "ExceptionGroup", "False", "FileExistsError", "FileNotFoundError", "FloatingPointError",
    "FutureWarning", "GeneratorExit", "IOError", "ImportError", "ImportWarning", "IndentationError",
    "IndexError", "InterruptedError", "IsADirectoryError", "KeyError", "KeyboardInterrupt",
    "LookupError", "MemoryError", "ModuleNotFoundError", "NameError", "None", "NotADirectoryError",
    "NotImplemented", "NotImplementedError", "OSError", "OverflowError", "PendingDeprecationWarning",
    "PermissionError", "ProcessLookupError", "PythonFinalizationError", "RecursionError",
    "ReferenceError", "ResourceWarning", "RuntimeError", "RuntimeWarning", "StopAsyncIteration",
    "StopIteration", "SyntaxError", "SyntaxWarning", "SystemError", "SystemExit", "TabError",
    "TimeoutError", "True", "TypeError", "UnboundLocalError", "UnicodeDecodeError",
    "UnicodeEncodeError", "UnicodeError", "UnicodeTranslateError", "UnicodeWarning", "UserWarning",
    "ValueError", "Warning", "ZeroDivisionError", "__build_class__", "__import__", "abs", "aiter",
    "all", "anext", "any", "ascii", "bin", "bool", "breakpoint", "bytearray", "bytes", "callable",
    "chr", "classmethod", "compile", "complex", "copyright", "credits", "delattr", "dict", "dir",
    "divmod", "enumerate", "eval", "exec", "exit", "filter", "float", "format", "frozenset",
    "getattr", "globals", "hasattr", "hash", "help", "hex", "id", "input", "int", "isinstance",
    "issubclass", "iter", "len", "license", "list", "locals", "map", "max", "memoryview", "min",
    "next", "object", "oct", "open", "ord", "pow", "print", "property", "quit", "range", "repr",
    "reversed", "round", "set", "setattr", "slice", "sorted", "staticmethod", "str", "sum", "super",
    "tuple", "type", "vars", "zip",
];

/// Method names of builtin types and common file/IO objects (`str`, `bytes`, `list`, `dict`,
/// `set`, `int`, `float`, `tuple`, `object`, files, iterators). A call `x.name()` on an unknown
/// receiver with one of these names is far more likely a builtin method than the one
/// first-party method of that name, so the unique-method fallback skips them.
pub const BUILTIN_METHODS: &[&str] = &[
    // str / bytes
    "capitalize", "casefold", "center", "count", "decode", "encode", "endswith", "expandtabs",
    "find", "format", "format_map", "index", "isalnum", "isalpha", "isascii", "isdecimal",
    "isdigit", "isidentifier", "islower", "isnumeric", "isprintable", "isspace", "istitle",
    "isupper", "join", "ljust", "lower", "lstrip", "maketrans", "partition", "removeprefix",
    "removesuffix", "replace", "rfind", "rindex", "rjust", "rpartition", "rsplit", "rstrip",
    "split", "splitlines", "startswith", "strip", "swapcase", "title", "translate", "upper",
    "zfill", "hex", "fromhex",
    // list / dict / set / tuple
    "append", "clear", "copy", "extend", "insert", "pop", "remove", "reverse", "sort", "fromkeys",
    "get", "items", "keys", "popitem", "setdefault", "update", "values", "add", "difference",
    "difference_update", "discard", "intersection", "intersection_update", "isdisjoint",
    "issubset", "issuperset", "symmetric_difference", "symmetric_difference_update", "union",
    // numbers
    "as_integer_ratio", "bit_count", "bit_length", "conjugate", "from_bytes", "to_bytes",
    "is_integer",
    // files / io / iterators / generators
    "close", "flush", "read", "readline", "readlines", "seek", "tell", "truncate", "write",
    "writelines", "fileno", "isatty", "readable", "writable", "seekable", "send", "throw",
    "mro",
];

/// A sorted table of interned names (binary search over raw `Sym` values).
#[derive(Debug, Default)]
pub(super) struct SymSet {
    syms: Vec<u32>,
}

impl SymSet {
    /// The interned subset of `names` (a name that was never interned cannot occur).
    pub fn new(strings: &Interner, names: &[&str]) -> SymSet {
        let mut syms: Vec<u32> = names.iter().filter_map(|n| strings.get(n.as_bytes())).map(Sym::to_raw).collect();
        syms.sort_unstable();
        syms.dedup();
        SymSet { syms }
    }

    #[inline]
    pub fn contains(&self, s: Sym) -> bool {
        self.syms.binary_search(&s.to_raw()).is_ok()
    }
}
