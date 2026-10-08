//! Layering contracts: layers, forbidden, independence, exemptions, transitive chains.

use super::compile::pattern_matches;
use super::*;
use crate::analysis::cycles::tests::{Fx, fx};
use crate::config::{ContractKind, ImportPattern};

fn contract(name: &str, kind: ContractKind) -> Contract {
    Contract { name: name.to_owned(), kind, allow_type_checking: true, transitive: false, ignore_imports: Vec::new() }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

fn layers(name: &str, layers: &[&[&str]]) -> Contract {
    let layers = layers.iter().map(|l| strings(l)).collect();
    contract(name, ContractKind::Layers { layers, containers: Vec::new(), independent_siblings: false })
}

fn forbidden(name: &str, source: &[&str], forbidden: &[&str]) -> Contract {
    contract(name, ContractKind::Forbidden { source: strings(source), forbidden: strings(forbidden) })
}

fn independence(name: &str, modules: &[&str]) -> Contract {
    contract(name, ContractKind::Independence { modules: strings(modules) })
}

fn ignore(c: &mut Contract, pats: &[(&str, &str)]) {
    c.ignore_imports =
        pats.iter().map(|(a, b)| ImportPattern { importer: (*a).to_owned(), imported: (*b).to_owned() }).collect();
}

fn check(f: &Fx, contracts: &[Contract]) -> (LayersReport, Vec<String>) {
    let mut out = Findings::new();
    let report = analyse(&f.ix, &f.imp, contracts, &mut out);
    assert_eq!(report.violations, out.len() as u64);
    let mut lines = f.render(&out, contracts);
    lines.sort();
    (report, lines)
}

const APP: &[(&str, &str)] = &[
    ("myapp/__init__.py", ""),
    ("myapp/api/__init__.py", "from myapp.service import run\n"),
    ("myapp/api/views.py", "from myapp import db\nimport myapp.service.core\n"),
    ("myapp/service/__init__.py", "def run(): pass\n"),
    ("myapp/service/core.py", "from myapp.api.views import a, b\nfrom myapp.db import models\n"),
    (
        "myapp/db/models.py",
        "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from myapp.api import views\nimport myapp.util\n",
    ),
    ("myapp/db/__init__.py", "from . import models\n"),
    ("myapp/util.py", "def f():\n    from myapp.api import views\n"),
];

#[test]
fn layers_report_imports_of_higher_layers() {
    let f = fx(APP);
    let c = [layers("arch", &[&["myapp.api"], &["myapp.service"], &["myapp.db"]])];
    let (report, lines) = check(&f, &c);
    assert!(report.missing.is_empty());
    // One finding for the two rows of `from myapp.api.views import a, b`.
    assert_eq!(lines, ["myapp/service/core.py:1:29: PM302 `myapp.service.core` imports higher layer `myapp.api.views` (contract `arch`)"]);
}

#[test]
fn type_checking_imports_are_exempt_unless_disallowed() {
    let f = fx(APP);
    let mut c = layers("arch", &[&["myapp.api"], &["myapp.service"], &["myapp.db"]]);
    c.allow_type_checking = false;
    let (_, lines) = check(&f, &[c]);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("myapp/db/models.py:3:"), "{lines:?}");
    assert!(lines[0].contains("`myapp.db.models` imports higher layer `myapp.api.views`"));
}

#[test]
fn transitive_layers_follow_unlisted_modules() {
    let f = fx(APP);
    let mut c = layers("arch", &[&["myapp.api"], &["myapp.service"], &["myapp.db"]]);
    c.transitive = true;
    let (_, lines) = check(&f, &[c]);
    // db.models -> util (unlisted) -> api.views, reported at the import of util.
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "myapp/db/models.py:4:8: PM302 `myapp.db.models` imports higher layer `myapp.api.views` (contract `arch`)");
}

#[test]
fn independent_siblings_and_containers() {
    let f = fx(&[
        ("shop/__init__.py", ""),
        ("shop/a/__init__.py", ""),
        ("shop/a/views.py", "from shop.a import billing\n"),
        ("shop/a/billing.py", "from shop.a import accounts\n"),
        ("shop/a/accounts.py", ""),
        ("shop/a/models.py", "from shop.a import views\nfrom shop.b import views as bv\n"),
        ("shop/b/__init__.py", ""),
        ("shop/b/views.py", ""),
        ("shop/b/models.py", "import shop.b.views\n"),
    ]);
    let c = Contract {
        kind: ContractKind::Layers {
            layers: vec![strings(&["views"]), strings(&["billing", "accounts"]), strings(&["models"])],
            containers: strings(&["shop.a", "shop.b"]),
            independent_siblings: true,
        },
        ..contract("apps", ContractKind::Independence { modules: Vec::new() })
    };
    let (report, lines) = check(&f, &[c]);
    let missing: Vec<&str> = report.missing.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(missing, ["shop.b.billing", "shop.b.accounts"]);
    assert_eq!(
        lines,
        [
            "shop/a/billing.py:1:20: PM304 `shop.a.billing` imports `shop.a.accounts`, which must stay independent (contract `apps`)",
            "shop/a/models.py:1:20: PM302 `shop.a.models` imports higher layer `shop.a.views` (contract `apps`)",
            "shop/b/models.py:1:8: PM302 `shop.b.models` imports higher layer `shop.b.views` (contract `apps`)",
        ]
    );
}

