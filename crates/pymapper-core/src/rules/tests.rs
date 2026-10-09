//! Tests for the rule framework: registry invariants, messages, findings, selection and gating,
//! filtering, suppressions and the generated docs.

use std::cell::RefCell;
use std::fmt;
use std::path::Path;

use super::finding::{Piece, pieces};
use super::*;
use crate::config::{self, Config, PerFileIgnore, RuleSelector};

// ----- registry -----

#[test]
fn registry_is_nonempty_and_sorted() {
    let codes: Vec<u16> = all().map(|d| d.code).collect();
    assert!(codes.len() >= 20, "{codes:?}");
    assert!(codes.windows(2).all(|w| w[0] < w[1]), "{codes:?}");
    for &cat in Category::ALL {
        let rules = in_category(cat);
        assert!(!rules.is_empty(), "{cat:?} has no rules");
        assert!(rules.iter().all(|d| d.code / 100 == cat.digit()));
    }
}

#[test]
fn names_are_unique_kebab_case() {
    let mut names: Vec<&str> = all().map(|d| d.name).collect();
    for n in &names {
        assert!(!n.is_empty() && n.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'), "{n}");
        assert!(!n.starts_with('-') && !n.ends_with('-') && !n.contains("--"), "{n}");
    }
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "duplicate rule names");
}

#[test]
fn text_fields_are_well_formed() {
    for d in all() {
        assert!(!d.summary.is_empty() && !d.summary.ends_with('.'), "PM{}: summary", d.code);
        assert!(d.summary.starts_with(|c: char| c.is_ascii_uppercase()), "PM{}: summary", d.code);
        assert!(d.explanation.contains("Fix:"), "PM{}: explanation needs a fix hint", d.code);
        assert!(!d.explanation.contains("  "), "PM{}: double space in explanation", d.code);
        assert!(!d.message.ends_with('.'), "PM{}: message", d.code);
        if let ConfidenceModel::Scored(how) = d.confidence {
            assert!(how.len() > 20, "PM{}: confidence doc", d.code);
        }
    }
}

#[test]
fn messages_match_declared_args() {
    for d in all() {
        assert_eq!(d.check_message(), Ok(()), "PM{}: {}", d.code, d.message);
    }
}

#[test]
fn gating_is_consistent() {
    for d in all() {
        assert_eq!(d.cost == CostClass::Expensive, d.gate != Gate::None, "PM{}", d.code);
        // Gated rules are on by default so that turning the toggle on is enough.
        if d.gate != Gate::None {
            assert!(d.default_enabled, "PM{}", d.code);
        }
    }
    assert!(all().any(|d| d.gate == Gate::Dupes));
}

#[test]
fn lookups() {
    let d = lookup(101).unwrap();
    assert_eq!(d.name, "complex-function");
    assert_eq!(codes::COMPLEX_FUNCTION.def(), d);
    assert_eq!(lookup_code("PM101"), Some(d));
    assert_eq!(lookup_name("complex-function"), Some(d));
    assert_eq!(lookup(100), None);
    assert_eq!(lookup(999), None);
    assert_eq!(lookup(u16::MAX), None);
    for bad in ["PM10", "PM1010", "pm101", "PMabc", "101", "", "PM", "PM10a"] {
        assert_eq!(lookup_code(bad), None, "{bad}");
    }
    assert_eq!(lookup_name("nope"), None);
    for d in all() {
        assert_eq!(lookup(d.code), Some(d));
        assert_eq!(lookup_code(&d.rule().to_string()), Some(d));
        assert_eq!(lookup_name(d.name), Some(d));
        assert_eq!(d.rule().def(), d);
    }
}

#[test]
#[should_panic(expected = "unregistered rule code PM999")]
fn def_of_unregistered_code_panics() {
    let _ = Rule(999).def();
}

#[test]
fn rule_display_and_category() {
    assert_eq!(Rule(101).to_string(), "PM101");
    assert_eq!(Rule(7).to_string(), "PM007");
    assert_eq!(Rule(101).category(), Some(Category::Complexity));
    assert_eq!(Rule(650).category(), Some(Category::Dependencies));
    assert_eq!(Rule(42).category(), None);
    assert_eq!(Rule(742).category(), None);
    assert_eq!(Category::of(301), Some(Category::Layering));
    assert_eq!(Category::of(405), Some(Category::DeadCode));
    assert_eq!(Category::of(599), Some(Category::Duplication));
    assert_eq!(Category::of(201), Some(Category::Coupling));
}

#[test]
fn passes_cover_their_categories() {
    for d in all() {
        let ok = match d.code / 100 {
            1 => d.pass == Pass::Hotspot,
            2 => d.pass == Pass::Coupling,
            3 => matches!(d.pass, Pass::Cycles | Pass::Layers),
            4 => d.pass == Pass::DeadCode,
            5 => d.pass == Pass::Dupes,
            6 => d.pass == Pass::Deps,
            _ => false,
        };
        assert!(ok, "PM{} in {:?}", d.code, d.pass);
    }
    for &p in Pass::ALL {
        assert!(all().any(|d| d.pass == p), "{p:?} produces no rule");
    }
}

