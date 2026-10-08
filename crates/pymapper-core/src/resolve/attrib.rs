//! Attribution of imports that resolve to no loaded module: the stdlib, a third-party
//! distribution, or unknown.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::intern::Interner;
use crate::roots::{DistId, Roots, is_identifier};

/// Where an unresolved top-level import name comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Attributed {
    Stdlib,
    ThirdParty(DistId),
    Unknown,
}

/// `owner` value of a stdlib name.
const STDLIB: u32 = u32::MAX;

/// Top-level import names the loaded code does not provide, by origin: a built-in stdlib
/// table (every supported version, so it works without an interpreter), the top-level entries
/// of the discovered stdlib directory, and every distribution's top-level names
/// (`top_level.txt`, `RECORD`, editable mappings). Stdlib names win over distributions, like
/// `sys.path` order. One hash lookup per query.
#[derive(Debug, Default)]
pub struct Attribution {
    names: Interner,
    /// Per name (`Sym` index): `STDLIB` or a [`DistId`].
    owner: Vec<u32>,
}

impl Attribution {
    /// The built-in stdlib table only.
    pub fn new() -> Attribution {
        Attribution::default()
    }

    /// The built-in stdlib table, the stdlib directory and the distributions of `roots`.
    pub fn from_roots(roots: &Roots) -> Attribution {
        let mut a = Attribution::new();
        if let Some(dir) = &roots.stdlib {
            a.add_stdlib_dir(dir);
        }
        for d in roots.dists.ids() {
            for top in roots.dists.top_level(d) {
                a.add(top, d.0);
            }
        }
        a
    }

    /// Adds the top-level modules of a stdlib directory (`lib/python3.N`): `*.py` stems,
    /// identifier directories and the extension modules of `lib-dynload`. Unreadable
    /// directories add nothing.
    pub fn add_stdlib_dir(&mut self, dir: &Path) {
        self.add_dir(dir, false);
        self.add_dir(&dir.join("lib-dynload"), true);
    }

    fn add_dir(&mut self, dir: &Path, extensions: bool) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let n = name.as_bytes();
            let stem = if extensions {
                // `_ssl.cpython-313-darwin.so` -> `_ssl`
                let Some(i) = n.iter().position(|&c| c == b'.') else { continue };
                if !(n.ends_with(b".so") || n.ends_with(b".pyd")) {
                    continue;
                }
                &n[..i]
            } else if let Some(s) = n.strip_suffix(b".py") {
                s
            } else if ent.file_type().is_ok_and(|t| t.is_dir()) && n != b"site-packages" {
                n
            } else {
                continue;
            };
            if is_identifier(stem) {
                self.add(stem, STDLIB);
            }
        }
    }

    /// Records `name` as provided by `dist` (first provider wins), unless it is a stdlib name.
    pub fn add_dist_name(&mut self, name: &[u8], dist: DistId) {
        self.add(name, dist.0);
    }

    fn add(&mut self, name: &[u8], owner: u32) {
        if is_builtin_stdlib(name) {
            return;
        }
        let s = self.names.intern(name);
        if s.index() == self.owner.len() {
            self.owner.push(owner);
        } else if owner == STDLIB {
            self.owner[s.index()] = STDLIB;
        }
    }

    /// Origin of the top-level import name `top`.
    pub fn classify(&self, top: &[u8]) -> Attributed {
        if is_builtin_stdlib(top) {
            return Attributed::Stdlib;
        }
        match self.names.get(top).map(|s| self.owner[s.index()]) {
            Some(STDLIB) => Attributed::Stdlib,
            Some(d) => Attributed::ThirdParty(DistId(d)),
            None => Attributed::Unknown,
        }
    }

    pub fn heap_bytes(&self) -> usize {
        self.names.heap_bytes() + self.owner.capacity() * 4
    }
}

/// `name` is in the built-in stdlib table.
pub fn is_builtin_stdlib(name: &[u8]) -> bool {
    STDLIB_NAMES.binary_search_by(|n| n.as_bytes().cmp(name)).is_ok()
}

