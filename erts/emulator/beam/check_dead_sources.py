#!/usr/bin/env python3
# %CopyrightBegin%
# SPDX-License-Identifier: Apache-2.0
#
# Mechanical dead-source checker for the legacy emulator C/C++ tree.
#
# WHY THIS EXISTS
#   erts/emulator/ is the legacy BEAM emulator, kept only as the FLAVOR=emu
#   bootstrap fallback while erts_rust/ becomes the runtime.  Sources that no
#   Makefile, no #include and no symbol reference reaches are dead weight and
#   may be deleted.  A full emulator compile is far too expensive to use as the
#   bar, so this script produces the mechanical reference proof instead.
#
# WHAT IT PROVES (decisive reference classes)
#   A  BUILD REFERENCE  - the file's stem appears as a whole word (or the full
#                         basename appears verbatim) in a build/config source:
#                         Makefile*, *.mk, *.in, *.m4, *.ac, *.am, *.tab, *.src,
#                         *.spec, *.sh, *.bat, *.def, *.nix, *.toml, *.exs,
#                         *.escript, *.erl, *.hrl, configure*, otp_build, CI
#                         yaml.  A $(wildcard ...) pattern that matches the path
#                         also counts.
#   B  INCLUDE REFERENCE - an #include of the basename (or a path ending in it)
#                         in any other C-family source.
#   C  SYMBOL REFERENCE - an identifier the file defines (function definition,
#                         #define, typedef name) appears as a whole word in any
#                         other C-family or build/config source.
#
#   Only whole-word matches count, so the stem "bin_drv" is NOT considered
#   referenced by the unrelated identifier "erts_bin_drv_alloc".  Files whose
#   own stem is generic (e.g. hash.c / time.c) are kept merely because the word
#   occurs somewhere - the check errs towards keeping files.
#
# ADVISORY (printed, never decisive)
#   A mention in any other tracked text file (docs, CONTEXT.md, erts_rust
#   sources, ...).  A doc mention is not a build/include/symbol reference, so it
#   does not make a file live - but it is reported so a human can see why a
#   grep "found" the name.
#
# USAGE
#   python3 check_dead_sources.py                 # report; exit 1 if deletable
#   python3 check_dead_sources.py --write-report  # also write dead_source_report.txt
#   python3 check_dead_sources.py --explain GLOB  # show evidence lines per file
#
# Scope: every tracked erts/emulator/**/*.{c,h,cpp,hpp,cc}.

import argparse
import fnmatch
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]          # repo root
EMU_REL = Path("erts/emulator")
CANDIDATE_EXTS = {".c", ".h", ".cpp", ".hpp", ".cc"}
CFAMILY_EXTS = {".c", ".h", ".cpp", ".hpp", ".cc", ".tab", ".def", ".inc"}

# Sources that can carry a *build* reference (class A).
BUILD_NAMES = {"makefile", "otp_build", "configure", "configure.ac", "configure.src"}
BUILD_EXTS = {".mk", ".in", ".m4", ".ac", ".am", ".tab", ".src", ".rel", ".spec",
              ".sh", ".bat", ".def", ".nix", ".toml", ".exs", ".escript", ".erl",
              ".hrl", ".yml", ".yaml", ".cmake", ".mk.in", ".cfg", ".pro"}

# Never a reference source: markdown/doc files and this tool's own output (which
# lists the very names we are testing for).
DOC_EXTS = {".md", ".markdown", ".rst", ".txt"}
SELF_EXCLUDE = {"erts/emulator/beam/check_dead_sources.py",
                "erts/emulator/beam/dead_source_report.txt"}

