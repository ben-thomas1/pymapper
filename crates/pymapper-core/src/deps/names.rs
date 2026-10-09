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
/// normalisation. Sorted by import name (byte order) for binary search. A name several
/// distributions provide (`cv2`: the four OpenCV builds) has one entry each, the usual one
/// first (it is the one named in findings).
pub const KNOWN: &[(&str, &str)] = &[
    ("Bio", "biopython"),
    ("Crypto", "pycryptodome"),
    ("Crypto", "pycrypto"),
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
    ("cv2", "opencv-python-headless"),
    ("cv2", "opencv-contrib-python"),
    ("cv2", "opencv-contrib-python-headless"),
    ("dateutil", "python-dateutil"),
    ("dns", "dnspython"),
    ("docx", "python-docx"),
    ("dotenv", "python-dotenv"),
    ("engineio", "python-engineio"),
    ("faiss", "faiss-cpu"),
    ("faiss", "faiss-gpu"),
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

/// The distributions of an import name that is not installed: its [`KNOWN`] entries (first =
/// the one to name), else the name itself as a guess. `None` for names that are not
/// distributions.
pub fn distributions_of(top: &[u8]) -> Option<Known<'_>> {
    if NOT_DISTRIBUTIONS.iter().any(|n| n.as_bytes() == top) {
        return None;
    }
    let start = KNOWN.partition_point(|(m, _)| m.as_bytes() < top);
    let len = KNOWN[start..].iter().take_while(|(m, _)| m.as_bytes() == top).count();
    Some(match len {
        0 => Known::Guess(top),
        _ => Known::Table(&KNOWN[start..start + len]),
    })
}

/// Result of [`distributions_of`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Known<'a> {
    /// The [`KNOWN`] entries of the name (one or more), [`Mapping::Table`].
    Table(&'static [(&'static str, &'static str)]),
    /// The import name as the distribution name, [`Mapping::Guess`].
    Guess(&'a [u8]),
}

/// Import names of a distribution that is not installed, by normalised name (`norm`): its
/// [`KNOWN`] entries with [`Mapping::Table`], then the guess (`-` and `.` as `_`, in
/// `scratch`) with [`Mapping::Guess`] unless an entry already named it (`attrs` installs both
/// `attr` and `attrs`). Calls `f` once per name. The table is tiny and only asked for declared
/// dependencies that are not installed.
pub fn import_names_of(norm: &[u8], scratch: &mut Vec<u8>, mut f: impl FnMut(&[u8], Mapping)) {
    let mut guess = Vec::with_capacity(norm.len());
    guess.extend(norm.iter().map(|&b| if b == b'-' || b == b'.' { b'_' } else { b }));
    let mut named = false;
    for &(m, d) in KNOWN {
        crate::roots::normalize_name(d.as_bytes(), scratch);
        if scratch.as_slice() == norm {
            f(m.as_bytes(), Mapping::Table);
            named |= m.as_bytes() == guess.as_slice();
        }
    }
    if !named {
        f(&guess, Mapping::Guess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_table_is_sorted_and_differs_from_guesses() {
        assert!(KNOWN.windows(2).all(|w| w[0].0.as_bytes() <= w[1].0.as_bytes() && w[0] != w[1]), "KNOWN must be sorted");
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
        assert_eq!(distributions_of(b"yaml"), Some(Known::Table(&[("yaml", "PyYAML")])));
        assert_eq!(distributions_of(b"sklearn"), Some(Known::Table(&[("sklearn", "scikit-learn")])));
        assert_eq!(distributions_of(b"requests"), Some(Known::Guess(b"requests")));
        assert_eq!(distributions_of(b"_typeshed"), None);
        // Several distributions provide `cv2`: every one is listed, the usual one first.
        let Some(Known::Table(cv2)) = distributions_of(b"cv2") else { panic!("cv2 is known") };
        let dists: Vec<&str> = cv2.iter().map(|(_, d)| *d).collect();
        assert_eq!(dists, ["opencv-python", "opencv-python-headless", "opencv-contrib-python", "opencv-contrib-python-headless"]);
        assert!(Mapping::Metadata.confidence() > Mapping::Table.confidence());
        assert!(Mapping::Table.confidence() > Mapping::Guess.confidence());
    }

    fn names_of(norm: &[u8]) -> Vec<(String, Mapping)> {
        let mut s = Vec::new();
        let mut got = Vec::new();
        import_names_of(norm, &mut s, |n, m| got.push((String::from_utf8_lossy(n).into_owned(), m)));
        got
    }

    #[test]
    fn reverse_lookup_by_normalised_name() {
        let got = names_of(b"pywin32");
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["pythoncom", "pywintypes", "win32api", "win32con", "pywin32"]);
        assert_eq!(got.last().map(|g| g.1), Some(Mapping::Guess));
        assert_eq!(names_of(b"flask-sqlalchemy"), [("flask_sqlalchemy".to_owned(), Mapping::Guess)]);
        assert_eq!(
            names_of(b"pyyaml"),
            [("_yaml".to_owned(), Mapping::Table), ("yaml".to_owned(), Mapping::Table), ("pyyaml".to_owned(), Mapping::Guess)]
        );
        // `attrs` installs `attr` and `attrs`: the guess is kept.
        assert_eq!(names_of(b"attrs"), [("attr".to_owned(), Mapping::Table), ("attrs".to_owned(), Mapping::Guess)]);
        assert_eq!(names_of(b"opencv-python-headless"), [("cv2".to_owned(), Mapping::Table), ("opencv_python_headless".to_owned(), Mapping::Guess)]);
    }
}
