# pymapper issues and agreed work

This is the single source of truth for what is wrong with pymapper **0.3.0** (tag `v0.3.0`, `master` @ `0b54c1a`) and for the work the user has **agreed** for the next release, **0.4.0**.

**Read `HANDOFF.md` first** for context: goals, user preferences, process rules, environment gotchas and the history.

**Evidence.** The issues come from:
- two evaluation rounds and an 8-codebase "hunt" (pandas 3.0.5, sympy 1.14.0, scipy 1.18.0, torch 2.13.0 and four internal repositories), in which about 680 findings were verified by reading the code;
- a hands-on output-format review with the user on a seeded demo project.

**Where the evidence lives.** The verbatim agent reports are in `reports/0.3.0/` (gitignored, since they contain internal names; summary in `reports/0.3.0/SUMMARY.md`). This file deliberately uses only public-library examples. Internal cases are described generically. Keep it that way, because this file may be committed.

**Status legend:**
- **AGREED**: the user has approved the design; implement it as specified.
- **PROPOSED**: recommended, but not yet approved by the user; confirm before building.
- **OPEN**: needs a decision.

**Severity** is how much the issue hurts users at the default confidence gate (80):
- **high**: wrong at confidence 100, real problems hidden, or a workflow blocked;
- **medium**: noisy or misleading;
- **low**: polish.

## Overview