# Unreferenced but intentionally retained, with the reason (checked before a file
# is ever reported as deletable).  Directory prefixes.
EXEMPT_PREFIXES = (
    # Vendored third-party copies: not OTP code, updated only by their updater.
    ("erts/emulator/asmjit/", "vendored 3PP (asmjit), see Makefile.in ASMJIT_CPP"),
    ("erts/emulator/pcre/", "vendored 3PP (PCRE2), always built in-tree"),
    ("erts/emulator/ryu/", "vendored 3PP (ryu)"),
    ("erts/emulator/zlib/", "vendored 3PP (zlib)"),
    ("erts/emulator/zstd/", "vendored 3PP (zstd)"),
    # Test fixtures: loaded by string name / compiled by the suites, not by the
    # emulator build.  Deleting them is a test-suite decision, not this mandate.
    ("erts/emulator/test/", "test fixture under test/ (owned by the CT suites)"),
    ("erts/emulator/asan/", "sanitizer helper/suppression fixture"),
    ("erts/emulator/valgrind/", "valgrind suppression fixture"),
)
# Individual files that must stay even if mechanically unreferenced.
EXEMPT_FILES = {
    "erts/emulator/sys/unix/driver_int.h": "installed public header (per-OS driver API)",
    "erts/emulator/sys/win32/driver_int.h": "installed public header (per-OS driver API)",
    "erts/emulator/sys/win32/erl_win_dyn_driver.h": "installed public header (win32)",
    "erts/emulator/beam/erl_driver.h": "installed public driver API header",
    "erts/emulator/beam/erl_nif.h": "installed public NIF API header",
    "erts/emulator/beam/erl_nif_api_funcs.h": "installed public NIF API header",
    "erts/emulator/beam/erl_drv_nif.h": "installed public header shared by erl_nif.h/erl_driver.h",
    "erts/emulator/beam/erl_internal_test.h": "installed to erts-*/include/internal",
}

WILDCARD_RE = re.compile(r"wildcard\s+([^)\n]*)")
GLOB_CHARS = re.compile(r"[*?\[]")
INCLUDE_RE = re.compile(r'#\s*include\s*[<"]([^">]+)[">]')
TOKEN_RE = re.compile(r"[A-Za-z_]\w*")
MACRO_RE = re.compile(r"^\s*#\s*define\s+([A-Za-z_]\w*)", re.M)
TYPEDEF_RE = re.compile(r"typedef[^;{}]*?([A-Za-z_]\w*)\s*;", re.S)
# Line-start definition/prototype (OTP C style keeps these at column 0), which
# covers both ANSI and K&R style.  `static`/`inline` definitions are FILE-LOCAL
# and must never be used as evidence - e.g. bin_drv.c's static read_fill()
# collides with the unrelated read_fill() of inet_drv.c.
FUNC_LINE_RE = re.compile(r"^([A-Za-z_][\w \t\*]*?)\b([A-Za-z_]\w*)\s*\(", re.M)
GVAR_LINE_RE = re.compile(r"^([A-Za-z_][\w \t\*]*?)\b([A-Za-z_]\w*)\s*(\[[^\]]*\])?\s*=", re.M)
HEADER_EXTS = {".h", ".hpp", ".inc"}

C_KEYWORDS = {
    "if", "else", "for", "while", "do", "switch", "case", "default", "return",
    "sizeof", "struct", "union", "enum", "typedef", "static", "const", "extern",
    "inline", "volatile", "register", "unsigned", "signed", "void", "char",
    "short", "int", "long", "float", "double", "goto", "break", "continue",
    "defined", "include", "define", "ifdef", "ifndef", "endif", "elif", "pragma",
    "error", "warning", "line", "TRUE", "FALSE", "NULL", "while0",
}


def tracked_files():
    out = subprocess.run(["git", "-C", str(ROOT), "ls-files", "-z"],
                         capture_output=True, check=True).stdout
    return [Path(p) for p in out.decode("utf-8", "replace").split("\0") if p]


def read_text(path):
    try:
        data = (ROOT / path).read_bytes()
    except OSError:
        return None
    if b"\0" in data[:8192]:
        return None
    try:
        return data.decode("utf-8", "replace")
    except Exception:
        return None


def is_makefile_source(path):
    """Sources that wire files into a build (object lists, wildcards, CI)."""
    name = path.name
    if name.startswith("Makefile") or name in (
            "otp_build", "otp_patch_apply", "erl-build-tool-vars.sh", "configure"):
        return True
    return path.suffix.lower() in {".mk", ".in", ".m4", ".ac", ".am", ".tab",
                                  ".src", ".spec", ".rel", ".def", ".cmake",
                                  ".cfg", ".pro", ".bat"}