// ----- thresholds -----

const ALL_KEYS: &[ThresholdKey] = &[
    ThresholdKey::MaxMccabe,
    ThresholdKey::MaxCognitive,
    ThresholdKey::MaxNesting,
    ThresholdKey::MaxFunctionLoc,
    ThresholdKey::MaxParams,
    ThresholdKey::MinMaintainability,
    ThresholdKey::MaxModuleLoc,
    ThresholdKey::MaxFanOut,
    ThresholdKey::MaxDistance,
    ThresholdKey::DistanceMinCoupling,
    ThresholdKey::MaxModuleSymbols,
    ThresholdKey::MaxModuleCoupling,
    ThresholdKey::MaxClassWmc,
    ThresholdKey::MaxClassFanOut,
    ThresholdKey::DupesMinTokens,
    ThresholdKey::DupesMinLines,
];

#[test]
fn every_threshold_is_read_by_a_rule_and_documented() {
    for &k in ALL_KEYS {
        assert!(all().any(|d| d.thresholds.contains(&k)), "{k:?} unused");
        assert!(config::TEMPLATE.contains(&format!("# {} = ", k.key())), "{} not in template", k.key());
        let expected = if k.key().starts_with("max-") {
            Bound::Max
        } else if k.key().starts_with("min-") {
            Bound::Min
        } else {
            Bound::Param
        };
        assert_eq!(k.bound(), expected, "{k:?}");
    }
}

#[test]
fn threshold_values_follow_config() {
    let t = config::Thresholds::default();
    assert_eq!(ThresholdKey::MaxMccabe.get(&t), 10.0);
    assert_eq!(ThresholdKey::MinMaintainability.get(&t), 10.0);
    assert_eq!(ThresholdKey::DupesMinLines.get(&t), 6.0);
    let mut cfg = Config::new(Path::new("/p"));
    cfg.thresholds.max_mccabe = 12;
    cfg.thresholds.max_cognitive = 3;
    cfg.thresholds.max_nesting = 4;
    cfg.thresholds.max_function_loc = 5;
    cfg.thresholds.max_params = 6;
    cfg.thresholds.min_maintainability = 7.5;
    cfg.thresholds.max_module_loc = 8;
    cfg.thresholds.max_fan_out = 9;
    cfg.thresholds.max_distance = 0.5;
    cfg.thresholds.distance_min_coupling = 14;
    cfg.thresholds.max_module_symbols = 15;
    cfg.thresholds.max_module_coupling = 16;
    cfg.thresholds.max_class_wmc = 17;
    cfg.thresholds.max_class_fan_out = 18;
    cfg.thresholds.dupes_min_tokens = 11;
    cfg.thresholds.dupes_min_lines = 13;
    let got: Vec<f64> = ALL_KEYS.iter().map(|k| k.get(&cfg.thresholds)).collect();
    assert_eq!(got, [12.0, 3.0, 4.0, 5.0, 6.0, 7.5, 8.0, 9.0, 0.5, 14.0, 15.0, 16.0, 17.0, 18.0, 11.0, 13.0]);

    let e = RuleEngine::new(&cfg);
    // Strictly beyond the limit.
    assert!(!e.violates(ThresholdKey::MaxMccabe, 12.0));
    assert!(e.violates(ThresholdKey::MaxMccabe, 13.0));
    assert!(!e.violates(ThresholdKey::MinMaintainability, 7.5));
    assert!(e.violates(ThresholdKey::MinMaintainability, 7.4));
    assert!(!e.violates(ThresholdKey::MinMaintainability, 100.0));
    // Parameters are never violated.
    assert!(!e.violates(ThresholdKey::DupesMinTokens, 1e9));
    assert!(!e.violates(ThresholdKey::DupesMinTokens, 0.0));
    assert_eq!(e.thresholds().max_mccabe, 12);
}

// ----- messages -----

/// Resolver printing ids recognisably: `mod.sym3`, `name7`, `file2.py`, `contract1`.
struct Names;

impl ArgResolver for Names {
    fn write_symbol(&self, id: SymbolId, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "mod.sym{}", id.0)
    }
    fn write_name(&self, id: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "name{id}")
    }
    fn write_file(&self, id: FileId, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "file{}.py", id.0)
    }
    fn write_contract(&self, index: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "contract{index}")
    }
}

fn message(rule: Rule, args: &[Arg]) -> String {
    let mut s = String::new();
    rule.def().write_message(args, &Names, &mut s).unwrap();
    s
}

