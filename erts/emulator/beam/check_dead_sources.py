#!/usr/bin/env python3
# %CopyrightBegin%
# SPDX-License-Identifier: Apache-2.0
#
# Mechanical dead-source checker for the legacy ERTS C/C++ tree.
#
# WHY THIS EXISTS
#   The legacy C/C++ ERTS tree is still the OTP build's default runtime and the
#   differential oracle for the erts_rust/ rewrite, so it must only ever shrink
#   by files that are PROVABLY unreferenced.  A full ERTS compile is far too
#   expensive to use as the bar, so this script produces the mechanical
#   reference proof instead.
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
# TWO TIERS
#   STRICT (decisive)   A fixpoint / transitive-closure liveness analysis over
#                       the build wiring.  A file is LIVE if it is
#                         (a) a compilation unit named by a real object-list
#                             entry (both the "$(VAR:%=prefix/%.o)" and the
#                             "$(VAR:.c=.o)" substitution forms - zlib's
#                             ZLIB_FILES, ryu's RYU_FILES, lib_src's
#                             ETHREAD_LIB_SRC - are resolved back to the
#                             sources they name), or covered by a
#                             $(wildcard ...) pattern, or
#                         (b) reached from a live file through an #include of its
#                             basename or through a symbol it defines.
#                       Everything else is STRICT DEAD.  Only candidate files
#                       (C-family sources) propagate liveness, so a reference
#                       that comes solely from a CT *.erl suite, a CI yaml, a
#                       doc or an already-dead file does NOT make a file live.
#                       The deletion gate is "STRICT DEAD and not exempt" = 0.
#   LOOSE (advisory)    The original single-pass A/B/C reference classes plus the
#                       per-file exemptions.  Reported for context only; it is
#                       NOT the deletion gate.
#
# USAGE
#   python3 check_dead_sources.py                       # default --root erts
#   python3 check_dead_sources.py --root erts/emulator  # narrow the scope
#   python3 check_dead_sources.py --root erts/lib_src
#   python3 check_dead_sources.py --root .              # whole repository
#   python3 check_dead_sources.py --write-report        # also write dead_source_report.txt
#   python3 check_dead_sources.py --explain GLOB        # show evidence lines per file
#
#   Exit status is 1 when the ACTIONABLE STRICT-dead set (STRICT dead and not
#   retained by an exemption rule) is non-empty, else 0.
#
# Scope: every tracked <root>/**/*.{c,h,cpp,hpp,cc}; the default root is `erts`.

import argparse
import fnmatch
import re
import subprocess
import sys
from collections import Counter, deque
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]          # repo root
DEFAULT_SCAN = "erts"                               # default --root scope
CANDIDATE_EXTS = {".c", ".h", ".cpp", ".hpp", ".cc"}
CFAMILY_EXTS = {".c", ".h", ".cpp", ".hpp", ".cc", ".tab", ".def", ".inc"}
COMPILE_EXTS = (".c", ".cpp", ".cc")                # units that get an object

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
# is ever reported as deletable, by BOTH the loose and the strict tier).
# Directory prefixes.
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
    # ERTS-level test tooling (e.g. erts/test/utils/gccifier.c): a C helper used
    # by the install/upgrade suites, not by the runtime build.
    ("erts/test/", "erts-level test tooling/fixture (owned by the CT suites)"),
    # Doc samples, deliberately not built/installed/tested by the OTP build.
    ("erts/example/", "doc sample; not built/installed/tested by the OTP build"),
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

# "$(VAR:%=$(OBJDIR)/%.o)" / "$(VAR:.c=.o)" - the object list is synthesized
# from VAR's contents, so VAR's assignment must be resolved to recover the
# sources it names (zlib's ZLIB_FILES, ryu's RYU_FILES, lib_src's
# ETHREAD_LIB_SRC / ERTS_INTERNAL_LIB_SRCS, ...).
SUBST_O_RE = re.compile(r"\$\(([A-Za-z_]\w*)\s*:((?:[^()\n]|\$\([^)\n]*\))*?\.o)\)")
MAKE_ASSIGN_RE = re.compile(r"^\s*([A-Za-z_]\w*)\s*[:+]?=\s*(.*)$")
# One item of a make variable that names sources: a bare object stem ("adler32")
# or a source file, possibly reached through a make variable
# ("$(ETHR_THR_LIB_BASE_DIR)/ethr_event.c").
SRC_TOKEN_RE = re.compile(r"^(?:[A-Za-z_][\w.\-]*|[^\s\\]*\.(?:c|cpp|cc))$")

