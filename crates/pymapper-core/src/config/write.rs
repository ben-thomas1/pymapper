//! Serialises a [`Config`] in `pymapper.toml` syntax (for `pymapper config` and round-trip
//! tests). Non-UTF-8 path bytes are written lossily.

use std::fmt::{Display, Write};
use std::path::{Path, PathBuf};

use super::{Config, ContractKind, RuleSelector};

pub(super) fn to_toml(c: &Config) -> String {
    let mut w = W { out: String::with_capacity(4096) };
    w.paths("paths", &c.paths);
    w.paths("src", &c.src);
    w.strs("exclude", &c.exclude);
    w.strs("extend-exclude", &c.extend_exclude);
    w.raw("respect-gitignore", c.respect_gitignore);
    w.raw("include-deps", c.include_deps);
    w.raw("include-stdlib", c.include_stdlib);
    if let Some(v) = &c.venv {
        w.path("venv", v);
    }
    if let Some(v) = c.python_version {
        w.str("python-version", &v.to_string());
    }
    w.raw("cache", c.cache);
    w.path("cache-dir", &c.cache_dir);
    w.raw("threads", c.threads);
    w.str("mode", c.mode.as_str());
    w.str("output-format", c.output_format.as_str());
    w.raw("min-confidence", c.min_confidence);
    if let Some(v) = &c.baseline {
        w.path("baseline", v);
    }
    w.strs("presets", c.presets.iter().map(|p| p.as_str()));

    w.out.push_str("\n[rules]\n");
    if let Some(sel) = &c.rules.select {
        w.selectors("select", sel);
    }
    w.selectors("extend-select", &c.rules.extend_select);
    w.selectors("ignore", &c.rules.ignore);
    if !c.rules.per_file_ignores.is_empty() {
        w.out.push_str("\n[rules.per-file-ignores]\n");
        for pfi in &c.rules.per_file_ignores {
            quote(&mut w.out, &pfi.pattern);
            w.out.push_str(" = ");
            w.list(pfi.rules.iter().map(|s| s.to_string()));
            w.out.push('\n');
        }
    }

    let t = &c.thresholds;
    w.out.push_str("\n[thresholds]\n");
    w.raw("max-mccabe", t.max_mccabe);
    w.raw("max-cognitive", t.max_cognitive);
    w.raw("max-nesting", t.max_nesting);
    w.raw("max-function-loc", t.max_function_loc);
    w.raw("max-params", t.max_params);
    // `{:?}` always prints a decimal point or exponent, so the value stays a TOML float.
    let _ = writeln!(w.out, "min-maintainability = {:?}", t.min_maintainability);
    w.raw("max-module-loc", t.max_module_loc);
    w.raw("max-fan-out", t.max_fan_out);
    w.raw("dupes-min-tokens", t.dupes_min_tokens);
    w.raw("dupes-min-lines", t.dupes_min_lines);

    let e = &c.entrypoints;
    w.out.push_str("\n[entrypoints]\n");
    w.strs("decorators", &e.decorators);
    w.strs("bases", &e.bases);
    w.strs("names", &e.names);
    w.raw("project-scripts", e.project_scripts);

    let a = &c.analysis;
    w.out.push_str("\n[analysis]\n");
    w.raw("dupes", a.dupes);
    w.raw("exact-reachability", a.exact_reachability);
    w.raw("name-fallback", a.name_fallback);
    w.raw("unique-method-fallback", a.unique_method_fallback);
    w.raw("type-checking-cycles", a.type_checking_cycles);
    if let Some(v) = &a.coverage {
        w.path("coverage", v);
    }

    for ct in &c.contracts {
        w.out.push_str("\n[[contracts]]\n");
        w.str("name", &ct.name);
        w.str("kind", ct.kind.name().as_str());
        w.raw("allow-type-checking", ct.allow_type_checking);
        w.raw("transitive", ct.transitive);
        w.strs("ignore-imports", ct.ignore_imports.iter().map(|p| format!("{} -> {}", p.importer, p.imported)));
        match &ct.kind {
            ContractKind::Layers { layers, containers, independent_siblings } => {
                w.out.push_str("layers = [");
                for (i, layer) in layers.iter().enumerate() {
                    if i > 0 {
                        w.out.push_str(", ");
                    }
                    if let [single] = layer.as_slice() {
                        quote(&mut w.out, single);
                    } else {
                        w.list(layer);
                    }
                }
                w.out.push_str("]\n");
                w.strs("containers", containers);
                w.raw("independent-siblings", *independent_siblings);
            }
            ContractKind::Forbidden { source, forbidden } => {
                w.strs("source", source);
                w.strs("forbidden", forbidden);
            }
            ContractKind::Independence { modules } => w.strs("modules", modules),
        }
    }
    w.out
}

struct W {
    out: String,
}

impl W {
    fn raw(&mut self, key: &str, v: impl Display) {
        let _ = writeln!(self.out, "{key} = {v}");
    }

    fn str(&mut self, key: &str, v: &str) {
        self.out.push_str(key);
        self.out.push_str(" = ");
        quote(&mut self.out, v);
        self.out.push('\n');
    }

    fn path(&mut self, key: &str, v: &Path) {
        self.str(key, &v.to_string_lossy());
    }

    fn list<S: AsRef<str>>(&mut self, items: impl IntoIterator<Item = S>) {
        self.out.push('[');
        for (i, s) in items.into_iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            quote(&mut self.out, s.as_ref());
        }
        self.out.push(']');
    }

    fn strs<S: AsRef<str>>(&mut self, key: &str, items: impl IntoIterator<Item = S>) {
        self.out.push_str(key);
        self.out.push_str(" = ");
        self.list(items);
        self.out.push('\n');
    }

    fn paths(&mut self, key: &str, items: &[PathBuf]) {
        self.strs(key, items.iter().map(|p| p.to_string_lossy()));
    }

    fn selectors(&mut self, key: &str, items: &[RuleSelector]) {
        self.strs(key, items.iter().map(|s| s.to_string()));
    }
}

/// Writes `s` as a TOML basic string.
fn quote(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quoting() {
        let mut s = String::new();
        quote(&mut s, "a\"b\\c\nd\u{1}é");
        assert_eq!(s, r#""a\"b\\c\nd\u0001é""#);
    }
}