#[test]
fn template_pieces() {
    let got: Vec<_> = pieces("`{0}` is {1} > {2}").collect();
    assert_eq!(
        got,
        [
            Ok(Piece::Lit("`")),
            Ok(Piece::Arg(0)),
            Ok(Piece::Lit("` is ")),
            Ok(Piece::Arg(1)),
            Ok(Piece::Lit(" > ")),
            Ok(Piece::Arg(2)),
        ]
    );
    let got: Vec<_> = pieces("cycle: {0.. -> }").collect();
    assert_eq!(got, [Ok(Piece::Lit("cycle: ")), Ok(Piece::Rest(0, " -> "))]);
    let got: Vec<_> = pieces("{12..}").collect();
    assert_eq!(got, [Ok(Piece::Rest(12, ""))]);
    assert_eq!(pieces("").count(), 0);
    for (bad, at) in [("{", 0), ("a}", 1), ("x{}", 1), ("{a}", 0), ("{123}", 0), ("{0..{}", 0), ("{0.}", 0), ("{0", 0)] {
        let errs: Vec<_> = pieces(bad).filter_map(Result::err).collect();
        assert_eq!(errs, [MessageError::Syntax(at)], "{bad}");
    }
}

#[test]
fn check_message_rejects_mismatches() {
    let base = *codes::COMPLEX_FUNCTION.def();
    let with = |message: &'static str, args: &'static [ArgKind]| RuleDef { message, args, ..base };
    assert_eq!(with("{0} {1} {2}", base.args).check_message(), Ok(()));
    assert_eq!(with("{0} {1}", base.args).check_message(), Err(MessageError::Unused(2)));
    assert_eq!(with("{0} {1} {2} {3}", base.args).check_message(), Err(MessageError::Missing(3)));
    assert_eq!(with("{0..,} {1}", &[ArgKind::U32, ArgKind::U32]).check_message(), Err(MessageError::Rest));
    assert_eq!(with("{0..,}", &[ArgKind::U32, ArgKind::U32]).check_message(), Err(MessageError::Rest));
    assert_eq!(with("{1} {0..,}", &[ArgKind::U32, ArgKind::U32]).check_message(), Err(MessageError::Rest));
    assert_eq!(with("{0} {1..,}", &[ArgKind::U32, ArgKind::U32]).check_message(), Ok(()));
    assert_eq!(with("{0} {3..,}", &[ArgKind::U32, ArgKind::U32]).check_message(), Err(MessageError::Missing(3)));
    assert_eq!(with("bad }", &[]).check_message(), Err(MessageError::Syntax(4)));
    assert_eq!(with("{0}{0}", &[ArgKind::U32]).check_message(), Ok(()));
    let many: &[ArgKind] = &[ArgKind::U32; 17];
    assert!(with("{0}", many).check_message().is_err());
}

#[test]
fn messages_render() {
    use codes::*;
    let s = |i| Arg::Symbol(SymbolId(i));
    assert_eq!(
        message(COMPLEX_FUNCTION, &[s(3), Arg::U32(12), Arg::U32(10)]),
        "`mod.sym3` is too complex (McCabe 12 > 10)"
    );
    assert_eq!(
        message(LOW_MAINTAINABILITY, &[s(1), Arg::F32(7.25), Arg::F32(10.0)]),
        "`mod.sym1` has a low maintainability index (7.2 < 10.0)"
    );
    assert_eq!(message(IMPORT_CYCLE, &[s(1), s(2), s(1)]), "import cycle: mod.sym1 -> mod.sym2 -> mod.sym1");
    assert_eq!(message(IMPORT_CYCLE, &[s(4)]), "import cycle: mod.sym4");
    assert_eq!(
        message(LAYER_VIOLATION, &[s(1), s(2), Arg::Contract(0)]),
        "`mod.sym1` imports higher layer `mod.sym2` (contract `contract0`)"
    );
    assert_eq!(
        message(DUPLICATE_CODE, &[Arg::U32(9), Arg::File(FileId(2)), Arg::U32(40)]),
        "9 lines duplicated in file2.py:40"
    );
    assert_eq!(
        message(UNDECLARED_DEPENDENCY, &[Arg::Name(1), Arg::Name(2)]),
        "`name1` is provided by `name2`, which is not a declared dependency"
    );
    // Missing arguments do not panic.
    assert_eq!(message(COMPLEX_FUNCTION, &[s(3)]), "`mod.sym3` is too complex (McCabe ? > ?)");
}

#[test]
fn every_rule_renders_with_sample_args() {
    for d in all() {
        let args: Vec<Arg> = d
            .args
            .iter()
            .enumerate()
            .map(|(i, k)| match k {
                ArgKind::U32 => Arg::U32(i as u32),
                ArgKind::F32 => Arg::F32(i as f32),
                ArgKind::Symbol => Arg::Symbol(SymbolId(i as u32)),
                ArgKind::Name => Arg::Name(i as u32),
                ArgKind::File => Arg::File(FileId(i as u32)),
                ArgKind::Contract => Arg::Contract(i as u32),
                ArgKind::Phrase(words) => Arg::Phrase(words.len() as u32 - 1),
            })
            .collect();
        assert!(d.accepts(&args), "PM{}", d.code);
        let m = message(d.rule(), &args);
        assert!(!m.contains('{') && !m.contains('}') && !m.contains('?'), "PM{}: {m}", d.code);
    }
}