C_KEYWORDS = {
    "if", "else", "for", "while", "do", "switch", "case", "default", "return",
    "sizeof", "struct", "union", "enum", "typedef", "static", "const", "extern",
    "inline", "volatile", "register", "unsigned", "signed", "void", "char",
    "short", "int", "long", "float", "double", "goto", "break", "continue",
    "defined", "include", "define", "ifdef", "ifndef", "endif", "elif", "pragma",
    "error", "warning", "line", "TRUE", "FALSE", "NULL", "while0",
}

# The scan scope, set from --root in main().  None means "the whole repository".
SCAN_REL = Path(DEFAULT_SCAN)


def exempt_reason(rel):
    """Why `rel` is retained although mechanically unreferenced (else None).

    Used by BOTH the loose and the strict tier so they agree on what counts as
    an intentional retention.
    """
    why = EXEMPT_FILES.get(rel)
    if why is not None:
        return why
    for pref, reason in EXEMPT_PREFIXES:
        if rel.startswith(pref):
            return reason
    # Generic CT-fixture rule: any *_SUITE_data/ fixture anywhere under test/.
    if "/test/" in rel and "_SUITE_data/" in rel:
        return "CT test fixture (owned by its *_SUITE)"
    return None


def scan_dir_relative(rel):
    """Parent directory of `rel` relative to SCAN_REL (report grouping)."""
    p = Path(rel)
    if SCAN_REL is not None:
        try:
            p = p.relative_to(SCAN_REL)
        except ValueError:
            pass
    parent = p.parent
    return str(parent) if str(parent) != "." else "(top level)"


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


def list_assignments(txt):
    """Map VAR -> set(items) for plain "VAR = a b \\\n c" assignments whose
    items all name a source (a bare object stem, or a .c/.cpp/.cc file).

    Anything with a flag, a path-less expansion or a non-source token is
    skipped, so "$(ZLIB_CFLAGS := -O2 ...)" and every flag list are ignored and
    only the source registries (ZLIB_FILES, RYU_FILES, ETHREAD_LIB_SRC,
    ERTS_INTERNAL_LIB_SRCS, ...) are captured.
    """
    assignments = {}
    for line in txt.replace("\\\n", " ").splitlines():
        m = MAKE_ASSIGN_RE.match(line)
        if not m:
            continue
        var, rhs = m.group(1), m.group(2).strip()
        if not rhs:
            continue
        tokens = rhs.split()
        if tokens and all(SRC_TOKEN_RE.match(tok) for tok in tokens):
            assignments.setdefault(var, set()).update(tokens)
    return assignments


