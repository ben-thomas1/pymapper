"""Differential test of the pymapper lexer against CPython's tokenize (3.14+).

Usage: python3 xtask/scripts/lexdiff.py [--xtask PATH] [--show N] [--fuzz N [--seed S]] [ROOT...]

Walks ROOT for *.py/*.pyi, runs `xtask lex-dump` on them, tokenizes each file with
CPython, and compares (category, start byte, end byte) streams. Files CPython rejects,
non-UTF-8 files and files containing CR are skipped (CRLF is covered by unit tests).

Normalisations (representation differences, not bugs): consecutive FSTRING_MIDDLE /
TSTRING_MIDDLE tokens are merged and end where the next token starts (CPython splits
them at `{{`/`}}` and emits empty ones); zero-length NL tokens are dropped.
"""

import argparse
import io
import os
import subprocess
import sys
import tokenize

MIDDLES = {"FSTRING_MIDDLE", "TSTRING_MIDDLE"}


def walk(roots):
    for root in roots:
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in ("__pycache__", ".git")]
            for name in filenames:
                if name.endswith((".py", ".pyi")):
                    yield os.path.join(dirpath, name)


def cpython_tokens(data):
    bom = data.startswith(b"\xef\xbb\xbf")
    text = data[3:].decode("utf-8") if bom else data.decode("utf-8")
    base = 3 if bom else 0
    # Byte offset of each line start, and per-line char -> byte conversion. Split on "\n"
    # only, like tokenize's rows (str.splitlines also splits on \x0c, \x1c, \x85, ...).
    lines = text.split("\n")
    lines = [line + "\n" for line in lines[:-1]] + ([lines[-1]] if lines[-1] else [])
    starts = [base]
    for line in lines:
        starts.append(starts[-1] + len(line.encode("utf-8")))

    def off(row, col):
        if row - 1 >= len(lines):
            return starts[-1]
        line = lines[row - 1]
        return starts[row - 1] + len(line[:col].encode("utf-8"))

    out = []
    prev = None
    for t in tokenize.generate_tokens(io.StringIO(text).readline):
        name = tokenize.tok_name[t.type]
        s, e = off(*t.start), off(*t.end)
        # tokenize splits `1.x` / `1abc` into NUMBER NAME, but the compiler rejects them
        # ("invalid decimal literal") and so do we; only these keywords may touch a number.
        if prev is not None and prev.type == tokenize.NUMBER and name == "NAME" and t.start == prev.end:
            if not t.string.startswith(("and", "else", "for", "if", "in", "is", "not", "or")):
                raise SyntaxError("invalid decimal literal")
        prev = t
        if name == "NL" and s == e:
            continue
        if name in MIDDLES and s == e:
            continue
        out.append([name, s, e])
    return normalise(out)


def normalise(toks):
    merged = []
    for t in toks:
        if merged and t[0] in MIDDLES and merged[-1][0] == t[0]:
            merged[-1][2] = t[2]
        else:
            merged.append(list(t))
    for i, t in enumerate(merged):
        if t[0] in MIDDLES and i + 1 < len(merged):
            t[2] = merged[i + 1][1]
    return [tuple(t) for t in merged]


def ours(xtask, paths):
    proc = subprocess.run([xtask, "lex-dump"], input="\n".join(paths).encode(), capture_output=True, check=True)
    result, cur = {}, None
    for line in proc.stdout.decode().splitlines():
        if line.startswith("@ "):
            cur = line[2:]
            result[cur] = []
        else:
            k, s, e = line.split()
            result[cur].append([k, int(s), int(e)])
    return {p: normalise(v) for p, v in result.items()}


def report(path, a, b):
    i = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
    with open(path, "rb") as f:
        data = f.read()
    pos = a[i][1] if i < len(a) else (b[i][1] if i < len(b) else 0)
    print(f"{path}: token {i}: cpython {a[i] if i < len(a) else None} ours {b[i] if i < len(b) else None}")
    print(f"    context: {data[max(0, pos - 40):pos + 40]!r}")


def gen_expr(r, depth):
    k = r.randrange(12 if depth < 4 else 5)
    if k == 0:
        return r.choice(["x", "y1", "_", "café", "match", "type"])
    if k == 1:
        return r.choice(["0", "1_000", "0x_f", "1.5e-3", ".5j", "0b1", "1if x else 2"])
    if k == 2:
        return gen_plain_string(r)
    if k == 3:
        return f"{gen_expr(r, depth + 1)}{r.choice([' + ', '*', ' != ', '<<', ' and ', ' .', '//'])}{gen_expr(r, depth + 1)}"
    if k == 4:
        return f"({gen_expr(r, depth + 1)})"
    if k in (5, 6, 7):
        return gen_fstring(r, depth + 1)
    if k == 8:
        return f"[{gen_expr(r, depth + 1)}, {gen_expr(r, depth + 1)}][1:2]"
    if k == 9:
        return f"{{{gen_expr(r, depth + 1)}: {gen_expr(r, depth + 1)}}}"
    if k == 10:
        return f"(lambda a: {gen_expr(r, depth + 1)})(1)"
    return f"(x := {gen_expr(r, depth + 1)})"