#[test]
fn accepts_checks_kinds_and_arity() {
    let d = codes::COMPLEX_FUNCTION.def();
    let s = Arg::Symbol(SymbolId(0));
    assert!(d.accepts(&[s, Arg::U32(1), Arg::U32(2)]));
    assert!(!d.accepts(&[s, Arg::U32(1)]));
    assert!(!d.accepts(&[s, Arg::U32(1), Arg::U32(2), Arg::U32(3)]));
    assert!(!d.accepts(&[s, Arg::F32(1.0), Arg::U32(2)]));
    assert!(!d.is_variadic());
    let c = codes::IMPORT_CYCLE.def();
    assert!(c.is_variadic());
    assert!(c.accepts(&[s]));
    assert!(c.accepts(&[s, s, s, s]));
    assert!(!c.accepts(&[]));
    assert!(!c.accepts(&[s, Arg::U32(1)]));
}

// ----- findings -----

#[test]
fn record_sizes() {
    assert_eq!(size_of::<Arg>(), 8);
    assert_eq!(size_of::<Finding>(), 28);
    assert_eq!(size_of::<RuleSet>(), 128);
    assert_eq!(size_of::<SuppressionEntry>(), 8);
}

fn at(file: u32, line: u32) -> Location {
    Location::new(FileId(file), line, 1)
}

/// A dead-code finding (scored rule) with one Name argument equal to `tag`.
fn push_dead(f: &mut Findings, file: u32, line: u32, confidence: u8, tag: u32) {
    f.push(codes::UNUSED_IMPORT, at(file, line), SymbolId::NONE, confidence, &[Arg::Name(tag)]);
}

/// The `tag` of each finding (`1000 + symbol` for findings with symbol arguments).
fn tags(f: &Findings) -> Vec<u32> {
    f.iter()
        .map(|(_, args)| match args {
            [Arg::Name(t)] => *t,
            [Arg::Symbol(s), ..] => 1000 + s.0,
            other => panic!("unexpected args {other:?}"),
        })
        .collect()
}

#[test]
fn findings_push_and_iterate() {
    let mut f = Findings::with_capacity(4, 8);
    assert!(f.is_empty());
    let sym = Arg::Symbol(SymbolId(5));
    f.push(codes::COMPLEX_FUNCTION, at(1, 10), SymbolId(5), 100, &[sym, Arg::U32(11), Arg::U32(10)]);
    push_dead(&mut f, 2, 3, 150, 7);
    assert_eq!(f.len(), 2);
    let (a, args) = f.iter().next().unwrap();
    assert_eq!(a.rule, codes::COMPLEX_FUNCTION);
    assert_eq!(a.loc, Location { file: FileId(1), line: 10, col: 1 });
    assert_eq!(a.symbol, SymbolId(5));
    assert_eq!(args.len(), 3);
    // Confidence is clamped.
    assert_eq!(f.as_slice()[1].confidence, 100);
    assert_eq!(f.args(&f.as_slice()[1]), &[Arg::Name(7)]);
    let mut s = String::new();
    f.write_message(&f.as_slice()[0], &Names, &mut s).unwrap();
    assert_eq!(s, "`mod.sym5` is too complex (McCabe 11 > 10)");
    f.clear();
    assert!(f.is_empty());
}

#[test]
fn findings_append_rebases_args() {
    let mut a = Findings::new();
    push_dead(&mut a, 1, 1, 90, 1);
    push_dead(&mut a, 1, 2, 90, 2);
    let mut b = Findings::new();
    b.push(codes::IMPORT_CYCLE, at(3, 1), SymbolId::NONE, 100, &[Arg::Symbol(SymbolId(1)); 3]);
    push_dead(&mut b, 4, 1, 90, 4);
    a.append(&mut b);
    assert!(b.is_empty());
    assert_eq!(tags(&a), [1, 2, 1001, 4]);
    assert_eq!(a.args(&a.as_slice()[2]).len(), 3);
    // `b` is reusable.
    push_dead(&mut b, 5, 1, 90, 5);
    a.append(&mut b);
    assert_eq!(tags(&a), [1, 2, 1001, 4, 5]);
}

#[test]
fn findings_retain_compacts() {
    let mut f = Findings::new();
    for i in 0..10 {
        push_dead(&mut f, i, 1, 90, i);
    }
    f.push(codes::IMPORT_CYCLE, at(10, 1), SymbolId::NONE, 100, &[Arg::Symbol(SymbolId(7)); 4]);
    f.retain(|x, _| x.loc.file.0 % 3 == 1);
    assert_eq!(tags(&f), [1, 4, 7, 1007]);
    assert_eq!(f.args(&f.as_slice()[3]).len(), 4);
    f.retain(|_, args| args.len() == 1);
    assert_eq!(tags(&f), [1, 4, 7]);
    f.retain(|_, _| false);
    assert!(f.is_empty());
}