def item_stem(item):
    """Object stem named by a make-variable item ("common/x.c" / "d2s" -> stem)."""
    return Path(item).name.rsplit(".", 1)[0]


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
    global SCAN_REL
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", default=DEFAULT_SCAN,
                    help="directory scope to scan, relative to the repo root "
                         f"(default: {DEFAULT_SCAN}); '.' or '' means the whole repo")
    ap.add_argument("--write-report", action="store_true",
                    help="write dead_source_report.txt next to this script")
    ap.add_argument("--explain", metavar="GLOB",
                    help="print the evidence lines for candidate files matching GLOB")
    ap.add_argument("--out", default=None, help="report path (default: dead_source_report.txt)")
    args = ap.parse_args()

    raw_root = (args.root or "").strip().rstrip("/")
    SCAN_REL = None if raw_root in ("", ".") else Path(raw_root)

    files = tracked_files()
    if SCAN_REL is None:
        candidates = sorted(p for p in files if p.suffix.lower() in CANDIDATE_EXTS)
    else:
        prefix = str(SCAN_REL) + "/"
        candidates = sorted(p for p in files
                            if p.suffix.lower() in CANDIDATE_EXTS
                            and str(p).startswith(prefix))

    # ---- index the reference sources -------------------------------------
    # Single pass: tokenize each source once and accumulate *inverted* indexes
    # (token -> sources) restricted to the tokens that matter, i.e. the stems of
    # the candidate files.  Without this the check is quadratic over the tree
    # (13k sources x ~900 candidates) and takes minutes.
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
    subst_decls = []       # (makefile rel, set(source items)) for $(VAR:...%.o)

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
                # Object lists synthesised from a source registry
                # ($(ZLIB_FILES:%=$(ZLIB_OBJDIR)/%.o), $(ETHREAD_LIB_SRC:.c=.o))
                # name no object token at all, so resolve the registry back to
                # the sources it names here.
                assigns = list_assignments(txt)
                for m in SUBST_O_RE.finditer(txt):
                    var = m.group(1)
                    if var in assigns:
                        subst_decls.append((rel, set(assigns[var])))
        for stem in candidate_stems.intersection(cnt):
            stem_refs_all.setdefault(stem, []).append(rel)

    # Tokens of the candidate files themselves must be subtractable.
    for c in candidates:
        txt = read_text(c)
        own_tokens[str(c)] = Counter(TOKEN_RE.findall(txt)) if txt else Counter()
        src_text[str(c)] = txt or ""

    def match_scan_rel(rx, c):
        """Glob-match a candidate's path relative to the scan scope."""
        if SCAN_REL is None:
            return False
        try:
            return bool(rx.match(str(c.relative_to(SCAN_REL))))
        except ValueError:
            return False

    def glob_covered(c):
        for _grel, _gpat, rx in globs:
            if rx.match(str(c)) or match_scan_rel(rx, c) or rx.match(c.name):
                return True
        return False

    def object_token_covered(c):
        stem = c.name.rsplit(".", 1)[0]
        for rx in object_res:
            if rx.search(stem + ".o") or rx.search(stem + ".obj"):
                return True
        return False

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
        for _grel, _gpat, rx in globs:
            if rx.match(str(c)) or match_scan_rel(rx, c) or rx.match(c.name):
                build_hits.append(f"{_grel}: $(wildcard {_gpat}) covers this file")
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

    # ===== loose (advisory) tier =========================================
    deletable, exempt, referenced, weak = [], [], [], []
    details = {}
    for c in candidates:
        rel = str(c)
        ev, strong = evidence(c)
        reason = exempt_reason(rel)
        details[rel] = (ev, reason)
        if ev:
            referenced.append(rel)
            if not strong and not reason:
                weak.append((rel, ev))
        elif reason:
            exempt.append((rel, reason))
        else:
            deletable.append(rel)

    # ===== STRICT liveness tier (fixpoint / transitive closure) ==========
    def strict_seed(c):
        """Seed a compilation unit that a real object list / wildcard covers."""
        if c.suffix.lower() in COMPILE_EXTS and (object_token_covered(c) or
                                                glob_covered(c)):
            return True
        return False

    # A synthesised object list ("$(VAR:%=$(OBJDIR)/%.o)", "$(VAR:.c=.o)") is
    # built from VAR's items; resolve each to the candidate source(s) it names.
    stem_to_cand = {}
    for c in candidates:
        stem_to_cand.setdefault(c.name.rsplit(".", 1)[0], []).append(c)
    subst_seeded = set()
    for mk_rel, items in subst_decls:
        mkdir = str(Path(mk_rel).parent)
        for item in items:
            named = [str(x) for x in stem_to_cand.get(item_stem(item), ())]
            if not named:
                continue
            local = [x for x in named if x.startswith(mkdir + "/")]
            subst_seeded.update(local or named)

    # Reverse index: implies[G] = candidates that become live once G is live.
    cand_set = {str(c) for c in candidates}
    cand_by_base = {}
    for c in candidates:
        cand_by_base.setdefault(c.name, []).append(str(c))
    sym_owners = {}
    for c in candidates:
        for s in defined_symbols(c):
            sym_owners.setdefault(s, []).append(str(c))
    all_syms = set(sym_owners)
    implies = {}           # live file -> candidates it makes live
    for base, owners in cand_by_base.items():
        for g in includes.get(base, ()):
            if g in cand_set:
                for owner in owners:
                    if owner != g:
                        implies.setdefault(g, set()).add(owner)
    for g in cand_set:
        for s in set(own_tokens[g]) & all_syms:
            for f in sym_owners[s]:
                if f != g:
                    implies.setdefault(g, set()).add(f)

    live = {str(c) for c in candidates if strict_seed(c)} | subst_seeded
    worklist = deque(sorted(live))
    while worklist:
        g = worklist.popleft()
        for f in implies.get(g, ()):
            if f not in live:
                live.add(f)
                worklist.append(f)

    strict_dead = sorted(rel for rel in (str(c) for c in candidates)
                         if rel not in live)
    strict_dead_action = [rel for rel in strict_dead if exempt_reason(rel) is None]

    def loose_verdict(rel):
        ev, reason = details[rel]
        if ev:
            return "REFERENCED"
        return "EXEMPT: " + reason if reason else "UNREFERENCED"

    def strict_verdict(rel):
        if rel in live:
            return "STRICT LIVE"
        why = exempt_reason(rel)
        return "STRICT DEAD (exempt: " + why + ")" if why else "STRICT DEAD (ACTIONABLE)"

    if args.explain:
        pat = args.explain
        for rel in sorted(details):
            if not fnmatch.fnmatch(rel, pat) and pat not in rel:
                continue
            ev, _reason = details[rel]
            print(f"== {rel}  [{loose_verdict(rel)}] [{strict_verdict(rel)}]")
            for cls, hits in ev.items():
                print(f"   {cls}:")
                for h in hits[:8]:
                    print(f"     - {h}")
            adv = [s for s in stem_refs_all.get(Path(rel).name.rsplit('.', 1)[0], ())
                   if s != rel]
            if adv and not ev:
                print(f"   advisory-only mentions ({len(adv)}): {', '.join(adv[:8])}")
        return 0

    def is_built(c):
        """Is this compilation unit named by an object list (or a wildcard)?"""
        return str(c) in subst_seeded or object_token_covered(c) or glob_covered(c)

    # ===== report ========================================================
    scope_glob = ("**/*.{c,h,cpp,hpp,cc}" if SCAN_REL is None
                  else f"{SCAN_REL}/**/*.{{c,h,cpp,hpp,cc}}")
    scope_desc = ("whole repository" if SCAN_REL is None
                  else f"whole {SCAN_REL}/ tree")
    lines = [f"Dead-source checker report for {scope_glob} — {scope_desc}"]
    lines.append(f"STRICT:  candidates checked: {len(candidates)}   "
                 f"live: {len(live)}   strict-dead: {len(strict_dead)}   "
                 f"strict-dead-not-exempt (ACTIONABLE): {len(strict_dead_action)}")
    lines.append(f"loose (advisory): referenced: {len(referenced)}   "
                 f"exempt (retained by rule): {len(exempt)}   "
                 f"deletable: {len(deletable)}")
    lines.append("")
    lines.append("STRICT DEAD (not built and not reachable from any live file),"
                 " grouped per subdirectory:")
    if not strict_dead:
        lines.append("  (none)")
    else:
        groups = {}
        for rel in strict_dead:
            groups.setdefault(scan_dir_relative(rel), []).append(rel)
        for d in sorted(groups):
            lines.append(f"  {d}/")
            ex = [r for r in groups[d] if exempt_reason(r)]
            ac = [r for r in groups[d] if not exempt_reason(r)]
            lines.append("    exempt:")
            if ex:
                for rel in ex:
                    lines.append(f"      {rel}  <- {exempt_reason(rel)}")
            else:
                lines.append("      (none)")
            lines.append("    ACTIONABLE:")
            if ac:
                for rel in ac:
                    lines.append(f"      {rel}")
            else:
                lines.append("      (none)")
    lines.append("")
    lines.append("=== ADVISORY: loose single-pass tier (NOT the deletion gate) ===")
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
               if Path(rel).suffix.lower() in COMPILE_EXTS
               and not is_built(Path(rel))
               and exempt_reason(rel) is None]
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

    return 1 if strict_dead_action else 0


if __name__ == "__main__":
    sys.exit(main())
