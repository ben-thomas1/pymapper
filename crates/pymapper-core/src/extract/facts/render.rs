//! Human-readable rendering of one file's facts (debugging and golden tests).
//! Not a hot path.

use std::fmt::Write as _;

use super::*;

/// Renders `view` as text, one fact per line, resolving names with `name`.
///
/// Evidence hashes are rendered as a count only (hashes are not readable).
pub fn render<'s>(view: FileView<'_>, name: impl Fn(Sym) -> &'s [u8]) -> String {
    let st = view.store;
    let ff = view.ff;
    let mut out = String::new();
    let s = |sym: Sym| String::from_utf8_lossy(name(sym)).into_owned();
    let chain = |c: &[Sym]| c.iter().map(|&x| s(x)).collect::<Vec<_>>().join(".");
    let scope = |d: Scope| -> String {
        let row = view.def_row(d);
        match st.defs.name[row] {
            Some(n) => format!("{d}:{}", s(n)),
            None => format!("{d}:<module>"),
        }
    };

    let _ = write!(out, "file lines={} evidence={}", ff.lines, ff.evidence.len);
    flag_list(&mut out, ff.flags as u16, &[
        (file_flags::MAIN_GUARD as u16, "main_guard"),
        (file_flags::ALL_DEFINED as u16, "all_defined"),
        (file_flags::ALL_DYNAMIC as u16, "all_dynamic"),
        (file_flags::TRUNCATED as u16, "truncated"),
    ]);
    out.push('\n');

    for d in 0..ff.defs.len {
        let row = view.def_row(d);
        let defs = &st.defs;
        let kind = match DefKind::from_u8(defs.kind[row]) {
            DefKind::Module => "module",
            DefKind::Function => "def",
            DefKind::Class => "class",
        };
        let nm = defs.name[row].map_or_else(|| "<module>".to_owned(), s);
        let _ = write!(out, "{kind} {d}:{nm}");
        if defs.parent[row] != NO_PARENT {
            let _ = write!(out, " in={}", defs.parent[row]);
        }
        let _ = write!(out, " L{}-{} b{}-{}", defs.start_line[row], defs.end_line[row], defs.start[row], defs.end[row]);
        if defs.name_line[row] != 0 {
            let _ = write!(out, " @{}:{}", defs.name_line[row], defs.name_col[row]);
        }
        let decs: Vec<String> = view
            .decorators(d)
            .map(|i| {
                let f = st.decorators.flags[i];
                let c = if f & dec_flags::COMPLEX != 0 { "<expr>".to_owned() } else { chain(view.decorator_chain(i)) };
                format!("{c}{}", if f & dec_flags::CALLED != 0 { "()" } else { "" })
            })
            .collect();
        if !decs.is_empty() {
            let _ = write!(out, " decs=[{}]", decs.join(", "));
        }
        let bases: Vec<String> = view
            .bases(d)
            .map(|i| {
                let f = st.bases.flags[i];
                let kw = st.bases.keyword[i].map(|k| format!("{}=", s(k))).unwrap_or_default();
                let c = view.base_chain(i);
                let c = if c.is_empty() { "<expr>".to_owned() } else { chain(c) };
                let star = if f & base_flags::STARRED != 0 { "*" } else { "" };
                let sub = if f & base_flags::SUBSCRIPTED != 0 { "[]" } else { "" };
                format!("{star}{kw}{c}{sub}")
            })
            .collect();
        if !bases.is_empty() {
            let _ = write!(out, " bases=[{}]", bases.join(", "));
        }
        let params: Vec<String> = view
            .params(d)
            .map(|i| {
                let f = st.params.flags[i];
                let k = match param_kind(f) {
                    ParamKind::PositionalOnly => "/",
                    ParamKind::Normal => "",
                    ParamKind::VarPositional => "*",
                    ParamKind::KeywordOnly => "kw:",
                    ParamKind::VarKeyword => "**",
                };
                let a = if f & param_flags::ANNOTATED != 0 { ":" } else { "" };
                let dflt = if f & param_flags::DEFAULT != 0 { "=" } else { "" };
                format!("{k}{}{a}{dflt}", s(st.params.name[i]))
            })
            .collect();
        if DefKind::from_u8(defs.kind[row]) == DefKind::Function {
            let _ = write!(out, " ({})", params.join(", "));
        }
        use def_flags::*;
        flag_list(&mut out, defs.flags[row], &[
            (ASYNC, "async"),
            (PROPERTY, "property"),
            (STATICMETHOD, "staticmethod"),
            (CLASSMETHOD, "classmethod"),
            (OVERLOAD, "overload"),
            (PRIVATE, "private"),
            (DUNDER, "dunder"),
            (ABSTRACT, "abstract"),
            (METHOD, "method"),
            (NESTED, "nested"),
            (TYPE_CHECKING, "type_checking"),
            (CONDITIONAL, "conditional"),
            (RETURN_ANNOTATION, "returns"),
            (OTHER_DECORATOR, "other_decorator"),
        ]);
        out.push('\n');
    }

    for i in ff.imports.range() {
        let im = &st.imports;
        let f = im.flags[i];
        let dots = ".".repeat(im.level[i] as usize);
        let _ = write!(out, "import {} L{}:{} ", scope(im.scope[i]), im.line[i], im.col[i]);
        if f & import_flags::FROM != 0 {
            let what = if f & import_flags::STAR != 0 { "*".to_owned() } else { im.name[i].map_or_else(String::new, s) };
            let _ = write!(out, "from {dots}{} import {what}", chain(view.import_module(i)));
        } else {
            let _ = write!(out, "import {}", chain(view.import_module(i)));
        }
        if let Some(a) = im.alias[i] {
            let _ = write!(out, " as {}", s(a));
        }
        flag_list(&mut out, f as u16, &[
            (import_flags::TYPE_CHECKING as u16, "type_checking"),
            (import_flags::TRY_IMPORT_ERROR as u16, "try_import_error"),
            (import_flags::CONDITIONAL as u16, "conditional"),
        ]);
        out.push('\n');
    }

    for i in ff.calls.range() {
        let c = &st.calls;
        let recv = match receiver(c.receiver[i]) {
            Receiver::None => "",
            Receiver::Dotted => " dotted",
            Receiver::SelfAttr => " self",
            Receiver::ClsAttr => " cls",
            Receiver::Super => " super",
            Receiver::Unknown => " unknown",
        };
        let _ = write!(out, "call {} L{}:{} {}(){recv}", scope(c.scope[i]), c.line[i], c.col[i], chain(view.call_chain(i)));
        flag_list(&mut out, c.flags[i] as u16, &[
            (call_flags::MAIN_GUARD as u16, "main_guard"),
            (call_flags::TYPE_CHECKING as u16, "type_checking"),
            (call_flags::LAMBDA as u16, "lambda"),
        ]);
        out.push('\n');
    }

    for i in ff.bindings.range() {
        let b = &st.bindings;
        let kind = match assign_kind(b.kind[i]) {
            AssignKind::Plain => "=",
            AssignKind::Augmented => "op=",
            AssignKind::Annotated => ":",
            AssignKind::Walrus => ":=",
            AssignKind::ForTarget => "for",
            AssignKind::AsTarget => "as",
        };
        let value = match ValueKind::from_u8(b.value[i]) {
            ValueKind::None => String::new(),
            ValueKind::Call => format!(" {}()", chain(view.binding_value(i))),
            ValueKind::Name => format!(" {}", chain(view.binding_value(i))),
            ValueKind::Strings => " <strings>".to_owned(),
            ValueKind::Other => " <expr>".to_owned(),
        };
        let _ = write!(out, "bind {} L{} {} {kind}{value}", scope(b.scope[i]), b.line[i], chain(view.binding_target(i)));
        flag_list(&mut out, b.flags[i] as u16, &[
            (binding_flags::UNPACKED as u16, "unpacked"),
            (binding_flags::STARRED as u16, "starred"),
            (binding_flags::TYPE_CHECKING as u16, "type_checking"),
            (binding_flags::CONDITIONAL as u16, "conditional"),
        ]);
        out.push('\n');
    }

    for i in ff.decls.range() {
        let d = &st.decls;
        let k = match decl_kind(d.kind[i]) {
            DeclKind::Global => "global",
            DeclKind::Nonlocal => "nonlocal",
        };
        let _ = writeln!(out, "decl {} {k} {}", scope(d.scope[i]), s(d.name[i]));
    }

    let exports = view.exports();
    if !exports.is_empty() {
        let names: Vec<String> = exports.iter().map(|&x| s(x)).collect();
        let _ = writeln!(out, "all [{}]", names.join(", "));
    }

    for i in ff.diags.range() {
        let d = &st.diags;
        let _ = writeln!(out, "diag L{}:{} {:?}", d.line[i], d.col[i], diag_kind(d.kind[i]));
    }

    for i in ff.suppressions.range() {
        let p = &st.suppressions;
        if p.all[i] != 0 {
            let _ = writeln!(out, "suppress L{} all", p.line[i]);
        } else {
            let codes: Vec<String> = view.chain(p.codes[i]).iter().map(|&x| s(x)).collect();
            let _ = writeln!(out, "suppress L{} [{}]", p.line[i], codes.join(", "));
        }
    }
    out
}

fn flag_list(out: &mut String, flags: u16, names: &[(u16, &str)]) {
    let set: Vec<&str> = names.iter().filter(|(f, _)| flags & f != 0).map(|&(_, n)| n).collect();
    if !set.is_empty() {
        let _ = write!(out, " [{}]", set.join(","));
    }
}