#[test]
fn findings_retain_after_sort() {
    let mut f = Findings::new();
    for (i, line) in [5u32, 3, 9, 1, 7].into_iter().enumerate() {
        if i == 2 {
            f.push(codes::IMPORT_CYCLE, at(0, line), SymbolId::NONE, 100, &[Arg::Symbol(SymbolId(3)); 2]);
        } else {
            push_dead(&mut f, 0, line, 90, line);
        }
    }
    f.sort_by_location();
    assert_eq!(f.as_slice().iter().map(|x| x.loc.line).collect::<Vec<_>>(), [1, 3, 5, 7, 9]);
    assert_eq!(tags(&f), [1, 3, 5, 7, 1003]);
    f.retain(|x, _| x.loc.line != 3);
    assert_eq!(tags(&f), [1, 5, 7, 1003]);
    f.sort_by(|a, b| b.loc.line.cmp(&a.loc.line));
    f.retain(|x, _| x.loc.line != 5);
    assert_eq!(tags(&f), [1003, 7, 1]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "do not match")]
fn push_checks_arg_kinds_in_debug() {
    let mut f = Findings::new();
    f.push(codes::COMPLEX_FUNCTION, at(0, 1), SymbolId::NONE, 100, &[Arg::U32(1)]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "certain rule")]
fn push_checks_certain_confidence_in_debug() {
    let mut f = Findings::new();
    let args = [Arg::Symbol(SymbolId(0)), Arg::U32(1), Arg::U32(2)];
    f.push(codes::COMPLEX_FUNCTION, at(0, 1), SymbolId::NONE, 50, &args);
}

// ----- rule sets -----

#[test]
fn rule_set_operations() {
    let mut s = RuleSet::EMPTY;
    assert!(s.is_empty());
    for c in [0u16, 63, 64, 101, 999] {
        s.insert(Rule(c));
    }
    assert_eq!(s.len(), 5);
    assert_eq!(s.iter().map(Rule::code).collect::<Vec<_>>(), [0, 63, 64, 101, 999]);
    assert!(s.contains(Rule(64)) && !s.contains(Rule(65)));
    assert!(!s.contains(Rule(1000)) && !s.contains(Rule(u16::MAX)));
    s.remove(Rule(64));
    assert!(!s.contains(Rule(64)));
    let t: RuleSet = [Rule(63), Rule(500)].into_iter().collect();
    assert_eq!(s.union(&t).len(), 5);
    assert_eq!(s.intersection(&t).iter().collect::<Vec<_>>(), [Rule(63)]);
    assert_eq!(s.difference(&t).iter().map(Rule::code).collect::<Vec<_>>(), [0, 101, 999]);
    assert_eq!(t.into_iter().count(), 2);
    assert_eq!(format!("{t:?}"), "{\"PM063\", \"PM500\"}");
}

// ----- engine: selection and gating -----

fn cfg() -> Config {
    Config::new(Path::new("/p"))
}

fn sel(s: &str) -> RuleSelector {
    s.parse().unwrap()
}

fn enabled_codes(e: &RuleEngine) -> Vec<u16> {
    e.enabled().iter().map(Rule::code).collect()
}

#[test]
fn defaults_enable_cheap_default_rules() {
    let e = RuleEngine::new(&cfg());
    for d in all() {
        assert_eq!(e.is_enabled(d.rule()), d.default_enabled && d.gate == Gate::None, "PM{}", d.code);
        assert_eq!(e.selected().contains(d.rule()), d.default_enabled, "PM{}", d.code);
    }
    assert!(!e.is_enabled(codes::LOW_MAINTAINABILITY));
    assert!(!e.is_enabled(codes::TRANSITIVE_DEPENDENCY));
    assert!(!e.is_enabled(codes::DUPLICATE_CODE));
    assert_eq!(e.gated_off().map(|d| d.code).collect::<Vec<_>>(), [501]);
    for &p in Pass::ALL {
        assert_eq!(e.should_run(p), p != Pass::Dupes, "{p:?}");
    }
    assert_eq!(e.min_confidence(), config::DEFAULT_MIN_CONFIDENCE);
}

#[test]
fn gate_toggle_alone_enables_expensive_rules() {
    let mut c = cfg();
    c.analysis.dupes = true;
    let e = RuleEngine::new(&c);
    assert!(e.is_enabled(codes::DUPLICATE_CODE));
    assert!(e.should_run(Pass::Dupes));
    assert_eq!(e.gated_off().count(), 0);
    // ... unless the rule is ignored.
    c.rules.ignore = vec![sel("PM5")];
    let e = RuleEngine::new(&c);
    assert!(!e.should_run(Pass::Dupes));
    assert!(!e.selected().contains(codes::DUPLICATE_CODE));
}

#[test]
fn select_replaces_defaults() {
    let mut c = cfg();
    c.rules.select = Some(vec![sel("PM1")]);
    let e = RuleEngine::new(&c);
    // Explicit selection enables default-off rules too.
    assert_eq!(enabled_codes(&e), [101, 102, 103, 104, 105, 106, 107]);
    assert!(e.should_run(Pass::Hotspot));
    for p in [Pass::Coupling, Pass::Cycles, Pass::Layers, Pass::DeadCode, Pass::Dupes, Pass::Deps] {
        assert!(!e.should_run(p), "{p:?}");
    }

    c.rules.select = Some(vec![sel("ALL")]);
    let e = RuleEngine::new(&c);
    assert_eq!(e.enabled().len(), all().count() - 1, "all but the gated PM501");
    assert_eq!(e.gated_off().map(|d| d.code).collect::<Vec<_>>(), [501]);

    c.rules.select = Some(vec![]);
    let e = RuleEngine::new(&c);
    assert!(e.enabled().is_empty());
    assert!(Pass::ALL.iter().all(|&p| !e.should_run(p)));
}

