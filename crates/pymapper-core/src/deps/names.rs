//! Import name <-> distribution name when the installed metadata cannot tell: a built-in table
//! of well-known top-level import names whose distribution is named differently, then a guess
//! (the PEP 503 normalised import name, so `flask_sqlalchemy` guesses `flask-sqlalchemy`).
//!
//! Installed distributions are always mapped through their `top_level.txt` / `RECORD`
//! ([`crate::roots::Distributions::providers`]); this table only serves imports of code that
//! is not installed and declared dependencies that are not installed.

/// How an import name was mapped to a distribution; the confidence of PM601 / PM602 / PM604.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mapping {
    /// Installed metadata (`top_level.txt`, `RECORD`): confidence 100.
    Metadata,
    /// The built-in [`KNOWN`] table: confidence 90.
    Table,
    /// The normalised import name: confidence 60 (below the default gate of 80).
    Guess,
}

impl Mapping {
    /// The finding confidence of a mapping.
    pub fn confidence(self) -> u8 {
        match self {
            Mapping::Metadata => 100,
            Mapping::Table => 90,
            Mapping::Guess => 60,
        }
    }
}

/// `(top-level import name, distribution name)` pairs where the two differ beyond PEP 503
/// normalisation. Sorted by import name (byte order) for binary search.
pub const KNOWN: &[(&str, &str)] = &[
    ("Bio", "biopython"),
    ("Crypto", "pycryptodome"),
    ("Cryptodome", "pycryptodomex"),
    ("MySQLdb", "mysqlclient"),
    ("OpenGL", "PyOpenGL"),
    ("OpenSSL", "pyOpenSSL"),
    ("PIL", "Pillow"),
    ("Xlib", "python-xlib"),
    ("_cffi_backend", "cffi"),
    ("_yaml", "PyYAML"),
    ("attr", "attrs"),
    ("bs4", "beautifulsoup4"),
    ("cv2", "opencv-python"),
    ("dateutil", "python-dateutil"),
    ("dns", "dnspython"),
    ("docx", "python-docx"),
    ("dotenv", "python-dotenv"),
    ("engineio", "python-engineio"),
    ("faiss", "faiss-cpu"),
    ("fitz", "PyMuPDF"),
    ("gi", "PyGObject"),
    ("git", "GitPython"),
    ("github", "PyGithub"),
    ("gitlab", "python-gitlab"),
    ("googleapiclient", "google-api-python-client"),
    ("grpc", "grpcio"),
    ("jose", "python-jose"),
    ("jwt", "PyJWT"),
    ("ldap", "python-ldap"),
    ("magic", "python-magic"),
    ("markdown_it", "markdown-it-py"),
    ("memcache", "python-memcached"),
    ("mpl_toolkits", "matplotlib"),
    ("multipart", "python-multipart"),
    ("nacl", "PyNaCl"),
    ("pkg_resources", "setuptools"),
    ("pptx", "python-pptx"),
    ("pythoncom", "pywin32"),
    ("pywintypes", "pywin32"),
    ("serial", "pyserial"),
    ("skimage", "scikit-image"),
    ("sklearn", "scikit-learn"),
    ("slugify", "python-slugify"),
    ("socketio", "python-socketio"),
    ("socks", "PySocks"),
    ("telegram", "python-telegram-bot"),
    ("umap", "umap-learn"),
    ("usb", "pyusb"),
    ("websocket", "websocket-client"),
    ("win32api", "pywin32"),
    ("win32con", "pywin32"),
    ("wx", "wxPython"),
    ("yaml", "PyYAML"),
    ("zmq", "pyzmq"),
];

/// Top-level names that are never distributions: type-checker-only modules.
const NOT_DISTRIBUTIONS: &[&str] = &["_typeshed"];

/// The distribution of an import name that is not installed: the [`KNOWN`] table, else the
/// name itself as a guess. `None` for names that are not distributions.
pub fn distribution_of(top: &[u8]) -> Option<(&[u8], Mapping)> {
    if NOT_DISTRIBUTIONS.iter().any(|n| n.as_bytes() == top) {
        return None;
    }
    match KNOWN.binary_search_by(|(m, _)| m.as_bytes().cmp(top)) {
        Ok(i) => Some((KNOWN[i].1.as_bytes(), Mapping::Table)),
        Err(_) => Some((top, Mapping::Guess)),
    }
}

/// Import names of a distribution that is not installed, by normalised name (`norm`): its
/// [`KNOWN`] entries, if any, with [`Mapping::Table`]; else the guess (`-` and `.` as `_`) in
/// `scratch`. Calls `f` once per name. The table is tiny and only asked for declared
/// dependencies that are not installed.
pub fn import_names_of(norm: &[u8], scratch: &mut Vec<u8>, mut f: impl FnMut(&[u8], Mapping)) {
    let mut found = false;
    for &(m, d) in KNOWN {
        crate::roots::normalize_name(d.as_bytes(), scratch);
        if scratch.as_slice() == norm {
            f(m.as_bytes(), Mapping::Table);
            found = true;
        }
    }
    if !found {
        scratch.clear();
        scratch.extend(norm.iter().map(|&b| if b == b'-' || b == b'.' { b'_' } else { b }));
        f(scratch, Mapping::Guess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_table_is_sorted_and_differs_from_guesses() {
        assert!(KNOWN.windows(2).all(|w| w[0].0.as_bytes() < w[1].0.as_bytes()), "KNOWN must be sorted");
        let mut a = Vec::new();
        let mut b = Vec::new();
        for (m, d) in KNOWN {
            crate::roots::normalize_name(m.as_bytes(), &mut a);
            crate::roots::normalize_name(d.as_bytes(), &mut b);
            assert_ne!(a, b, "{m} -> {d} needs no table entry");
        }
    }

    #[test]
    fn maps_known_names_then_guesses() {
        assert_eq!(distribution_of(b"yaml"), Some((&b"PyYAML"[..], Mapping::Table)));
        assert_eq!(distribution_of(b"sklearn"), Some((&b"scikit-learn"[..], Mapping::Table)));
        assert_eq!(distribution_of(b"requests"), Some((&b"requests"[..], Mapping::Guess)));
        assert_eq!(distribution_of(b"_typeshed"), None);
        assert!(Mapping::Metadata.confidence() > Mapping::Table.confidence());
        assert!(Mapping::Table.confidence() > Mapping::Guess.confidence());
    }

    #[test]
    fn reverse_lookup_by_normalised_name() {
        let mut s = Vec::new();
        let mut got = Vec::new();
        import_names_of(b"pywin32", &mut s, |n, m| got.push((String::from_utf8_lossy(n).into_owned(), m)));
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["pythoncom", "pywintypes", "win32api", "win32con"]);
        assert!(got.iter().all(|(_, m)| *m == Mapping::Table));
        got.clear();
        import_names_of(b"flask-sqlalchemy", &mut s, |n, m| got.push((String::from_utf8_lossy(n).into_owned(), m)));
        assert_eq!(got, [("flask_sqlalchemy".to_owned(), Mapping::Guess)]);
        got.clear();
        import_names_of(b"pyyaml", &mut s, |n, m| got.push((String::from_utf8_lossy(n).into_owned(), m)));
        assert_eq!(got, [("_yaml".to_owned(), Mapping::Table), ("yaml".to_owned(), Mapping::Table)]);
    }
}