/// Top-level stdlib module names: the union of `sys.stdlib_module_names` of CPython 3.12 and
/// 3.13, plus modules removed in 3.12 (`distutils`, `imp`, `asynchat`, `asyncore`, `smtpd`) and
/// added in 3.14 (`annotationlib`, `compression`), plus `__main__` (always in `sys.modules`).
/// Sorted (byte order) for binary search.
pub(super) const STDLIB_NAMES: &[&str] = &[
    "__future__", "__main__", "_abc", "_aix_support", "_android_support", "_apple_support", "_ast", "_asyncio",
    "_bisect", "_blake2", "_bz2", "_codecs", "_codecs_cn", "_codecs_hk", "_codecs_iso2022",
    "_codecs_jp", "_codecs_kr", "_codecs_tw", "_collections", "_collections_abc", "_colorize",
    "_compat_pickle", "_compression", "_contextvars", "_crypt", "_csv", "_ctypes", "_curses",
    "_curses_panel", "_datetime", "_dbm", "_decimal", "_elementtree", "_frozen_importlib",
    "_frozen_importlib_external", "_functools", "_gdbm", "_hashlib", "_heapq", "_imp",
    "_interpchannels", "_interpqueues", "_interpreters", "_io", "_ios_support", "_json", "_locale",
    "_lsprof", "_lzma", "_markupbase", "_md5", "_msi", "_multibytecodec", "_multiprocessing",
    "_opcode", "_opcode_metadata", "_operator", "_osx_support", "_overlapped", "_pickle",
    "_posixshmem", "_posixsubprocess", "_py_abc", "_pydatetime", "_pydecimal", "_pyio", "_pylong",
    "_pyrepl", "_queue", "_random", "_scproxy", "_sha1", "_sha2", "_sha3", "_signal",
    "_sitebuiltins", "_socket", "_sqlite3", "_sre", "_ssl", "_stat", "_statistics", "_string",
    "_strptime", "_struct", "_suggestions", "_symtable", "_sysconfig", "_thread",
    "_threading_local", "_tkinter", "_tokenize", "_tracemalloc", "_typing", "_uuid", "_warnings",
    "_weakref", "_weakrefset", "_winapi", "_wmi", "_zoneinfo", "abc", "aifc", "annotationlib",
    "antigravity", "argparse", "array", "ast", "asynchat", "asyncio", "asyncore", "atexit",
    "audioop", "base64", "bdb", "binascii", "bisect", "builtins", "bz2", "cProfile", "calendar",
    "cgi", "cgitb", "chunk", "cmath", "cmd", "code", "codecs", "codeop", "collections", "colorsys",
    "compileall", "compression", "concurrent", "configparser", "contextlib", "contextvars", "copy",
    "copyreg", "crypt", "csv", "ctypes", "curses", "dataclasses", "datetime", "dbm", "decimal",
    "difflib", "dis", "distutils", "doctest", "email", "encodings", "ensurepip", "enum", "errno",
    "faulthandler", "fcntl", "filecmp", "fileinput", "fnmatch", "fractions", "ftplib", "functools",
    "gc", "genericpath", "getopt", "getpass", "gettext", "glob", "graphlib", "grp", "gzip",
    "hashlib", "heapq", "hmac", "html", "http", "idlelib", "imaplib", "imghdr", "imp", "importlib",
    "inspect", "io", "ipaddress", "itertools", "json", "keyword", "lib2to3", "linecache", "locale",
    "logging", "lzma", "mailbox", "mailcap", "marshal", "math", "mimetypes", "mmap", "modulefinder",
    "msilib", "msvcrt", "multiprocessing", "netrc", "nis", "nntplib", "nt", "ntpath", "nturl2path",
    "numbers", "opcode", "operator", "optparse", "os", "ossaudiodev", "pathlib", "pdb", "pickle",
    "pickletools", "pipes", "pkgutil", "platform", "plistlib", "poplib", "posix", "posixpath",
    "pprint", "profile", "pstats", "pty", "pwd", "py_compile", "pyclbr", "pydoc", "pydoc_data",
    "pyexpat", "queue", "quopri", "random", "re", "readline", "reprlib", "resource", "rlcompleter",
    "runpy", "sched", "secrets", "select", "selectors", "shelve", "shlex", "shutil", "signal",
    "site", "smtpd", "smtplib", "sndhdr", "socket", "socketserver", "spwd", "sqlite3",
    "sre_compile", "sre_constants", "sre_parse", "ssl", "stat", "statistics", "string",
    "stringprep", "struct", "subprocess", "sunau", "symtable", "sys", "sysconfig", "syslog",
    "tabnanny", "tarfile", "telnetlib", "tempfile", "termios", "textwrap", "this", "threading",
    "time", "timeit", "tkinter", "token", "tokenize", "tomllib", "trace", "traceback",
    "tracemalloc", "tty", "turtle", "turtledemo", "types", "typing", "unicodedata", "unittest",
    "urllib", "uu", "uuid", "venv", "warnings", "wave", "weakref", "webbrowser", "winreg",
    "winsound", "wsgiref", "xdrlib", "xml", "xmlrpc", "zipapp", "zipfile", "zipimport", "zlib",
    "zoneinfo",
];