#[test]
fn ignore_and_extend_select() {
    let mut c = cfg();
    c.rules.ignore = vec![sel("PM4"), sel("PM6")];
    c.rules.extend_select = vec![sel("PM106"), sel("PM603")];
    let e = RuleEngine::new(&c);
    assert!(e.is_enabled(codes::LOW_MAINTAINABILITY));
    // `PM603` is more specific than the `PM6` ignore.
    assert!(e.is_enabled(codes::PRIVATE_IMPORT));
    assert!(!e.is_enabled(codes::UNDECLARED_DEPENDENCY));
    assert!(e.should_run(Pass::Deps));
    assert!(!e.should_run(Pass::DeadCode));
    // Same specificity: ignore wins.
    c.rules.ignore.push(sel("PM106"));
    assert!(!RuleEngine::new(&c).is_enabled(codes::LOW_MAINTAINABILITY));
}

#[test]
fn engine_from_parsed_config() {
    let text = "min-confidence = 60\n\
                [rules]\nselect = [\"PM3\", \"PM101\"]\nignore = [\"PM304\"]\n\
                [rules.per-file-ignores]\n\"tests/**\" = [\"PM301\"]\n\
                [thresholds]\nmax-mccabe = 5\n";
    let c = config::parse_standalone(text, "pymapper.toml", Path::new("/p")).unwrap();
    let e = RuleEngine::new(&c);
    assert_eq!(enabled_codes(&e), [101, 301, 302, 303]);
    assert_eq!(e.min_confidence(), 60);
    assert!(e.violates(ThresholdKey::MaxMccabe, 6.0));
    assert!(e.should_run(Pass::Cycles) && e.should_run(Pass::Layers) && e.should_run(Pass::Hotspot));
    assert!(!e.should_run(Pass::DeadCode));
    assert_eq!(e.file_ignores(|_| true).iter().collect::<Vec<_>>(), [codes::IMPORT_CYCLE]);
    assert!(e.file_ignores(|_| false).is_empty());
}

#[test]
fn shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RuleEngine>();
    assert_send_sync::<Findings>();
    assert_send_sync::<Suppressions>();
    assert_send_sync::<&'static RuleDef>();
}

#[test]
fn min_confidence_is_clamped() {
    let c = cfg();
    let e = RuleEngine::from_parts(&c.rules, &c.analysis, &c.thresholds, 250);
    assert_eq!(e.min_confidence(), 100);
}

#[test]
fn file_ignores_only_consults_relevant_entries() {
    let mut c = cfg();
    c.rules.per_file_ignores = vec![
        PerFileIgnore { pattern: "a/**".into(), rules: vec![sel("PM4")] },
        // Ignores only rules that are disabled anyway: never asked.
        PerFileIgnore { pattern: "b/**".into(), rules: vec![sel("PM106")] },
        PerFileIgnore { pattern: "c/**".into(), rules: vec![] },
        PerFileIgnore { pattern: "d/**".into(), rules: vec![sel("PM101"), sel("PM102")] },
    ];
    let e = RuleEngine::new(&c);
    let asked = RefCell::new(Vec::new());
    let set = e.file_ignores(|i| {
        asked.borrow_mut().push(i);
        i != 0
    });
    assert_eq!(*asked.borrow(), [0, 3]);
    assert_eq!(set.iter().map(Rule::code).collect::<Vec<_>>(), [101, 102]);
}

// ----- suppressions -----

fn parse(line: u32, codes: Option<&[u8]>) -> (Vec<SuppressionEntry>, Vec<BadCode>) {
    let mut out = Vec::new();
    let mut bad = Vec::new();
    parse_codes(line, codes, &mut out, |b| bad.push(b));
    (out, bad)
}

fn entry(line: u32, s: &str) -> SuppressionEntry {
    SuppressionEntry { line, selector: sel(s) }
}

#[test]
fn parse_codes_lists() {
    assert_eq!(parse(3, None), (vec![entry(3, "ALL")], vec![]));
    assert_eq!(parse(3, Some(b"PM101")), (vec![entry(3, "PM101")], vec![]));
    assert_eq!(parse(4, Some(b"PM101, PM4,PM")), (vec![entry(4, "PM101"), entry(4, "PM4"), entry(4, "PM")], vec![]));
    assert_eq!(parse(5, Some(b" \tPM301 ,, ,")), (vec![entry(5, "PM301")], vec![]));
    assert_eq!(parse(6, Some(b"ALL")), (vec![entry(6, "ALL")], vec![]));
}