def is_build_source(path):
    """Superset of is_makefile_source: also scripts/suites that name a source
    file directly (a CT suite opening "dead_port.c", a CI script, ...)."""
    if is_makefile_source(path):
        return True
    if path.name.lower() in ("configure.ac", "configure.src"):
        return True
    return path.suffix.lower() in {".sh", ".nix", ".toml", ".exs", ".escript",
                                  ".erl", ".hrl", ".yml", ".yaml"}


def glob_to_regex(pattern):
    """Translate a make $(wildcard) pattern to a regex matching any path suffix.

    Make variable prefixes are stripped first ($(ASMJIT_DIR)/core/*.cpp becomes
    /core/*.cpp) and the pattern is allowed to match at any directory depth, so
    `beam/*.c` covers erts/emulator/beam/x.c without needing to know which
    Makefile directory the pattern was written in.
    """
    pat = re.sub(r"\$\([^)]*\)|\$\{[^}]*\}", "", pattern).strip()
    if not pat or not GLOB_CHARS.search(pat):
        return None
    out, i = [], 0
    while i < len(pat):
        c = pat[i]
        if pat.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif c == "*":
            out.append("[^/]*")
            i += 1
        elif c == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(c))
            i += 1
    return re.compile("^(?:.*/)?" + "".join(out).lstrip("/") + "$")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--write-report", action="store_true",
                    help="write dead_source_report.txt next to this script")
    ap.add_argument("--explain", metavar="GLOB",
                    help="print the evidence lines for candidate files matching GLOB")
    ap.add_argument("--out", default=None, help="report path (default: dead_source_report.txt)")
    args = ap.parse_args()

    files = tracked_files()
    candidates = sorted(p for p in files
                        if p.suffix.lower() in CANDIDATE_EXTS
                        and str(p).startswith(str(EMU_REL) + "/"))

    # ---- index the reference sources -------------------------------------
    # Single pass: tokenize each source once and accumulate *inverted* indexes
    # (token -> sources) restricted to the tokens that matter, i.e. the stems of
    # the candidate files.  Without this the check is quadratic over the tree
    # (13k sources x ~750 candidates) and takes minutes.
    candidate_stems = {p.name.rsplit(".", 1)[0] for p in candidates}
    src_text = {}          # path -> text (kept for candidate files only)
    globs = []             # (pattern_source_path, pattern, regex)
    tokens_all = Counter()
    tokens_cfamily = Counter()
    tokens_build = Counter()
    stem_refs_all = {}     # stem -> list of sources mentioning it
    stem_refs_build = {}   # stem -> list of *build* sources mentioning it
    stem_refs_make = {}    # stem -> list of *makefile* sources mentioning it
    basename_refs = {}     # "x.c" -> list of makefile sources mentioning it verbatim
    includes = {}          # basename -> list of "path" that #include it
    own_tokens = {}        # candidate rel -> Counter
    object_res = []        # regex matching object basenames listed in a Makefile

    basename_re = re.compile(r"([A-Za-z_][\w.\-]*\.(?:c|h|cpp|hpp|cc|tab|inc|def))\b")
    obj_token_re = re.compile(r"([A-Za-z_][\w.\-]*\.(?:o|obj))\b")
    for p in files:
        rel = str(p)
        if rel in SELF_EXCLUDE or p.suffix.lower() in DOC_EXTS:
            continue
        try:
            if (ROOT / p).stat().st_size > 4_000_000:
                continue
        except OSError:
            continue
        txt = read_text(p)
        if txt is None:
            continue
        is_c = p.suffix.lower() in CFAMILY_EXTS
        is_b = is_build_source(p)
        is_m = is_makefile_source(p)
        cnt = Counter(TOKEN_RE.findall(txt))
        tokens_all.update(cnt)
        if is_c:
            tokens_cfamily.update(cnt)
            for m in INCLUDE_RE.finditer(txt):
                includes.setdefault(m.group(1).rsplit("/", 1)[-1], []).append(rel)
        if is_b:
            tokens_build.update(cnt)
            for stem in candidate_stems.intersection(cnt):
                stem_refs_build.setdefault(stem, []).append(rel)
                if is_m:
                    stem_refs_make.setdefault(stem, []).append(rel)
            if is_m:
                for m in basename_re.finditer(txt):
                    base = m.group(1)
                    if base.rsplit(".", 1)[0] in candidate_stems:
                        basename_refs.setdefault(base, []).append(rel)
                for m in WILDCARD_RE.finditer(txt):
                    for tok in m.group(1).split():
                        rx = glob_to_regex(tok)
                        if rx:
                            globs.append((rel, tok, rx))
                # Object lists may name objects through make variables, e.g.
                # $(OBJDIR)/erl_$(ERLANG_OSTYPE)_sys_ddll.o - expand those into
                # word wildcards so such a file is not reported as unbuilt.
                for m in obj_token_re.finditer(txt):
                    body = re.sub(r"\$\([^)]*\)|\$\{[^}]*\}", "\x00", m.group(1))
                    pat = "".join("[^/]*" if ch == "\x00" else re.escape(ch)
                                  for ch in body)
                    object_res.append(re.compile(pat + "$"))
        for stem in candidate_stems.intersection(cnt):
            stem_refs_all.setdefault(stem, []).append(rel)

    # Tokens of the candidate files themselves must be subtractable.
    for c in candidates:
        txt = read_text(c)
        own_tokens[str(c)] = Counter(TOKEN_RE.findall(txt)) if txt else Counter()
        src_text[str(c)] = txt or ""

    # ---- decide ----------------------------------------------------------
    def defined_symbols(c):
        """Externally visible symbols only.

        A translation unit's own static functions, file-local macros and local
        typedefs are invisible to the rest of the program, so a same-named
        symbol elsewhere is NOT evidence that this file is reachable (that is
        exactly the "bin_drv.c defines static read_fill()" false positive).
        Headers, in contrast, are included wholesale, so everything they define
        counts.
        """
        txt = src_text.get(str(c)) or ""
        syms = set()
        is_header = c.suffix.lower() in HEADER_EXTS
        if is_header:
            syms |= set(MACRO_RE.findall(txt))
            syms |= set(TYPEDEF_RE.findall(txt))
        for m in FUNC_LINE_RE.finditer(txt):
            first = m.group(1).split()[:1]
            if first and first[0] in ("static", "inline", "typedef"):
                continue
            syms.add(m.group(2))
        for m in GVAR_LINE_RE.finditer(txt):
            first = m.group(1).split()[:1]
            if first and first[0] in ("static", "typedef", "return"):
                continue
            syms.add(m.group(2))
        return {s for s in syms if len(s) >= 4 and s not in C_KEYWORDS}

    def evidence(c):
        """Return (evidence dict, strong flag).

        `strong` is False when the only references found are *non-makefile*
        build-source mentions (an .erl suite, a CI script, a .def list).
        """
        rel, base, stem = str(c), c.name, c.name.rsplit(".", 1)[0]
        own_all = own_tokens[rel]
        own_build = own_all if is_build_source(c) else Counter()
        ev = {}
        strong = False

        # class A: build reference (whole-word stem, verbatim basename, glob)
        build_hits = [f"{s}: mentions '{stem}'" for s in stem_refs_build.get(stem, ())]
        build_hits += [f"{s}: mentions '{base}'" for s in basename_refs.get(base, ())]
        for grel, gpat, rx in globs:
            if rx.match(str(c)) or rx.match(str(c.relative_to(EMU_REL))) or rx.match(c.name):
                build_hits.append(f"{grel}: $(wildcard {gpat}) covers this file")
        if build_hits:
            ev["A build ref"] = build_hits
            if stem_refs_make.get(stem) or basename_refs.get(base) or \
                    any("wildcard" in h for h in build_hits):
                strong = True

        # class B: include reference
        inc_hits = [f"{h}: #include \"{base}\"" for h in includes.get(base, ()) if h != rel]
        if inc_hits:
            ev["B include ref"] = inc_hits
            strong = True

        # class C: a defined symbol used elsewhere
        sym_hits = []
        for s in sorted(defined_symbols(c)):
            avail = (tokens_cfamily[s] - own_all[s]) + \
                    max(tokens_build[s] - own_build[s], 0)
            if avail > 0:
                sym_hits.append(f"symbol '{s}' used {avail}x outside this file")
        if sym_hits:
            ev["C symbol ref"] = sym_hits
            strong = True
        return ev, strong

    deletable, exempt, referenced, weak = [], [], [], []
    details = {}
    for c in candidates:
        rel = str(c)
        ev, strong = evidence(c)
        reason = EXEMPT_FILES.get(rel)
        if reason is None:
            for pref, why in EXEMPT_PREFIXES:
                if rel.startswith(pref):
                    reason = why
                    break
        details[rel] = (ev, reason)
        if ev:
            referenced.append(rel)
            if not strong and not reason:
                weak.append((rel, ev))
        elif reason:
            exempt.append((rel, reason))
        else:
            deletable.append(rel)

    if args.explain:
        pat = args.explain
        for rel in sorted(details):
            if not fnmatch.fnmatch(rel, pat) and pat not in rel:
                continue
            ev, reason = details[rel]
            verdict = "REFERENCED" if ev else ("EXEMPT: " + reason if reason else "UNREFERENCED")
            print(f"== {rel}  [{verdict}]")
            for cls, hits in ev.items():
                print(f"   {cls}:")
                for h in hits[:8]:
                    print(f"     - {h}")
            adv = [s for s in stem_refs_all.get(Path(rel).name.rsplit('.', 1)[0], ()) if s != rel]
            if adv and not ev:
                print(f"   advisory-only mentions ({len(adv)}): {', '.join(adv[:8])}")
        return 0

    def is_built(c):
        """Is this compilation unit named by an object list (or a wildcard)?"""
        stem = c.name.rsplit(".", 1)[0]
        base = stem + ".o"
        for rx in object_res:
            if rx.search(base):
                return True
        # wildcard-covered sources host their objects through a pattern rule
        for grel, gpat, rx in globs:
            if rx.match(str(c)) or rx.match(str(c.relative_to(EMU_REL))) or rx.match(c.name):
                return True
        return False

    lines = []
    lines.append("Dead-source checker report for erts/emulator/**/*.{c,h,cpp,hpp,cc}")
    lines.append(f"candidates checked: {len(candidates)}   "
                 f"referenced: {len(referenced)}   "
                 f"exempt (retained by rule): {len(exempt)}   "
                 f"deletable: {len(deletable)}")
    lines.append("")
    lines.append("DELETABLE (no build ref, no #include, no symbol use):")
    if deletable:
        for rel in deletable:
            lines.append(f"  {rel}")
    else:
        lines.append("  (none)")
    lines.append("")
    lines.append("EXEMPT (mechanically unreferenced but retained):")
    for rel, why in exempt:
        lines.append(f"  {rel}  <- {why}")
    lines.append("")
    lines.append("WEAK (retained, but the only reference is a non-makefile mention -")
    lines.append("      a CT suite, an escript or a CI list; audit before deleting):")
    for rel, ev in weak:
        hits = ev.get("A build ref", [])
        lines.append(f"  {rel}  <- {hits[0] if hits else '?'}")
    lines.append("")
    lines.append("REFERENCED BUT NOT IN AN OBJECT LIST (compilation units reachable only by")
    lines.append("#include / symbol use - i.e. dead-but-referenced; audit before deleting):")
    unbuilt = [rel for rel in referenced
               if Path(rel).suffix.lower() in (".c", ".cpp", ".cc")
               and not is_built(Path(rel))
               and not any(rel.startswith(pref) for pref, _ in EXEMPT_PREFIXES)]
    if unbuilt:
        for rel in unbuilt:
            lines.append(f"  {rel}")
    else:
        lines.append("  (none)")
    report = "\n".join(lines) + "\n"
    sys.stdout.write(report)

    if args.write_report:
        out = Path(args.out) if args.out else (
            Path(__file__).resolve().parent / "dead_source_report.txt")
        out.write_text(report)
        print(f"[written to {out.relative_to(ROOT) if out.is_absolute() else out}]")

    return 1 if deletable else 0


if __name__ == "__main__":
    sys.exit(main())