const CORE: &[(&str, &str)] = &[
    ("myapp/__init__.py", ""),
    ("myapp/core/__init__.py", "import requests\n"),
    ("myapp/core/a.py", "from requests.adapters import HTTPAdapter\nfrom json import decoder\nimport json\n"),
    ("myapp/core/b.py", "import myapp.web.views\nfrom myapp import helper\n"),
    ("myapp/web/__init__.py", ""),
    ("myapp/web/views.py", ""),
    ("myapp/helper.py", "import requestsfoo\nimport myapp.deep\n"),
    ("myapp/deep.py", "import requests.sessions\n"),
];

#[test]
fn forbidden_loaded_and_external_modules() {
    let f = fx(CORE);
    let c = [forbidden("pure", &["myapp.core"], &["requests", "myapp.web", "json.decoder", "nosuch.mod"])];
    let (report, lines) = check(&f, &c);
    // `nosuch` appears in no import: the name can never match.
    let missing: Vec<&str> = report.missing.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(missing, ["nosuch.mod"]);
    assert_eq!(
        lines,
        [
            "myapp/core/__init__.py:1:8: PM303 `myapp.core` imports forbidden module `requests` (contract `pure`)",
            "myapp/core/a.py:1:31: PM303 `myapp.core.a` imports forbidden module `requests` (contract `pure`)",
            "myapp/core/a.py:2:18: PM303 `myapp.core.a` imports forbidden module `json` (contract `pure`)",
            "myapp/core/b.py:1:8: PM303 `myapp.core.b` imports forbidden module `myapp.web.views` (contract `pure`)",
        ]
    );
}

#[test]
fn transitive_forbidden_reaches_external_names() {
    let f = fx(CORE);
    let mut c = forbidden("pure", &["myapp.core"], &["requests"]);
    c.transitive = true;
    let (_, lines) = check(&f, &[c]);
    // core.b -> helper -> deep -> requests.sessions; `requestsfoo` is not `requests`.
    assert!(
        lines.contains(&"myapp/core/b.py:2:19: PM303 `myapp.core.b` imports forbidden module `requests` (contract `pure`)".to_owned()),
        "{lines:?}"
    );
    assert_eq!(lines.len(), 3, "{lines:?}");
}

const PLUGINS: &[(&str, &str)] = &[
    ("plug/__init__.py", ""),
    ("plug/a/__init__.py", "from plug.a import sub\nfrom plug import shared\n"),
    ("plug/a/sub.py", "import plug.b\n"),
    ("plug/b/__init__.py", "from plug import own\n"),
    ("plug/c.py", "from plug import shared\n"),
    ("plug/shared.py", "import plug.a.sub\nimport plug.c\n"),
    ("plug/own.py", "import plug.b\n"),
];

#[test]
fn independence_direct_and_transitive() {
    let f = fx(PLUGINS);
    let mut c = independence("plugins", &["plug.a", "plug.b", "plug.c"]);
    let (_, lines) = check(&f, std::slice::from_ref(&c));
    assert_eq!(lines, ["plug/a/sub.py:1:8: PM304 `plug.a.sub` imports `plug.b`, which must stay independent (contract `plugins`)"]);
    c.transitive = true;
    let (_, lines) = check(&f, &[c]);
    // a -> shared -> {a.sub, c}: a reaches c. c -> shared -> a.sub: c reaches a. b -> own -> b
    // only reaches itself.
    assert_eq!(
        lines,
        [
            "plug/a/__init__.py:2:18: PM304 `plug.a` imports `plug.c`, which must stay independent (contract `plugins`)",
            "plug/a/sub.py:1:8: PM304 `plug.a.sub` imports `plug.b`, which must stay independent (contract `plugins`)",
            "plug/c.py:1:18: PM304 `plug.c` imports `plug.a.sub`, which must stay independent (contract `plugins`)",
        ]
    );
}

#[test]
fn ignore_imports_patterns() {
    let f = fx(PLUGINS);
    let mut c = independence("plugins", &["plug.a", "plug.b", "plug.c"]);
    ignore(&mut c, &[("plug.*.sub", "plug.**")]);
    assert!(check(&f, std::slice::from_ref(&c)).1.is_empty());
    ignore(&mut c, &[("plug.a", "plug.b")]);
    assert_eq!(check(&f, &[c]).1.len(), 1);

    let f = fx(CORE);
    let mut c = forbidden("pure", &["myapp.core"], &["requests"]);
    ignore(&mut c, &[("myapp.core.*", "requests.**")]);
    let (_, lines) = check(&f, &[c]);
    assert_eq!(lines, ["myapp/core/__init__.py:1:8: PM303 `myapp.core` imports forbidden module `requests` (contract `pure`)"]);
}