#[test]
fn parse_codes_reports_bad_codes() {
    let malformed = |start, len| BadCode { start, len, kind: BadCodeKind::Malformed };
    assert_eq!(parse(1, Some(b"E501")), (vec![], vec![malformed(0, 4)]));
    assert_eq!(parse(1, Some(b"PM101, pm102 , E501")).1, [malformed(7, 5), malformed(15, 4)]);
    assert_eq!(parse(1, Some(b"PM101, pm102 , E501")).0, [entry(1, "PM101")]);
    assert_eq!(parse(1, Some(b"PM1234")).1, [malformed(0, 6)]);
    assert_eq!(parse(1, Some(b"PM 101")).1, [malformed(0, 6)]);
    assert_eq!(parse(1, Some(b"P\xffM1")).1, [malformed(0, 4)]);
    assert_eq!(parse(1, Some(b"all")).1, [malformed(0, 3)]);
    // An empty list suppresses nothing and is reported.
    assert_eq!(parse(1, Some(b"")), (vec![], vec![malformed(0, 0)]));
    assert_eq!(parse(1, Some(b" , ")), (vec![], vec![malformed(0, 0)]));
    // Unknown but well-formed codes are reported and kept.
    let unknown = BadCode { start: 7, len: 5, kind: BadCodeKind::Unknown };
    assert_eq!(parse(2, Some(b"PM101, PM999")), (vec![entry(2, "PM101"), entry(2, "PM999")], vec![unknown]));
    assert_eq!(parse(2, Some(b"PM9")).1[0].kind, BadCodeKind::Unknown);
    assert_eq!(parse(2, Some(b"PM0")).1[0].kind, BadCodeKind::Unknown);
    assert!(parse(2, Some(b"PM1")).1.is_empty());
}

#[test]
fn suppressions_table_lookup() {
    let mut b = SuppressionsBuilder::new();
    b.push_file(FileId(2), &[entry(10, "PM101"), entry(3, "PM4"), entry(10, "PM301")]);
    b.push_file(FileId(3), &[]);
    b.push_file(FileId(7), &[entry(1, "ALL")]);
    let s = b.build();
    assert_eq!(s.len(), 4);
    assert_eq!(s.for_file(FileId(2)).iter().map(|e| e.line).collect::<Vec<_>>(), [3, 10, 10]);
    assert!(s.for_file(FileId(3)).is_empty());
    assert!(s.is_suppressed(FileId(2), 10, codes::COMPLEX_FUNCTION));
    assert!(s.is_suppressed(FileId(2), 10, codes::IMPORT_CYCLE));
    assert!(!s.is_suppressed(FileId(2), 10, codes::COGNITIVE_COMPLEXITY));
    assert!(s.is_suppressed(FileId(2), 3, codes::UNUSED_FUNCTION));
    assert!(s.is_suppressed(FileId(2), 3, codes::UNUSED_IMPORT));
    assert!(!s.is_suppressed(FileId(2), 4, codes::UNUSED_FUNCTION));
    assert!(!s.is_suppressed(FileId(2), 2, codes::UNUSED_FUNCTION));
    assert!(s.is_suppressed(FileId(7), 1, codes::DUPLICATE_CODE));
    assert!(!s.is_suppressed(FileId(7), 2, codes::DUPLICATE_CODE));
    assert!(!s.is_suppressed(FileId(1), 1, codes::DUPLICATE_CODE));
    assert!(!s.is_suppressed(FileId(8), 1, codes::DUPLICATE_CODE));
    assert!(!Suppressions::new().is_suppressed(FileId(0), 1, codes::COMPLEX_FUNCTION));
}

#[test]
fn suppressions_builder_regroups_out_of_order_files() {
    let mut b = SuppressionsBuilder::new();
    b.push_file(FileId(9), &[entry(5, "PM101")]);
    b.push_file(FileId(1), &[entry(8, "PM102")]);
    b.push_file(FileId(9), &[entry(2, "PM103")]);
    b.push_file(FileId(4), &[entry(1, "PM1")]);
    let s = b.build();
    assert_eq!(s.for_file(FileId(9)), &[entry(2, "PM103"), entry(5, "PM101")]);
    assert_eq!(s.for_file(FileId(1)), &[entry(8, "PM102")]);
    assert_eq!(s.for_file(FileId(4)), &[entry(1, "PM1")]);
    assert!(s.is_suppressed(FileId(9), 2, codes::DEEP_NESTING));
    assert!(s.is_suppressed(FileId(9), 5, codes::COMPLEX_FUNCTION));
    assert!(!s.is_suppressed(FileId(9), 5, codes::DEEP_NESTING));
    let mut ordered = SuppressionsBuilder::new();
    ordered.push_file(FileId(1), &[entry(8, "PM102")]);
    ordered.push_file(FileId(4), &[entry(1, "PM1")]);
    ordered.push_file(FileId(9), &[entry(5, "PM101"), entry(2, "PM103")]);
    assert_eq!(ordered.build(), s);
}

// ----- engine: filtering -----