| ID | Area | Title | Severity | Status |
|---|---|---|---|---|
| [O1](#o1-human-friendly-default-output-category--file) | output | Human-friendly default output, grouped by category then file | high | AGREED |
| [O2](#o2---llm-mode-for-check-and-deps---check) | output | `--llm` mode for `check` / `deps --check`: compact prioritised report for agents | high | AGREED |
| [O3](#o3-pymapper-explain-id--symbol) | CLI | New `pymapper explain <id \| symbol>` drill-down | high | AGREED |
| [O4](#o4---llm-mode-for-every-command) | CLI | `--llm` mode for every command | high | AGREED |
| [O5](#o5-stable-finding-ids) | output | Stable finding ids (`PM401-3fa2`) | high | AGREED |
| [O6](#o6-paths-relative-to-the-current-directory) | output | Paths relative to the project root, not the cwd (bug) | high | AGREED |
| [O7](#o7-json-sarif-and-github-format-improvements) | output | JSON/SARIF/GitHub improvements (root, ids, endLine, fix, related locations, reasons) | medium | AGREED (JSON root/id/fix, SARIF+GitHub endLine), deferred (rest) |
| [O8](#o8-config-docs-and-skill-updates-for-the-new-output) | docs | Config keys, docs and agent skill for the new output | medium | AGREED |
| [F1](#f1-auto-mode-flips-libraries-to-application-mode) | dead code | Auto mode flips libraries to application on a stray `__main__` guard or script | high | PROPOSED |
| [F2](#f2-notebooks-are-not-scanned) | dead code | Jupyter notebooks not scanned | high | PROPOSED |
| [F3](#f3-functions-and-classes-used-as-values-are-not-references) | dead code | Functions/classes used as values are not references | high | PROPOSED |
| [F4](#f4-import-cycles-mix-eager-deferred-and-type-only-edges) | cycles | Import cycles mix eager/deferred/type-only edges; phantom extension cycles | high | PROPOSED |
| [F5](#f5-dynamic-loading-patterns-are-not-recognised) | dead code | Dynamic loading by name not recognised | high | PROPOSED |
| [F6](#f6-code-called-from-cc-extensions-reported-dead) | dead code | Code called from C/C++ extensions reported dead | high | PROPOSED |
| [F7](#f7-confidence-70-dynamic-access-penalty-too-broad) | dead code | Confidence-70 dynamic-access penalty too broad | high | PROPOSED |
| [F8](#f8-project-and-framework-conventions-not-understood) | dead code | Project and framework conventions not understood | medium–high | PROPOSED |
| [F9](#f9-output-metric-and-query-defects) | output/metrics | Nested complexity double counted, PM501 pairs twice, wording, `impact` noise, `@overload` | medium | PROPOSED (partly covered by O1–O3) |
| [F10](#f10-deps-defects) | deps | PM601 per site, PM604 never fires, impossible scorecard values | medium | PROPOSED |
| [F11](#f11-front-end-speed-and-memory-targets) | performance | Front end ~225 MB/s/core against a 400 target; 15.22 B/LOC against 15 | low | PROPOSED |
| [F12](#f12-internal-names-committed-to-the-repository) | repository | Internal names, findings and local paths committed (blocks pushing) | high | PROPOSED (must be done before any push) |
| [R1](#r1-repository-and-tooling-housekeeping) | repository | Housekeeping: stale `rewrite` worktree, `.gitignore`, demo project, build output | low | mostly DONE |

**Release decisions already made by the user:**
- The next version is **0.4.0**, because the default output changes.
- Finding-id format: `PM401-3fa2` (O5).
- The `--llm` cap defaults to 50 items, and the output must end with an "N more … not displayed" line (O2).
- Fix hints and verification commands are included in `--llm` (O2).
- Every command gets an `--llm` mode (O4).
- The human default is grouped **category > file** (O1).
- **Ids are always shown**, in human output too (no `--show-ids` flag) (O1, O5).
- O7 scope: JSON `root`, `id` and `fix`; SARIF and GitHub `endLine`. The rest of O7 is deferred.
- 0.4.0 scope is **O1–O8 only**; F1–F12 come later (F12 still before any push).
- Built by subagents in worktrees (A human, B llm + ids, C explain + queries) on top of a coordinator "Phase 0" commit; success = golden tests + the demo renders as specified + concise/json/sarif/github byte-identical to 0.3.0.

## 0.4.0 Phase 0: shared interfaces (done by the coordinator)

So the agents do not collide, these land first (one commit on `master`):

| Piece | Where | Notes |
|---|---|---|
| `OutputFormat::Human` (default) and `OutputFormat::Llm` | `config.rs` | `--llm` is a CLI shorthand for `--output-format llm` and conflicts with `--output-format` (exit 2). Modelling llm as a format keeps one dispatch point. |
| `GroupBy`, `ColorChoice` (+ `resolve(is_terminal)` honouring `NO_COLOR`), `limit` (default `DEFAULT_LIMIT` = 50) | `config.rs`, `config/parse.rs`, `config/write.rs`, template | config keys `group-by`, `color`, `limit`; flags `--group-by`, `--color`, `--limit` in `ConfigArgs` (help heading "Output") |
| **One shared `--limit`** | `cmd/config.rs`, `cmd/query/session.rs` | The queries already had a per-command `--limit` (rows, default 200), which clashed. Now: an explicit `--limit N` caps any format (0 = all); without it, `llm` uses the `limit` option (50), query text keeps 200 rows (`DEFAULT_ROWS`, `Session::row_limit`), human `check` is uncapped. |
| `Options { format, min_confidence, footer, group_by, color, limit }`, `Options::from_config(&config, is_terminal)` | `output.rs` | `color` is already resolved (human only) |
| `output/human.rs`, `output/llm.rs` | core | **placeholders that write concise**; agents A and B replace the bodies |
| Paths relative to the cwd (O6) | `output/context.rs` (`IndexContext::relative_to`, `push_relative`, `Context::write_root_path`), `deps/hygiene.rs`, `cmd/check.rs`, `cmd/check/plan.rs` (`Footnote.base`, `RelPath`) | text formats (human, llm, concise, github) use the cwd; json/sarif the root; baseline keys and ids always root-relative |
| Finding ids (O5) | `output/ids.rs`: `stable_hash` (FNV-1a 64, frozen), `FindingId` (Display `PM401-3fa2`), `FindingIds` (`Report::ids()`, `get(pos)`, `find(text)` accepting any unique ≥4-digit prefix), `FindError` | equal keys disambiguated by occurrence; per-finding extension on collision |
| Short symbols / plain messages | `Report::short_symbol`, `Report::message_plain` | for the human and llm writers |
| `pymapper explain <target> [path]` | `cmd/explain.rs` (stub, bails), `main.rs` | arguments final; agent C implements |
| Tracked demo | `tests/corpus/demo/` (copy of the seeded root `sample_python/`, without `expected.json`) + facts goldens | the untracked root `sample_python/` stays the user's playground |
| Goldens | `crates/pymapper-cli/tests/golden.rs`, `tests/golden/demo/{concise.txt, concise-dupes-all.txt, json.json, sarif.json, github.txt}` | produced by the **0.3.0 binary**; current build is byte-identical; also O6 path tests (repo root, project root, subdirectory, filter note, json, baselines) |
| E2E test helpers | `crates/pymapper-cli/tests/{check,coverage,deps_check,watch}.rs` | inject `--output-format concise` unless the test picks a format (`concise()` helper; `check_exact` bypasses it) |

---

# Part A: Output and UX redesign (AGREED for 0.4.0)

## Background: what the output looks like today

`pymapper check` defaults to `concise`: one `path:line:col: CODE message` line per finding, then a footer. The user ran `cargo run --release -- check ./sample_python` on the seeded demo project (see R1/HANDOFF) and called it "a huge wall of text, with no easy grouping visible, and I can't see the category easily". Today's output:

```
app.py:35:5: PM401 function `app:dead_endpoint` is never used
middleware/pipeline.py:6:25: PM405 `User` is imported but never used
...
services/pricing.py:6:5: PM101 `services.pricing:calculate_price` is too complex (McCabe 14 > 10)
services/pricing.py:6:5: PM102 `services.pricing:calculate_price` is hard to understand (cognitive complexity 32 > 15)
services/pricing.py:6:5: PM103 `services.pricing:calculate_price` nests blocks 5 levels deep (> 4)
services/pricing.py:6:5: PM105 `services.pricing:calculate_price` takes 8 parameters (> 7)
...
Found 26 findings in 15 files (23 files checked).
Not shown: 10 below min-confidence.
```

Problems identified with the user:
- no grouping;
- the category is invisible (only the code shows);
- one function produces 4 near-identical lines;
- the qualified symbol repeats the module even though the path already shows it;
- no severity, colour or per-category tally;
- paths are relative to the project root, not the cwd (O6);
- duplicate pairs are printed twice;
- cross-file findings (cycles, contracts) are buried.

Today's formats and where they live:
- **Writers:** `crates/pymapper-core/src/output/`, with `concise.rs`, `json.rs` (schema version 2), `sarif.rs` (SARIF 2.1.0), `github.rs` (Actions annotations), `baseline.rs`, `context.rs` (display paths, symbol ids), `buf.rs` (a 64 KiB buffered writer that stops quietly on EPIPE).
- **Report:** `output.rs` holds `Report`. It renders each path once, sorts findings by (path, line, col, rule, symbol, confidence), applies baselines and computes the exit status.
- **Format choice:** the CLI picks the format in `crates/pymapper-cli/src/cmd/check.rs` (around line 250–280) from `config.output_format` (`config.rs`, enum `OutputFormat { Concise, Json, Sarif, Github }`). `deps` (`cmd/deps.rs`) supports only concise/json for its listing.
- **Finding model** (`rules/finding.rs`): rule, location (file, line, col), `end_line`, symbol, confidence (0–100), `reasons` (u32 bitset, names via `reason_names`), message args, and extra data args such as `value`/`threshold` (metrics), `cycle` members (PM301) and `other` location (PM501). Messages are templates rendered at output time.
- **Categories** (`rules.rs` `Category`): Complexity (PM1), Coupling (PM2), Layering (PM3), DeadCode (PM4), Duplication (PM5), Dependencies (PM6).

## O1 Human-friendly default output (category > file)

**Status:** AGREED · **Severity:** high

**What:** a new default format, `human`, grouped by **category, then file**. The user first picked file > findings, asked why the category grouping had been recommended, and then chose category > file. The reasoning that persuaded them:
- categories mean different kinds of work (delete, refactor, re-architect);
- cross-file findings appear once;
- rare but important architecture issues are not buried;
- category headers with counts summarise large runs.

**Target layout** (no colour shown):

```
Architecture  2
  models/order.py
     2:29  imports forbidden repo.order_repo (contract: models are independent of repo)   PM303
  repo/order_repo.py
     1:26  import cycle (2 modules): repo.order_repo -> models.order -> repo.order_repo     PM301

Complexity  1 function
  services/pricing.py
     6:5   calculate_price  McCabe 14>10 · cognitive 32>15 · nesting 5>4 · params 8>7     PM101 PM102 PM103 PM105

Duplication  1 pair
  services/pricing.py:39  receipt_lines  ↔  services/invoice.py:1  invoice_lines  (12 lines)  PM501

Dead code  16
  app.py
    35:5   function  dead_endpoint            never used                                 PM401
  models/order.py
    15:9   method    Order.archive            never used                                 PM403
  services/pricing.py
    53:5   function  legacy_discount          never used                                 PM401
    57:7   class     PromoEngine              never used                                 PM402
  utils/formatting.py
     5:5   function  format_currency          only used by unused code                   PM401
  utils/legacy_helpers.py
     1:1   module    utils.legacy_helpers     never imported                             PM404
  ...

Unused imports  2
  middleware/pipeline.py
     6:25  User                                                                          PM405
  services/pricing.py
     1:8   json                                                                          PM405

26 findings in 15 files · architecture 2 · complexity 1 function · duplication 1 pair · dead code 16 · unused imports 2
10 more below confidence 80 (use --min-confidence 0 to show them)
```

**Rules:**
1. **Section order is by priority, not by code:**
   1. Architecture: import cycles PM301 and contracts PM302–304.
   2. Dependencies: PM6xx, shown under `deps --check`.
   3. Complexity: PM1xx.
   4. Coupling: PM2xx.
   5. Duplication: PM5xx.
   6. Dead code: PM401–404.
   7. Unused imports: PM405, its own section.
2. **Section header:** the category name plus a count. For complexity the count is functions, and for duplication it is pairs.
3. **Sorting:** files alphabetically within a section (the user agreed); findings by line within a file.
4. **Merging:** metric findings PM101–PM107 on the same symbol merge into **one line**, built from the structured `value`/`threshold` args (`McCabe 14>10 · cognitive 32>15 · nesting 5>4 · params 8>7`), listing every code at the end.
5. **Cross-file findings appear once:**
   - an import cycle goes under the file holding the import that closes it;
   - a duplicate pair is one item showing both locations (this also fixes "PM501 pairs reported twice", F9);
   - a contract break goes under the importing file.
6. **Short symbols** (the file is already the heading): `calculate_price`, `Order.archive`. No backticks in human messages. Dead code shows a kind column (function/class/method/module) and a short verdict ("never used", "only used by unused code", "has no resolved callers (name appears elsewhere)", ...).
7. **Low confidence:** findings below 100 end with `(confidence 40: unknown-decorator)`, dimmed on a TTY.
8. **Ids** (AGREED: always shown): every item ends with its finding id (`PM401-3fa2`) instead of the bare code, right-aligned when the terminal width allows, otherwise two spaces after the message. **Never truncate.** A merged metric line shows the first finding's id and the other codes (`PM101-0b2e +PM102,PM103,PM105`); `explain` on that id covers every metric of the symbol. A duplicate pair or cycle shows its one id. (The target layout below predates this decision and shows bare codes.)
9. **Footer:** a per-category tally, plus the existing notes (below confidence, suppressed, baselined, per-file-ignored, "Filtered to <paths> … the whole project was analysed", the rules-ran/skipped line).
10. **Colour** (AGREED: "yes, when a terminal"):
    - category headers coloured by family: architecture/cycles red, dependencies cyan, complexity magenta, coupling magenta-dim, duplication blue, dead code yellow, unused imports yellow-dim;
    - file paths bold; line:col and codes dim; low-confidence suffix dim.
    - `--color auto|always|never` (default `auto` = only when stdout is a TTY), honour `NO_COLOR`, no colour under `--llm`.
    - Use plain ANSI escapes and `std::io::IsTerminal`. **No new dependencies** (dependency policy in `docs/design.md`).
11. **`--group-by file`** (config `group-by = "category" | "file"`, default `category`): the file-first layout for editing sessions and changed-file CI, with the same line format and footer:

    ```
    services/pricing.py
        1:8   unused import   json                                         PM405
        6:5   complexity      calculate_price  McCabe 14>10 · cognitive 32>15 · nesting 5>4 · params 8>7   PM101 PM102 PM103 PM105
       53:5   dead code       function legacy_discount  never used         PM401
    ```
12. **Uncapped:** the human output is not capped (people scroll; `--limit` exists, see O2).
13. **Exit codes unchanged:** 0 clean, 1 findings at or above the gate, 2 usage/fatal.

**Implementation notes:**
- New writer `output/human.rs`.
- `OutputFormat::Human` becomes the default in `config.rs`; `Concise` stays valid.
- Add a render mode for short symbols and plain (no-backtick) messages, through the message-rendering path (`Report::message` / `Findings::write_message` / `Context`).
- Grouping and merging work on the already sorted `Report` index, with no per-finding allocation (performance rules apply to output too: reuse the scratch buffers as `concise.rs` does).

**Acceptance:**
- golden tests for both groupings, with and without colour;
- metric merging;
- a cycle appears once and a duplicate pair once;
- section order;
- footer tally;
- `NO_COLOR` and `--color`;
- piped output has no escapes;
- the demo `sample_python/` renders as above.

## O2 `--llm` mode for `check` and `deps --check`

**Status:** AGREED · **Severity:** high

**Why:** the concise format suits line tools, not LLMs:
- it repeats the full path and qualified symbol on every line;
- it has no prioritisation and no cap, so it floods the context window;
- it gives no evidence or next step, so the hunt agents spent many tool calls per finding re-deriving why something was flagged;
- related findings are split up.

The user asked to "have a deeper think" and agreed to the design below. They said: "we are free to structure the output completely for LLM reads".

**Principles:**
1. **Token efficiency:** paths and sections once, short symbols, no alignment padding.
2. **Prioritised and capped:** the most important first. Default cap of **50 items** (AGREED). `--limit N`, `--limit 0` = all. The output **must end** with a line like `N more findings not displayed (in M files). Use --limit 0 to show all, or --select <code> to narrow.` (AGREED wording, approximately "N more messages not displayed").
3. **Evidence, not just verdicts:**
   - confidence and reasons on **every** scored item;
   - metric values;
   - the edge kinds of cycles (once F4 lands);
   - the blind spots for the command, stated once in the header.
4. **A next step on every item** (AGREED, include them):
   - a one-line **fix hint** from per-rule templates filled with real facts, never invented advice;
   - a **verification command** (`pymapper callers X`, `pymapper impact X`, a `grep -rnw` line).
5. **Merge related findings:**
   - metrics per function;
   - **dead chains** as one decision ("root X; also dead if X goes: A, B, C"), using the existing dead-code origin data (`only-used-by-unused-code`);
   - cycles, contracts and duplicate pairs as single items.
6. **Stable ids** on every item (O5), used by `explain` (O3).
7. **Header facts:**
   - command, root (absolute and as given);
   - dead-code mode and why (per unit when there are several);
   - counts (shown / total / below the gate / suppressed);
   - **what the exit code means** ("exit 1 = findings, not an error");
   - skipped rules and why;
   - the suppression syntax (once).
8. **No colour; deterministic; Markdown-like.** It is the structure LLMs parse most reliably and cheaper than JSON. JSON stays the strict machine contract for scripts.

**Target output** (illustrative; exact wording is up to the implementer, the content is required):

```
# pymapper check: sample_python (root: ./sample_python, mode: application via [project.scripts])
26 findings · showing 12 most important · 10 more below confidence 80 · exit 1 = findings, not an error
Not analysed: notebooks, C extensions, names used only via getattr/strings. Verify dead code before deleting.

## Architecture (2)
PM301-a1f3 import cycle, 2 modules, eager: repo.order_repo -> models.order -> repo.order_repo
   at repo/order_repo.py:1 (top-level import), models/order.py:2 (top-level import)
   fix: break one edge, e.g. import inside the function that needs it · verify: pymapper explain PM301-a1f3
PM303-77c0 contract "models are independent of repo": models.order imports repo.order_repo
   at models/order.py:2 · fix: move next_order_id out of repo or invert the dependency

## Complexity (1 function)
PM101-0b2e services/pricing.py:6-36 calculate_price: mccabe 14>10, cognitive 32>15, nesting 5>4, params 8>7
   fix: split by branch (customer tier, coupon, shipping) · verify: pymapper callers services.pricing:calculate_price

## Duplication (1 pair)
PM501-c912 12 identical lines: services/pricing.py:39-50 receipt_lines = services/invoice.py:1-12 invoice_lines
   fix: keep one, call it from the other

## Dead code (16 items, 18 symbols)
PM401-3fa2 services/pricing.py: legacy_discount:53 (function), PromoEngine:57 (class): no references (conf 100)
PM404-e01d utils/legacy_helpers.py: whole module never imported (conf 100)
PM401-5b6a utils/formatting.py: format_percentage:9 no references; format_currency:5 only used by dead code (chain root: utils.formatting:format_percentage)
...

## Unused imports (2)
PM405-19aa services/pricing.py:1 json · PM405-d4e2 middleware/pipeline.py:6 User

14 more findings not displayed (in 9 files). Use --limit 0 to show all, or --select <code> to narrow.
Next: `pymapper explain <id>` for evidence and source · suppress with `# pymapper: ignore[PMxxx]` on the reported line
```

**Priority order** (inside the cap): the section order of O1. Within a section:
- higher severity first, then higher confidence;
- complexity by the worst metric overshoot;
- dead code: chains and modules before single symbols.

The cap counts **items** (merged groups), not raw findings.

**CLI:**
- `--llm` on `check` and `deps --check`.
- `--output-format concise` keeps today's exact one-line output for scripts (byte-for-byte identical to 0.3.0's concise).
- `--llm` and `--output-format json/sarif/github` are mutually exclusive (usage error, exit 2).
- **Exit codes unchanged.**

**Acceptance:**
- golden `--llm` output on `sample_python/`;
- cap and trailer line;
- `--limit 0`;
- chain merging;
- ids stable across two runs;
- fix hints present for every rule family;
- the header states the mode, the exit code meaning and the blind spots;
- concise output unchanged (golden comparison with 0.3.0 behaviour).

## O3 `pymapper explain <id | symbol>`

**Status:** AGREED · **Severity:** high

**Purpose:** the drill-down an agent runs **before acting** on an item. It shows everything pymapper knows about one finding, so the agent can verify and fix it without rebuilding context through many grep and read calls.

**Behaviour:**
- **Input:** a finding id from O5 (`PM401-3fa2`), or a symbol (`services.pricing:legacy_discount`, with dotted-name and last-segment matching as in `callers`; ambiguous input lists the candidates and exits 2).
- **Stateless:** it re-runs the analysis (warm cache, about 0.1 s on a mid-size repo), finds the finding whose id matches, and renders the detail. Ids stay valid while the code does not change.
- **For a symbol:** every finding on that symbol, plus a connectivity summary (callers/callees counts, importers).
- **Output modes:** both a human mode and `--llm`. Paths relative to the cwd (O6).

**Content by family** (facts from the analysis; fix text from per-rule templates filled with facts):

| Family | `explain` output |
|---|---|
| **Dead code** (PM401–404) | The definition with its first few source lines (re-read from disk on demand; sources are not retained, per the design) and its kind. **The full evidence behind the confidence:** call sites found (none / only `possible` / only from dead code, with locations), places the name appears as text, string or config, dynamic-access patterns that lowered the score (with line), decorators and bases considered, which roots were checked (mode and why, entry points). **Effect of deleting it:** what else becomes dead (the chain) and which imports become unused. **Blind spots for this repo:** e.g. "repo contains 12 notebooks; the package ships .so files: not scanned". **A ready verification command** (`grep -rnw legacy_discount . --exclude-dir=.venv`) and the suppression line. |
| **Complexity** (PM1xx) | Every metric against its threshold. **Where the complexity comes from:** the branches and loops with their lines and nesting cost, ranked; the deepest nesting path; length excluding docstrings. **Split points:** the largest self-contained nested blocks. **How risky a refactor is:** number of callers and the `impact` size. |
| **Import cycle** (PM301) | All members. **Every edge with its file:line and kind** (top-level / inside a function / `TYPE_CHECKING` / try; see F4). Whether it is a real runtime cycle. **The cheapest edge to break:** a top-level import whose names are used only inside functions, or one that could import from a sibling module. |
| **Contract** (PM302–304) | The contract as configured, the offending import line and the names it imports, and other modules that import the same target legitimately. |
| **Duplication** (PM501) | Both spans with their source, and whether they are identical or differ only in names and literals (listing the differences). |
| **Unused import** (PM405) | The import line, re-export / `__all__` status, and appearances in strings, annotations or `cast()`. |
| **Dependencies** (PM6xx) | The distribution, every import site, the declaring groups (or none), and the import-name → distribution mapping used. |

**Acceptance:** for each family, a fixture in which `explain` shows the listed items. An id round-trips from `check --llm` to `explain`. Symbol input works. An ambiguous query exits 2.

## O4 `--llm` mode for every command

**Status:** AGREED ("yes everything should have an LLM mode") · **Severity:** high

**Commands:** `check`, `deps` (listing, `--score`, `--check`), `callers`, `callees`, `impact`, `path`, `imports`, `cycles` (and `--calls`), `summary`, `config`, `explain`.

**Shared conventions:**
- a header with the command, target symbol (resolved id, kind, location), root and counts;
- the blind spots relevant to that command, e.g. `callers` notes that callbacks passed as values, getattr dispatch and C callers are not edges (until F3/F5/F6 land);
- grouped results with paths and symbols once, and confidence tags;
- the cap and trailer line;
- a next-step line;
- no colour, deterministic output.

**Specific improvements to fold in:**
- `impact`: show the affected symbols first. Collapse "importing modules" to a count and the top N, instead of 200 rows of unrelated transitive importers (F9).
- `callers` / `callees`: group by caller module; show how many calls are `possible`.
- `summary`: the "most called" list must not count `possible` edges as real (F9).
- `deps --score`: a compact table with grade, `api used` and `loc reached`; fix the impossible values (F10).
- `cycles`: edge kinds and eager-only status (F4).
- `config`: the effective config as TOML with its source file.

**Acceptance:** an `--llm` golden test per command on `sample_python/` or `tests/corpus`, plus header facts, cap and trailer.

## O5 Stable finding ids

**Status:** AGREED · **Severity:** high

- **Format:** `PM401-3fa2`, i.e. the rule code, `-`, then 4 hex characters, automatically extended to 5–6 when two findings in the same run would collide.
- **Derivation:** from the **baseline key** (`output/baseline.rs`: rule + stable id (the symbol's qualified id, else the file path) + stable message arguments, never numbers). Use a fixed, toolchain-independent hasher. The std `DefaultHasher` is not stable across Rust releases, so use Fx/FNV/xxhash-style code written in-house.
- **Shown in:** human output always (AGREED: no `--show-ids` flag); `--llm` always; JSON as a new `"id"` field; SARIF in `partialFingerprints` (already present as `pymapperKey/v1`; keep it consistent).
- **Used by:** `explain`, and later possibly `--ignore-id`.

**Acceptance:**
- ids identical across runs and across cache on/off;
- the collision-extension test;
- identical in `--llm` and JSON;
- unaffected by line moves when the symbol is unchanged (the key has no numbers).

## O6 Paths relative to the current directory

**Status:** AGREED · **Severity:** high (bug)

- **Bug:** paths are rendered relative to the **project root** (`output/context.rs` `display_path`). Run from the repo root as `pymapper check ./sample_python`, the output shows `app.py:35:5` instead of `sample_python/app.py:35:5`, so the paths are not clickable or usable from the terminal the user is in.
- **Fix:**
  - **human, `--llm`, concise and github:** paths relative to the **cwd**, like ruff. Files outside the cwd show `../…`.
  - **JSON and SARIF:** keep paths relative to the project root (documented) and add a top-level JSON `"root"` (absolute path). SARIF already uses `%SRCROOT%`; set `originalUriBaseIds` to the root.
  - The `Filtered to …` footer must use the same convention.
- **Acceptance:** CLI tests run from the repo root, from the project root and from a subdirectory, for every format.

## O7 JSON, SARIF and GitHub format improvements

**Status:** JSON `"root"` + `"id"`: AGREED (part of O5/O6). The rest: PROPOSED.

- **JSON (additive, stays version 2):**
  - `root`, and `id` per finding;
  - PROPOSED: `url` per finding, linking to the rule's documentation anchor;
  - PROPOSED: `fix` (the same template as `--llm`);
  - PROPOSED: per-rule counts in `summary`.
- **AGREED for 0.4.0:** JSON `fix` per finding; SARIF `region.endLine`; GitHub `endLine`. The rest of this section is deferred.
- **SARIF (deferred except `endLine`):**
  - `region.endLine` (JSON already has `end_line`);
  - the second PM501 copy and cycle members as `relatedLocations`;
  - dead-code `reasons` in `properties`;
  - PM501 reported once per pair.
- **GitHub (PROPOSED):** `endLine` in annotations so the whole function is highlighted. The footer lines are already printed to stdout and are harmless.

## O8 Config, docs and skill updates for the new output

**Status:** AGREED · **Severity:** medium

- **Config** (`config.rs`, `config/parse.rs`, `config/write.rs`, `pymapper.toml.template`; strict validation with suggestions, as for every key):
  - `output-format = "human"` (new default; values: human, concise, json, sarif, github);
  - `group-by = "category" | "file"`;
  - `color = "auto" | "always" | "never"`;
  - `limit = 50`, applying to `--llm` only by default.
  - Document every key in the template with its default and cost note.
- **CLI flags:** `--llm`, `--group-by`, `--color`, `--limit` (done in Phase 0, on every command through `ConfigArgs`; `--limit` also replaces the queries' old row limit). Shared flags live in `crates/pymapper-cli/src/cmd/config.rs` (`ConfigArgs`).
- **Docs:**
  - `README.md` (usage, formats);
  - `docs/cli.md` (output formats section, exit codes, `explain`);
  - `docs/usage.md` (written in 0.3.0: update every example output, explain human vs `--llm` vs JSON, document `explain`);
  - `docs/rules.md` (generated from `rules/docs.rs`; add the fix-hint text per rule).
- **Skill:** update `~/.claude/skills/pymapper/SKILL.md` (a symlink to `~/.agents/skills/pymapper/SKILL.md`; its snapshot is in `reports/0.3.0/SKILL.md`) so agents:
  - use `--llm` for reading and `--output-format json` for scripting;
  - use `explain <id>` before acting;
  - follow the stated blind spots.

  The verification protocol in the skill stays.
- **Version:** bump the workspace to **0.4.0** (`Cargo.toml` `[workspace.package] version`; all crates use `version.workspace = true`).

---

# Part B: Precision issues from the field evaluations (PROPOSED)

Precision snapshot for 0.3.0 (verified samples; full tables in `reports/0.3.0/SUMMARY.md`):
- **Metrics (PM1xx/PM2xx):** the values are exact. PM101 matched ruff C901 on every finding compared (285/285 and 379/379).
- **Dead code at ≥ 80:**
  - 96–100% on a well-structured application monorepo;
  - 76% and 45% on two data-science repos (notebooks, F2);
  - 4/4 on pandas;
  - 80–94% on sympy in library mode, but 11% for PM403 in the application mode it picked (F1);
  - about 12% on scipy (F5: 123 of 142 PM401 from one pattern);
  - 46% on torch (F1, F6).
- **Dead code below 80:** 0–27% true, so the confidence ordering works. **Except at confidence 70, which was often true** (F7).
- **Cycles:** the SCCs are real except phantom extension cycles (F4). Most are closed by deliberate deferred imports, and the witnesses are misleading.

## F1 Auto mode flips libraries to application mode

**Severity:** high · **Rules:** PM401–404 · **Code:** `reach/` (`FileModes`, `plausible_entry`, `ModeReason`), `roots/units.rs` (project units, `app_scripts`)

- **Problem:** `mode = "auto"` decides per unit (each directory with a `pyproject.toml`). A unit becomes an application on any of:
  - a `[project.scripts]` / `gui-scripts` entry;
  - a `__main__.py`;
  - a `__main__` guard in a file not classified as test, private, script, example, docs or benchmark.

  In application mode only entry points are roots, so the public API of a library is reported dead at confidence 100.
- **Evidence:**
  - sympy → application because of a demo guard in `sympy/plotting/pygletplot/managed_window.py:105`. That produces 183 dead-code findings at the gate, against 36 in `--mode library`. The extra 147 are public API, lark transformer callbacks and pyglet handlers.
  - torch → application because of `torch/backends/xeon/run_cpu.py`. 172 extra findings (PM403 94, PM404 58, PM401 11, PM402 9).
  - An internal Hydra-driven library with one `[project.scripts]` entry: 152 dead-code candidates in application mode, 3 in library mode.
  - Internal shared libraries flipped by incidental guards in helper modules (`config_initialisation.py`, `client.py`).
  - A real CLI script's guard was wrongly ignored as a "helper file" (the opposite error).
- **Proposed fix:**
  - **Application evidence only:** `[project.scripts]`/`gui-scripts` of the unit, a top-level package `__main__.py`, or an entry from F8 (Dockerfile `CMD`, `uvicorn module:app`, `python -m`).
  - **A lone guard in a non-entry module never decides.**
  - **Prefer library** when the unit's root `__init__` has a large `__all__`, or when there are no scripts.
  - **Consider a "both" mode:** entry points plus public names.
  - **`--stats`:** list every deciding and ignored guard (not just the first).
- **Acceptance:**
  - sympy and torch copies resolve to library mode;
  - fixtures: a library with a demo guard → library; an app with scripts → application; a package `__main__.py` → application.

## F2 Notebooks are not scanned

**Severity:** high · **Rules:** PM401–404 · **Code:** `walk/` (file collection), `extract/config.rs` (config evidence, as a model), dead-code evidence (`analysis/deadcode/evidence.rs`)

- **Problem:** `.ipynb` files are ignored, so code used only from notebooks is reported as `no-references` at confidence 100.
- **Evidence:** internal data-science repos:
  - 12 of 14 false positives at ≥ 80 in one repo;
  - 10 in another;
  - a plotting module called 24 times from a notebook was reported dead;
  - a whole module imported only by a demo notebook got PM404.
- **Proposed fix:**
  - read notebook code cells (JSON `cells[].source` where `cell_type == "code"`) as evidence (identifiers, attributes, dotted strings) **and as importers** (`import` / `from … import` lines, resolved like Python imports);
  - cache like config evidence;
  - skip outputs; cap the size;
  - add the reason `notebook-reference`.
- **Side benefit:** imports of deleted modules from notebooks could be reported (one real broken notebook was found by hand in the hunt).
- **Acceptance:** a fixture notebook importing and calling a module function produces no PM401/PM404 at ≥ 50.

## F3 Functions and classes used as values are not references

**Severity:** high (cascades through "only used by unused code") · **Rules:** PM401–403, PM405 · **Code:** extractor events (`extract/`), `resolve/` (calls), dead-code propagation (`analysis/deadcode/prop.rs`, `seeds.rs`)

- **Problem:** only calls (and decorators, imports, bases) create edges. Value uses count as nothing. These include:
  - passed as an argument;
  - stored in a dict, list, tuple or set;
  - returned;
  - used as a default argument;
  - assigned to a variable or attribute;
  - used in `sort(key=…)`, `map(fn, …)`, `run_in_executor(…, fn)`, `__reduce__`, `monkeypatch.setattr`, or a pytest `params=[fn]`;
  - bound methods stored in tables.
- **Evidence:**
  - **pandas:** 486 of 663 PM401 findings are nested functions (returned decorator closures). Examples:
    - `missing._pad_1d` in a dispatch dict;
    - `_version.run_command` as `runner=run_command`;
    - `_new_IntervalIndex` via `__reduce__`;
    - `fmt_klass = _IntArrayFormatter`;
    - the excel `_engines` dict;
    - `SAS7BDATReader._process_subheader_counts` in a table.
  - **sympy:** `_laplace_rule_exp` in a rules list; `CCompilerRunner` returned in tuples.
  - **torch:** `_flex_attention_hop_wrapper`.
  - **Internal repos:** `launch_job(run, …)` made a whole training pipeline look dead; `HTTPServer((...), Handler)`.
- **Related gaps:**
  - property and `cached_property` **reads** are not uses of the property (pandas `self._combined`, `self._ilevel`);
  - **annotation-only** uses (including string annotations and `Unpack[TypedDict]`) are not uses for PM402/PM405;
  - a local holding two types (`x = A(); …; x = B()`) degrades calls on it to `possible` (pandas `infer_freq` / `_FrequencyInferer.get_freq`).
- **Proposed fix:**
  - record **name and attribute loads** of first-party functions and classes as reference edges (confidence just below `certain`);
  - property reads count as uses of the property;
  - annotation references count for PM402/PM405;
  - a closure returned from a live factory inherits the factory's liveness.
  - Keep it cheap: the extractor already sees every name token (performance rules).
- **Acceptance:** one fixture per pattern above; no dead-code finding at ≥ 50; `impact` on a callback-only function is no longer "0 affected".

## F4 Import cycles mix eager, deferred and type-only edges

**Severity:** high · **Rules:** PM301, `pymapper cycles` · **Code:** `analysis/cycles.rs`, `graph/`, `resolve/` (import flags already exist: type-only, conditional, try, lazy)

- **Problems:**
  1. **SCCs are computed over all import edges.** Deliberate in-function imports (the standard way to break a cycle) and `if TYPE_CHECKING:` imports close cycles that never exist at runtime.
  2. **The printed witness often uses such an edge.**
  3. **The size overstates the runtime cycle.**
  4. **`from . import <compiled extension>` resolves to the parent package,** creating phantom cycles.
  5. **An import of a missing module resolves to its parent package** instead of being unresolved.
  6. **The witness can be shorter than the cycle** ("3 modules" with 2 shown), and `check` doesn't show the other members.
- **Evidence:**
  - **pandas:** witness `core/dtypes/dtypes.py:94` is inside `if TYPE_CHECKING:` (from line 76). The SCC size is 227, but the eager-only SCC is about 148. A real eager witness exists: `core/dtypes/cast.py:90` → `io/_util.py:25` (`import pandas as pd`) → `pandas` → … → `cast`.
  - **sympy:** the 515-module SCC witness is a deferred import (`core/function.py:2927`); the real eager back-edge is `solvers/polysys.py:10` (`from sympy import Dummy`).
  - **scipy:** 7 of 16 cycles are phantom (`cluster/hierarchy/_hierarchy_impl.py:43` `from . import _hierarchy`, a `.so`; `ndimage/_filters.py:41`). Proven by stubbing all 106 `.so` files: 16 cycles became 9, and the largest shrank from 121 to 86.
  - **An internal monorepo:** 10 reported cycles, 0 eager. One cycle went through a `try: from x.cache import …` of a module that does not exist.
- **Proposed fix:**
  - classify each import edge as eager / deferred (function scope) / type-only / try-conditional;
  - compute and report the **eager SCC**;
  - tag cycles that exist only through deferred or type-only edges, with lower severity or a separate rule (OPEN decision);
  - choose an eager witness when one exists;
  - `cycles --eager`;
  - resolve `from . import X` to a sibling extension module (`.so`/`.pyd`), marked "not loaded";
  - missing modules are unresolved;
  - add edge kinds to the JSON `cycle` object;
  - human and `--llm` output show the cycle size, the members count and the witness edges with kinds.
- **Acceptance:**
  - the pandas witness is eager;
  - the scipy phantom cycles disappear;
  - a deferred-only cycle fixture is tagged;
  - a missing-module fixture is unresolved.

## F5 Dynamic loading patterns are not recognised

**Severity:** high · **Rules:** PM401–404 · **Code:** `extract/facts/build.rs` (getattr shape classification exists: constant/prefix/variable), dead-code evidence

| Pattern | Evidence |
|---|---|
| `importlib.import_module("literal")`, `__import__("literal")` | pandas `plotting/_core.py:2181` loads `pandas.plotting._matplotlib`: about 100 false findings. sympy loads `_parse_latex_antlr` the same way. |
| Import loops over `os.listdir` / `glob` / `pkgutil.walk_packages` | torch `_export/db/examples/__init__.py:19-26` (26 PM404), `autoheuristic/artifacts`, inductor `import_submodule` (`lowering.py:9111`) |
| `getattr(module, name + "_suffix")` | scipy `ndimage/_support_alternative_backends.py:112` and `signal/...:377`: 123 of 142 default PM401 (`*_signature` in `_delegators.py`) |
| A variable built from a constant prefix, then `getattr` | sympy `polys/domains/domain.py:389` (`method = "from_" + base.alias; getattr(self, method)`) |
| `getattr(module_alias, f"_{op}")` across modules | torch `distributed/_functional_collectives.py:1660` → `_functional_collectives_impl.py` (11 at 100) |
| `module.__dict__[name]` | an internal `vits.__dict__[args.arch](...)` |
| PEP 562 module `__getattr__` serving `__all__` | an internal lazy re-export: `callers` returned 0 for a class instantiated by a CLI |
| Dotted `from a.b import c` inside generated source strings | torch inductor runtime helpers (`runtime/halide_helpers.py`, `proton_utils.py`, `cuda/nvtx.py:71` in an f-string) |
| Frame introspection (`inspect.currentframe().f_back.f_locals.get(name)`) | **sympy `geometry/entity.py:35-36`: imports reported unused (PM405), but removing them silently breaks multiple-dispatch registration (proven at runtime).** Dangerous. |

- **Proposed fix:**
  - recognise each pattern in the extractor as reference evidence, with the narrowest scope (the module named, the prefix/suffix, the receiver's module);
  - treat `__all__` members served by a module `__getattr__` as re-exports;
  - modules that use frame introspection on names should lower PM405 confidence for names re-bound in the same module.
- **Acceptance:** a fixture per pattern; the scipy delegators and the pandas matplotlib backend no longer appear at ≥ 80; the sympy entity case is not reported at ≥ 80.

## F6 Code called from C/C++ extensions reported dead

**Severity:** high · **Rules:** PM401–404

- **Problem:** Python called by name from C/C++, or imported by compiled extensions, has no visible callers.
- **Evidence:**
  - **torch:** 14 findings at confidence 100 whose names appear in `libtorch_python` / `_C` strings, for example:
    - `torch._inductor.aoti_eager`;
    - `_library/simple_registry.py:142 find_torch_dispatch_rule`;
    - `_utils_internal.py:65`;
    - `autograd/function.py:327/335 BackwardCFunction.apply_boxed/apply_jvp`;
    - `distributed/rpc/internal.py:198 _run_function`;
    - `jit/_ir_utils.py:29`, bound in `_C/__init__.pyi:958`;
    - `distributed/tensor/_dispatch.py:489`.

    A shipped `.h` loads `runtime/triton_lazy_compile.py` with `PyImport_ImportModule`.
  - **pandas:** `tests/io/json/test_ujson.py:579 toDict`, called by the ujson C extension.
  - **scipy:** `sparse/csgraph/_validation.py` is imported by `_traversal.so`; it appears in the Cython string table.
- **Proposed fix:**
  - scan `.pyi` stubs, shipped headers and the string tables of `.so`/`.pyd` files inside the analysed tree for dotted names and identifiers, as evidence (cached per file);
  - lower PM404 confidence in packages that contain extension modules.
- **Acceptance:** the torch and scipy examples drop below 80.

## F7 Confidence-70 dynamic-access penalty too broad

**Severity:** high (hides true positives) · **Rules:** PM401–403 · **Code:** dead-code seeds/levels (`level::DYNAMIC`, reasons `module-dynamic-access`, `getattr-prefix`)

- **Problem:** the penalty caps definitions at 70, below the default gate. It is triggered by:
  - any variable `getattr` or `globals()` in the module;
  - `x.__dict__` on unrelated objects;
  - a class `__getattr__` that forwards elsewhere;
  - `getattr-prefix` patterns applied **repo-wide** (3+ byte prefixes), seen in unrelated test code.
- **Evidence:**
  - **pandas:** `TimeGrouper._get_time_period_bins` (`core/resample.py:2710`) and `MultiIndex._recode_for_new_levels` (`core/indexes/multi.py:3034`) are genuinely dead (each name appears once in the tree) but scored 70. The trigger is a class `__getattr__` and `getattr(self, name)` in an ancestor class.
  - **Internal repos:** confidence 70 was true 4/4 in two codebases. `logger.info(args.__dict__)` triggered it. A test's `getattr(obj, f"get_{x}_ids")` lowered unrelated `get_*`/`load_*` functions in other packages (20 findings).
- **Proposed fix:**
  - apply the penalty only when the dynamic receiver can be the definition's own module or class (self/cls/module alias);
  - ignore `__dict__` reads on unrelated objects;
  - scope prefix/suffix patterns to the receiver's module or class (see F5);
  - keep 70 only where a matching pattern exists.
- **Acceptance:** the pandas examples are reported at ≥ 80; pandas `ArrowExtensionArray._dt_*` (real f-string dispatch, `core/indexes/accessors.py:140`) and `OpenpyxlWriter._convert_to_*` stay below 80.

## F8 Project and framework conventions not understood

**Severity:** medium–high · **Rules:** PM401–404, import roots, test detection, metrics

| Convention | Problem today | Proposed handling |
|---|---|---|
| Alembic migrations | `upgrade()`/`downgrade()` in `versions/` reported dead: 29 findings at confidence 100 in one internal repo; `upgrade` only escaped because the word appears in a CI file | Treat them as roots in a `versions/` dir next to `env.py` / `alembic.ini` (or referenced by `script_location`) |
| `setup.py`/`setup.cfg` src layouts | Not import roots: ids like `pkg.src.pkg.mod`, imports of `pkg.*` unresolved, `sys.path.insert` + `from src.x import` scripts | Read `package_dir`/`packages=find_packages("src")` and `setup.cfg [options]`; use them as import roots |
| Entry points outside Python | Dockerfile `CMD`, `uvicorn module:app`, `gunicorn module:app`, `python -m module`, compose files, a shell `docker_entrypoint.sh` assembling `$PKG.api.$SERVER:$APP` | Parse common forms as roots and application evidence (F1) |
| marimo notebooks | `@app.cell` functions `_`, `_#2`… reported dead and given PM102/PM105 | Recognise `marimo.App` cells as roots; exclude them from metrics |
| Notebook-style cell scripts ("run cells in VS Code", `# %%`) | Helpers reported dead | Treat `# %%` cell files as scripts with module-level roots |
| Hydra `_target_` through `__init__` re-exports | Config evidence gives `name-in-importer` 10 instead of `config-reference` | Resolve dotted config paths through re-exports to the defining symbol |
| Directory-scan registration (e.g. Atlas `models_path`, plugin dirs) | Modules reported never imported | Config option for "loaded-by-scan" globs (PROPOSED), or evidence from tool config files |
| `integration_tests/`, `integrate_test_*.py` | Not test code (metrics and PM501 noise) | Extend test detection |
| File-level `# ruff: noqa` / `# flake8: noqa`; "generated, do not edit" headers | Not honoured: sympy `parsing/latex/_antlr/` (28 metric findings), torch `fx_passes/serialized_patterns` (464 PM405; these carry a standalone `# noqa: F401, E501` line, which as in ruff applies only to that line, but the files are generated) | Honour file-level directives; exclude generated files from metrics and PM405 |
| Framework presets | Not enabled automatically even when fastapi/pydantic are declared dependencies (460 `unknown-decorator` findings in one repo) | Auto-enable presets from the unit's declared dependencies |
| Non-importable hyphenated dirs used as golden fixtures | sympy `parsing/autolev/test-examples/*.py` (18 of 24 PM405), compared as text by a test | Exclude non-importable fixture dirs from PM405, or lower confidence |
| Library public API not in `__all__` | Scored exactly 80, so it passes the gate (torch 287 findings: `_inductor/__init__.py:57 aoti_compile_and_package`, which error messages tell users to call) | Lower to 70 when the name is public and referenced in docstrings or messages, or treat public names of a library's top-level packages as roots |

**Acceptance:** one fixture per row.

---

# Part C: Output, metric, query and deps defects (PROPOSED)

## F9 Output, metric and query defects

**Severity:** medium

1. **Nested functions counted twice.** An enclosing function's McCabe and cognitive scores include its nested functions, which are also reported themselves.
   - **Examples:**
     - torch `TorchInGraphFunctionVariable._get_handlers`: cyclomatic 246 from 53 nested handlers, 2 branches of its own;
     - torch `register_onednn_fusion_ops`: cognitive 448;
     - scipy `_axis_nan_policy_factory`: reported three times;
     - pandas `generate_numba_ewm_func` / `.ewm`;
     - an internal `spawn_agent_toolset`: 337, mostly from nested functions.
   - **Fix:** report a nested function's complexity once (exclude nested bodies from the parent, or add an option). This is a conscious departure from ruff C901 parity for PM101: decide and document it (OPEN).
2. **PM501 pairs reported twice** (A↔B and B↔A): sympy 3,609 findings; one internal repo 715 for 441 unique pairs. Docstrings and numeric literals count as matching tokens: scipy `fft/_basic.py:940` vs `:1308` is reported as "369 duplicated lines", of which 352 differ; `_lebedev.py` tables are different constants. **Fix:** one finding per pair (O1 does this in human output; fix it at the source), ignore docstring and literal tokens in matching, and add `end_line` to `other`.
3. **Wording:** "N lines long without docstrings" reads as "has no docstrings". Use "N lines excluding docstrings" (PM104, PM107).
4. **`impact` noise:** it lists every transitive importer of the target's module, even for a function with 0 callers ("0 affected … 1309 importing modules"). **Fix:** affected symbols first; importers as a count plus top N (O4).
5. **`@overload`:** `callers` and `path` bind calls to the last `@overload` stub, so the implementation shows 0 callers and paths through it are missing. Dead code already handles overloads correctly.
6. **`Class().method()`** resolves only `__init__` (scipy `MMFile().write`); classes taken from dicts (`solver = method(...)`) give 0 callers for `OdeSolver.step`.
7. **PM204 coupling counts override-dispatch edges to subclasses** (scipy `rv_generic` "uses 133 classes"). Exclude override edges.
8. **The `summary` "most called" list includes `possible` edges:** every `x.model_dump()` gets attributed to the only first-party override (264 callers, 161 of them possible).
9. **Reasons:**
   - undocumented reasons appear in output (`dynamic-base`, `library-api-method`);
   - `only-used-by-unused-code` hides other evidence (decorator, config);
   - some reasons don't match the evidence: `name-in-importer` was given for a name that appears nowhere else; a class was lowered because the same class name exists in an unrelated package;
   - "only used by unused code" for a recursive SCC with no external caller should say "never used (recursive)".
10. **`callers`** lists the class's own definition line as a "[likely, dispatch]" caller; and `callers` across services can list a same-named method in another service as possible.
11. **`impact`** says "0 in tests" while listing a test module as an importer.
12. **The `.pyi` files** that the docs say are not analysed showed up in PM501.

## F10 deps defects

**Severity:** medium · **Code:** `crates/pymapper-cli/src/cmd/deps.rs`, `cmd/deps/hygiene.rs`, `crates/pymapper-core/src/deps/`

1. PM601 (undeclared dependency) fires **once per import site** (394 findings in one repo) instead of once per (package, distribution) with the sites listed.
2. PM604 (transitive dependency) **never fired**, even for a package available only through another package's extra.
3. `deps --score` printed `api used 46/2` (more used than exist), and listed a distribution with 0 importers as "used by first-party code".
4. PM602 false positives: tools used as CLIs (in compose files or Makefiles) and kernel-provisioning dependencies. Consider reading compose/Makefile/Dockerfile for CLI usage (ties to F8).

## F11 Front-end speed and memory targets

**Severity:** low

- **Speed:** the full extractor (lex + parse + facts + metrics) runs at about 222–225 MB/s/core, against the design target of 400 (S1.6 got +6% via a SIMD line index and single-pass string evidence; details in `docs/benchmarks.md`, "Front-end speed (S1.6)"). Lexing and parsing alone cost about 10 cycles/byte, so the target needs a redesign rather than tuning: the lexer filling token batches, and folding the metrics token pass into parser dispatch. Wall time on big trees is I/O-bound anyway.
- **Memory:** facts take 15.22 B/LOC on the 16M-LOC venv, against a 15 target, after the `doc_lines` metrics column was added.
- **Testing:** `cargo xtask facts-digest` is a manual front-end equivalence check using `DefaultHasher`, which is not stable across Rust releases. Make it an automated golden test with a stable hasher.

---

# Part D: Repository hygiene

## F12 Internal names committed to the repository

**Severity:** high (blocks pushing to the public GitHub remote) · **Status:** PROPOSED; must be done before any push

- **Problem:** workflow agents committed internal project names, real findings from internal repos, and local filesystem paths into tracked files. `master` is 248 commits ahead of `origin/master` (`7fb544c`), and **nothing has been pushed**. Known locations (the exact scan command with the name patterns is in `HANDOFF.md`, kept out of this file on purpose):
  - `docs/benchmarks.md` (benchmark tables naming an internal monorepo, local corpus paths);
  - `docs/gui.md` (benchmark tables);
  - `docs/cli.md` (one line);
  - `docs/deps.md` (real findings with internal paths);
  - `README.md` and `pymapper.toml.template` (an internal module used as the entry-point example);
  - `docs/design.md` (local paths in the bench rule);
  - `xtask/src/bench/corpus.rs` (default corpus paths);
  - about 10 test files: `crates/pymapper-core/src/extract/config.rs`, `roots/tests.rs`, `analysis/deadcode/test_code_tests.rs`, `rollup/tests.rs`, `resolve/tests.rs`, `resolve/calls_tests.rs`, `index/tests.rs`, `cache/tests.rs`, `analysis/dupes/tests.rs`.
  - `tests/fixtures/db.py` is an **untracked, proprietary** file (gitignored via `/tests/corpus/fixtures/db.py`; it is in `tests/fixtures/`): never commit it.
- **Proposed fix:**
  - replace with neutral descriptions ("a 480k-line monorepo") and generic examples (`myapp.plugins.tools:*`);
  - benchmark corpora configurable via environment variables (`PYMAPPER_STDLIB`, `PYMAPPER_VENV` exist) with no internal defaults;
  - then decide on history before the first push (OPEN; user decision): squash to a clean commit, or `git filter-repo` to rewrite.
- **Acceptance:** `git grep` and `git log -p` find none of these names or local paths on the commits to be pushed.

## R1 Repository and tooling housekeeping

**Status:** mostly DONE · **Severity:** low

- **Stale `rewrite` worktree:** DONE (2026-10-13): pruned and the `rewrite` branch deleted (it was fully contained in `master`).
- **Build output in worktrees:** each worktree builds its own `target/`, which reached about 13 GB. Future multi-worktree runs should run `cargo clean` at the end or share `CARGO_TARGET_DIR` (with care: a shared target dir serialises builds).
- **Demo project:** the seeded demo is the **untracked** root `sample_python/` (ignored). It holds deliberate problems for every rule family (see HANDOFF). Do **not** seed the tracked `tests/corpus/sample_python`: about 12 tests depend on its exact contents (resolve, query, dead code, rollup, coupling, hotspot, GUI offscreen, xtask layout). DONE (Phase 0): the tracked copy for goldens is `tests/corpus/demo/`.
- **`.gitignore`:** DONE (`68763cb`): the allow-list was replaced by explicit ignores. It had a real bug: the proprietary-fixture rule pointed at `tests/corpus/fixtures/db.py` while the file is `tests/fixtures/db.py`, so it was not ignored. `ISSUES.md` is now tracked; `HANDOFF.md`, `TODO.md`, `.pymapper.toml`, `reports/`, `sample_python/`, `.claude/` and `tests/fixtures/` are ignored, so fresh worktrees lack them (give agents absolute paths to `HANDOFF.md`).
- **Stray caches:** runs without `--cache-dir` write `.pymapper_cache/` (self-gitignored) into project roots. Agents analysing other repos must always pass `--cache-dir /tmp/...`. Remove stray caches after runs.