#[test]
fn contract_indices_and_multiple_contracts() {
    let f = fx(PLUGINS);
    let c = [
        layers("unused", &[&["nothing.here"], &["nor.here"]]),
        independence("plugins", &["plug.a", "plug.b"]),
        forbidden("no-b", &["plug.a"], &["plug.b"]),
    ];
    let mut out = Findings::new();
    let report = analyse(&f.ix, &f.imp, &c, &mut out);
    assert_eq!(report.missing.len(), 2);
    let contracts: Vec<u32> = out
        .iter()
        .map(|(_, args)| match args[2] {
            Arg::Contract(i) => i,
            _ => panic!("contract argument"),
        })
        .collect();
    assert_eq!(contracts, [1, 2]);
    assert!(report.rows_checked > 0);
}

#[test]
fn statement_reported_once_per_contract() {
    let f = fx(&[
        ("p/__init__.py", ""),
        ("p/a.py", "from p.b import x, y, z\nfrom p.b import w\n"),
        ("p/b.py", "x = y = z = w = 1\n"),
    ]);
    let c = [independence("one", &["p.a", "p.b"]), forbidden("two", &["p.a"], &["p.b"])];
    let (_, lines) = check(&f, &c);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].starts_with("p/a.py:1:17: PM303"), "{lines:?}");
    assert!(lines[1].starts_with("p/a.py:1:17: PM304"), "{lines:?}");
    assert!(lines[2].starts_with("p/a.py:2:17: PM303"), "{lines:?}");
}

#[test]
fn no_contracts_cost_nothing() {
    let f = fx(PLUGINS);
    let mut out = Findings::new();
    let report = analyse(&f.ix, &f.imp, &[], &mut out);
    assert_eq!(report, LayersReport::default());
    assert!(out.is_empty());
}

fn segs(s: &str) -> Vec<Box<[u8]>> {
    s.split('.').map(|x| Box::from(x.as_bytes())).collect()
}

fn m(pat: &str, name: &str) -> bool {
    let name: Vec<&[u8]> = name.split('.').map(str::as_bytes).collect();
    pattern_matches(&segs(pat), &name)
}

#[test]
fn import_patterns() {
    assert!(m("a.b", "a.b"));
    assert!(!m("a.b", "a.b.c"));
    assert!(!m("a.b", "a"));
    assert!(m("a.*", "a.b"));
    assert!(!m("a.*", "a"));
    assert!(!m("a.*", "a.b.c"));
    assert!(m("a.**", "a"));
    assert!(m("a.**", "a.b.c.d"));
    assert!(m("**.views", "x.y.views"));
    assert!(m("**.views", "views"));
    assert!(!m("**.views", "x.views.y"));
    assert!(m("a.**.z", "a.z"));
    assert!(m("a.**.z", "a.b.c.z"));
    assert!(!m("a.**.z", "a.b.c"));
    assert!(m("a.*.c.**", "a.b.c"));
    assert!(m("**.**", "x"));
    assert!(m("a.**.b.**.c", "a.x.b.y.b.z.c"));
    assert!(!m("a.**.b.*.c", "a.x.b.c"));
}

#[test]
fn stub_files_are_not_checked() {
    // `y.py` + `y.pyi` both import the higher layer: one finding, at the implementation, naming
    // the module (not the index's `#2` stub disambiguation). Stubs never run, so a stub-only
    // module and a stub's imports in a transitive chain do not count either.
    let f = fx(&[
        ("ns/__init__.py", ""),
        ("ns/hi/__init__.py", ""),
        ("ns/hi/x.py", ""),
        ("ns/lo/__init__.py", ""),
        ("ns/lo/y.py", "import ns.hi.x\n"),
        ("ns/lo/y.pyi", "import ns.hi.x\n"),
        ("ns/lo/z.pyi", "import ns.hi.x\n"),
        ("ns/mid.py", ""),
        ("ns/mid.pyi", "import ns.hi.x\n"),
        ("ns/lo/w.py", "import ns.mid\n"),
    ]);
    let want = ["ns/lo/y.py:1:8: PM302 `ns.lo.y` imports higher layer `ns.hi.x` (contract `arch`)"];
    let mut c = layers("arch", &[&["ns.hi"], &["ns.lo"]]);
    let (_, lines) = check(&f, std::slice::from_ref(&c));
    assert_eq!(lines, want);
    c.transitive = true;
    let (_, lines) = check(&f, &[c]);
    assert_eq!(lines, want);
}