#[test]
fn filter_applies_every_stage() {
    let mut c = cfg();
    c.min_confidence = 80;
    c.rules.ignore = vec![sel("PM102")];
    c.rules.per_file_ignores = vec![PerFileIgnore { pattern: "tests/**".into(), rules: vec![sel("PM4")] }];
    let e = RuleEngine::new(&c);
    let mut supp = SuppressionsBuilder::new();
    supp.push_file(FileId(1), &[entry(20, "PM405")]);
    let supp = supp.build();

    let sym = |i| Arg::Symbol(SymbolId(i));
    let mut f = Findings::new();
    push_dead(&mut f, 1, 10, 95, 1); // reported
    push_dead(&mut f, 1, 20, 95, 2); // suppressed inline
    push_dead(&mut f, 1, 30, 50, 3); // below confidence
    push_dead(&mut f, 2, 10, 95, 4); // file 2 is under tests/: file-ignored
    f.push(codes::COGNITIVE_COMPLEXITY, at(1, 5), SymbolId(9), 100, &[sym(9), Arg::U32(30), Arg::U32(15)]); // disabled
    f.push(codes::COMPLEX_FUNCTION, at(2, 5), SymbolId(8), 100, &[sym(8), Arg::U32(30), Arg::U32(10)]); // reported
    f.push(codes::UNUSED_DEPENDENCY, Location::new(FileId::NONE, 0, 0), SymbolId::NONE, 90, &[Arg::Name(6)]); // reported
    push_dead(&mut f, 2, 11, 95, 7); // file-ignored (cached mask)
    push_dead(&mut f, 1, 40, 80, 8); // exactly at the gate: reported

    let mut calls = Vec::new();
    let mut below = f.clone();
    let stats = e.filter(&mut f, &supp, false, |file, i| {
        calls.push((file, i));
        file == FileId(2)
    });
    assert_eq!(stats, FilterStats { reported: 4, below_confidence: 1, disabled: 1, file_ignored: 2, suppressed: 1 });
    assert_eq!(tags(&f), [1, 1008, 6, 8]);
    // One path match per (file, entry), only for files with per-file-ignored rules.
    assert_eq!(calls, [(FileId(1), 0), (FileId(2), 0)]);

    let stats = e.filter(&mut below, &supp, true, |file, _| file == FileId(2));
    assert_eq!(stats.below_confidence, 1);
    assert_eq!(tags(&below), [1, 3, 1008, 6, 8]);
}

#[test]
fn filter_without_per_file_ignores_never_matches_paths() {
    let e = RuleEngine::new(&cfg());
    let mut f = Findings::new();
    for i in 0..100 {
        push_dead(&mut f, i, 1, 90, i);
    }
    let stats = e.filter(&mut f, &Suppressions::new(), false, |_, _| panic!("no per-file-ignores configured"));
    assert_eq!(stats.reported, 100);
    assert_eq!(f.len(), 100);
}

#[test]
fn verdict_order() {
    let mut c = cfg();
    c.rules.per_file_ignores = vec![PerFileIgnore { pattern: "*".into(), rules: vec![sel("PM405")] }];
    let e = RuleEngine::new(&c);
    let mut supp = SuppressionsBuilder::new();
    supp.push_file(FileId(0), &[entry(1, "ALL")]);
    let supp = supp.build();
    let mut f = Findings::new();
    push_dead(&mut f, 0, 1, 10, 0);
    let x = f.as_slice()[0];
    let all_ignored = e.file_ignores(|_| true);
    assert_eq!(e.verdict(&x, &all_ignored, &supp), Verdict::FileIgnored);
    assert_eq!(e.verdict(&x, &RuleSet::EMPTY, &supp), Verdict::Suppressed);
    assert_eq!(e.verdict(&x, &RuleSet::EMPTY, &Suppressions::new()), Verdict::BelowConfidence);
    c.rules.ignore = vec![sel("PM4")];
    assert_eq!(RuleEngine::new(&c).verdict(&x, &all_ignored, &supp), Verdict::Disabled);
}

// ----- docs -----

#[test]
fn docs_up_to_date() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rules.md");
    let generated = render_markdown();
    if std::env::var_os("PYMAPPER_BLESS").is_some() {
        std::fs::write(&path, &generated).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        on_disk == generated,
        "docs/rules.md is stale; regenerate with `PYMAPPER_BLESS=1 cargo test -p pymapper-core rules::tests::docs_up_to_date`"
    );
}

#[test]
fn docs_cover_every_rule() {
    let md = render_markdown();
    for d in all() {
        assert!(md.contains(&format!("### PM{:03} {}", d.code, d.name)), "PM{}", d.code);
        assert!(md.contains(&format!("(#pm{:03}-{})", d.code, d.name)), "PM{} link", d.code);
        for k in d.thresholds {
            assert!(md.contains(&format!("`thresholds.{}`", k.key())), "{}", k.key());
        }
    }
    for &cat in Category::ALL {
        assert!(md.contains(&format!("\n## {}\n", cat.title())));
    }
    assert!(md.contains("needs `analysis.dupes`"));
}