def gen_plain_string(r):
    q = r.choice(["'", '"', "'''", '"""'])
    body = r.choice(["", "a", "\\n", "\\'", '\\"', "{x}", "\\\\", "é"])
    return r.choice(["", "r", "b", "rb", "u", "R"]) + q + body + q


def gen_fstring(r, depth):
    q = r.choice(["'", '"', "'''", '"""'])
    prefix = r.choice(["f", "F", "rf", "fR", "t", "Tr"])
    parts = []
    for _ in range(r.randrange(4)):
        k = r.randrange(6)
        if k == 0:
            parts.append(r.choice(["a", " ", "{{", "}}", "\\t", "é", "\\N{DASH}" if "r" not in prefix.lower() else "z"]))
        elif k == 1 and len(q) == 3:
            parts.append("\n")
        else:
            field = gen_expr(r, depth)
            if len(q) == 1 and "\n" in field:
                field = "x"
            if r.randrange(4) == 0:
                field += "="
            if r.randrange(4) == 0:
                field += r.choice(["!r", "!s", "!a"])
            if r.randrange(3) == 0:
                field += ":" + r.choice(["", ">10", "{w}", ".{p}f", "x{y:>2}", "%Y"])
            parts.append("{" + (" " if field.startswith("{") else "") + field + "}")
    return prefix + q + "".join(parts) + q


def gen_file(r):
    lines, indent = [], 0
    for _ in range(r.randrange(1, 12)):
        k = r.randrange(6)
        pad = "    " * indent
        if k == 0 and indent < 6:
            lines.append(f"{pad}if {gen_expr(r, 2)}:")
            indent += 1
            lines.append("    " * indent + "pass")
        elif k == 1 and indent:
            indent -= r.randrange(1, indent + 1)
            lines.append("    " * indent + "z = 1")
        elif k == 2:
            lines.append(f"{pad}a = ({gen_expr(r, 1)},\n{pad}  # c\n   {gen_expr(r, 1)})")
        elif k == 3:
            lines.append(f"{pad}b = {gen_expr(r, 1)} \\\n        + 1  # pymapper: ignore[PM1]")
        else:
            lines.append(f"{pad}c = {gen_expr(r, 0)}")
        if r.randrange(5) == 0:
            lines.append("")
    return "\n".join(lines) + r.choice(["\n", ""])


def fuzz_dir(n, seed):
    import random
    import tempfile

    r = random.Random(seed)
    d = tempfile.mkdtemp(prefix="lexdiff-fuzz-")
    for i in range(n):
        with open(os.path.join(d, f"f{i}.py"), "w", encoding="utf-8") as f:
            f.write(gen_file(r))
    return d


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--xtask", default="target/debug/xtask")
    ap.add_argument("--show", type=int, default=20)
    ap.add_argument("--fuzz", type=int, default=0, metavar="N",
                    help="also compare N generated files (CPython-rejected ones are skipped)")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("roots", nargs="*")
    args = ap.parse_args()
    if args.fuzz:
        args.roots.append(fuzz_dir(args.fuzz, args.seed))
        print(f"fuzz files in {args.roots[-1]}")
    if not args.roots:
        ap.error("give at least one ROOT or --fuzz N")

    all_paths = list(walk(args.roots))
    compared = skipped = bad = 0
    for lo in range(0, len(all_paths), 500):
        expected = {}
        for path in all_paths[lo:lo + 500]:
            with open(path, "rb") as f:
                data = f.read()
            if b"\r" in data:
                skipped += 1
                continue
            try:
                expected[path] = cpython_tokens(data)
            except (UnicodeDecodeError, SyntaxError, tokenize.TokenError):
                skipped += 1
        paths = sorted(expected)
        compared += len(paths)
        got = ours(args.xtask, paths)
        for path in paths:
            a, b = expected[path], got.get(path, [])
            if a == b:
                continue
            bad += 1
            if bad <= args.show:
                report(path, a, b)
    print(f"{compared} files compared, {bad} differ, {skipped} skipped")
    if args.fuzz and not bad:
        import shutil

        shutil.rmtree(args.roots[-1])
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
