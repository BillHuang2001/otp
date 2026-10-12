#!/usr/bin/env python3
# %CopyrightBegin%
# SPDX-License-Identifier: Apache-2.0
#
# Mechanical dead-source checker for the legacy ERTS C/C++ tree.
#
# WHY THIS EXISTS
#   The legacy C/C++ ERTS tree is the OTP build's default runtime and the
#   differential oracle for the erts_rust/ rewrite, so it may only shrink by files
#   that are PROVABLY unreferenced.  A full ERTS compile is far too expensive to
#   use as the bar, so this script produces the mechanical reference proof.  A
#   false "dead" verdict is the one failure mode that matters, so every reference
#   class errs towards KEEPING a file.
#
# WHAT IT PROVES (decisive reference classes)
#   A  BUILD/SCRIPT REFERENCE - a real build or script source names the file:
#      the basename appears VERBATIM ("foo_drv.c") in any build/script source
#      (.sh, .erl/.hrl, .escript, .nix, .toml, .yml/.yaml, Makefile*, *.mk/*.in/
#      *.src/*.spec/*.rel/*.def/*.cmake/*.cfg/*.pro, *.ac/*.m4, configure,
#      otp_build, CI yaml, xcomp/*.conf, the extension-less codegen tools in
#      erts/emulator/utils/, *.S, ...), or its stem appears as a whole word in a
#      makefile-role source, or a $(wildcard ...) pattern matches the path.
#   B  INCLUDE REFERENCE - an #include of the basename (or a path ending in it)
#      in any other C-family source.
#   C  SYMBOL REFERENCE - an identifier the file defines (function definition,
#      #define, typedef name) appears as a whole word in any other C-family or
#      build/config source.
#   Only whole-word matches count, so the stem "bin_drv" is NOT "referenced" by
#   the unrelated identifier "erts_bin_drv_alloc".  Files whose own stem is
#   generic (hash.c/time.c) are kept merely because the word occurs somewhere.
#
# ADVISORY (printed, never decisive)
#   A mention in any other tracked text file (docs, CONTEXT.md, erts_rust
#   sources, ...).  That is not a build/include/symbol reference, so it does not
#   make a file live - it is reported only so a human can see why a grep "found"
#   the name.
#
# TWO TIERS
#   STRICT (decisive)   A fixpoint / transitive-closure liveness analysis over the
#                       build wiring.  A file is LIVE if it is
#                         (a) a compilation unit named by a real object-list entry
#                             - every make synthesis form is resolved back to the
#                             sources it names:
#                               $(VAR:%=$(OBJDIR)/%.o) / $(VAR:.c=.o)
#                               $(patsubst %.c,$(OBJDIR)/%.o,$(VAR))
#                               $(VAR:%=prefix/%.c) / $(addprefix D,$(notdir ...
#                             - or covered by a $(wildcard ...) pattern, or
#                         (b) named by a build/script/codegen source (class A), or
#                         (c) a driver/NIF CT fixture owned by its *_SUITE.erl, or
#                         (d) reached from a live file through an #include of its
#                             basename or through a symbol it defines.
#                       Everything else is STRICT DEAD.  Only candidate files
#                       propagate liveness, so a reference that comes solely from
#                       a DOC does NOT make a file live, and neither does text
#                       inside a never-compiled #if region (advisory section).
#                       The deletion gate is "STRICT DEAD and not exempt" = 0.
#   LOOSE (advisory)    The original single-pass A/B/C reference classes plus the
#                       per-file exemptions; NOT the deletion gate.
#
# NEVER-COMPILED #if REGIONS (advisory report section, never decisive)
#   `#ifdef M` / `#if defined(M)` regions whose controlling macro M is never
#   definable in this tree are reported as `file:line` + macro.  "Never
#   definable" means, conservatively, ALL of:
#     - M is not #define'd (or #undef'd) anywhere, even mid-line by a generator or
#       in a commented-out `/* #define M */` enable path, and not a -D flag or an
#       AC_DEFINE (configure.ac/xcomp) or a config.h.in template entry,
#     - M is not a compiler/toolchain builtin or platform name ("_" / "__" names,
#       HAVE_*, ERLANG_OSTYPE, DEBUG*, ETHR_*, AO_*, ZLIB*, PCRE*, ASMJIT*, ...),
#     - M occurs in the tree only on preprocessor condition lines - a *use* in a
#       C-family or build/config source would rule it out, but a mention in a doc
#       or in a Rust comment is not a use and is ignored.
#   Anything not decidable that way stays SILENT (no false positives), and a
#   condition that is not a single macro (arithmetic, &&/||, function-like) is
#   never reported.  `#ifndef M` with such an M is ALWAYS compiled, so its body is
#   not a dead region.  These findings NEVER change the strict/actionable verdict
#   or the exit status; a removal pass must still verify each block against the
#   toolchain and the configure options.
#
# USAGE
#   python3 check_dead_sources.py [--root DIR] [--write-report] [--explain GLOB]
#   --root defaults to `erts`; "." means the whole repository.  Exit status is 1
#   when the ACTIONABLE STRICT-dead set is non-empty, else 0.
#   Scope: every tracked <root>/**/*.{c,h,cpp,hpp,cc}; the default root is `erts`.

import argparse
import fnmatch
import os
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
# Assembly inputs are build sources too (they are compiled, not merely data).
ASM_EXTS = {".s"}

# Never a reference source: markdown/doc files and this tool's own output (which
# lists the very names we are testing for).
DOC_EXTS = {".md", ".markdown", ".rst", ".txt"}
SELF_EXCLUDE = {"erts/emulator/beam/check_dead_sources.py",
                "erts/emulator/beam/dead_source_report.txt"}

# Directories whose files are build/codegen inputs even without a recognised
# suffix: the codegen tools (beam_makeops, make_tables, make_preload, ...) are
# EXTENSION-LESS, erts/preloaded/src/ holds add_abstract_code + the preloaded
# *.erl, and xcomp/ holds the cross-compilation descriptions (*.conf/.template).
CODEGEN_DIRS = ("erts/emulator/utils/", "erts/preloaded/src/")
XCOMP_PREFIX = "xcomp/"

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

# Object lists synthesised from a source registry - the object list names no
# object token at all, so the registry variable must be resolved back to the
# sources it names.
#   $(VAR:%=$(OBJDIR)/%.o) / $(VAR:.c=.o)        - zlib's ZLIB_FILES, lib_src's
#                                                  ETHREAD_LIB_SRC, ...
SUBST_O_RE = re.compile(r"\$\(([A-Za-z_]\w*)\s*:((?:[^()\n]|\$\([^)\n]*\))*?\.o)\)")
#   $(patsubst %.c,$(OBJDIR)/%.o,$(VAR))         - ASMJIT_OBJS over ASMJIT_CPP
PATSUBST_O_RE = re.compile(r"\$\(\s*patsubst\s+%\.(?:c|cpp|cc)\s*,\s*"
                           r"(?:[^,()\n]|\$\([^)\n]*\))*?%\.o\s*,\s*\$\(([A-Za-z_]\w*)\)\s*\)")
#   $(VAR:%=prefix/%.c)                          - ryu's RYU_SRC, zlib's ZLIB_SRC
REGISTRY_SRC_RE = re.compile(r"\$\(\s*([A-Za-z_]\w*)\s*:\s*%=\s*([^)\n]*?%\.(?:c|cpp|cc))\s*\)")
#   $(addprefix $(OBJDIR)/,$(notdir $(VAR:.c=.o))) - ERTS_INTERNAL_LIB_OBJS
ADDPREFIX_NOTDIR_RE = re.compile(r"\$\(\s*addprefix\s+[^,()\n]*,\s*\$\(\s*notdir\s+"
                                 r"\$\(\s*([A-Za-z_]\w*)\s*:\s*\.c\s*=\s*\.o\s*\)\s*\)\s*\)")
MAKE_ASSIGN_RE = re.compile(r"^\s*([A-Za-z_]\w*)\s*[:+]?=\s*(.*)$")
# One item of a make variable that names sources: a bare object stem ("adler32")
# or a source file, possibly reached through a make variable
# ("$(ETHR_THR_LIB_BASE_DIR)/ethr_event.c").
SRC_TOKEN_RE = re.compile(r"^(?:[A-Za-z_][\w.\-]*|[^\s\\]*\.(?:c|cpp|cc))$")

# A preprocessor condition/undef line.  `#else`/`#endif` are included: a name on
# such a line can only be a comment (`#endif /* NAME */`), never a use.  Used to
# prove that a macro name occurs ONLY as a condition (i.e. is neither defined nor
# used by any code).
COND_LINE_RE = re.compile(
    r"^[ \t]*#[ \t]*(?:if|ifdef|ifndef|elif|else|endif|undef)[^\n]*$", re.M)
# re.M is REQUIRED: this regex is used both per-line (find_never_compiled_blocks)
# and over a whole file text (Pass A's `finditer`, which must see every line).
PP_LINE_RE = re.compile(r"^\s*#\s*(if|ifdef|ifndef|elif|else|endif)\b[ \t]*(.*?)[ \t]*$", re.M)
IDENT_RE = re.compile(r"^[A-Za-z_]\w*$")
# Every spelling that defines/undeclares a macro anywhere in the tree.  The
# `#define`/`#undef` one is deliberately UNANCHORED: generators print their
# `#define NAME` (and commented-out/disabled toggles spell it) mid-line, and a
# macro emitted by a code generator IS definable here.
DEF_NAME_RES = (re.compile(r"#[ \t]*(?:define|undef)[ \t]+([A-Za-z_]\w*)"),
                re.compile(r"(?<![\w-])-D\s*([A-Za-z_]\w*)"),
                re.compile(r"AC_DEFINE(?:_UNQUOTED)?\([ \t]*([A-Za-z_]\w*)"))

# Conservative allow-list for the never-compiled-block detector: names in the
# compiler/platform/toolchain namespaces can always be defined outside the
# tracked sources (command line, compiler builtin, system/3PP headers), so a
# region guarded by them is never reported.
KNOWN_MACROS = {
    "unix", "linux", "WIN32", "APPLE", "DARWIN", "POSIX", "VXWORKS", "SOLARIS",
    "FREEBSD", "OPENBSD", "NETBSD", "_REENTRANT", "NDEBUG", "DEBUG", "ERLANG_OSTYPE",
}
MACRO_BUILTIN_PREFIXES = ("DEBUG", "NDEBUG", "ETHR_", "AO_", "EI_", "ZLIB", "PCRE", "ASMJIT", "Z_")

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


def is_codegen_source(path):
    """Extension-less codegen tools and other build inputs that a plain suffix
    test misses (erts/emulator/utils/beam_makeops, .../make_tables,
    erts/preloaded/src/add_abstract_code)."""
    s = str(path)
    if s.startswith(CODEGEN_DIRS):
        return True
    return s.startswith(XCOMP_PREFIX)


def is_makefile_source(path):
    """Sources that wire files into a build (object lists, wildcards, CI)."""
    name = path.name
    if name.startswith("Makefile") or name in (
            "otp_build", "otp_patch_apply", "erl-build-tool-vars.sh", "configure"):
        return True
    if is_codegen_source(path):
        return True
    if path.suffix.lower() in ASM_EXTS:
        return True
    return path.suffix.lower() in {".mk", ".in", ".m4", ".ac", ".am", ".tab",
                                  ".src", ".spec", ".rel", ".def", ".cmake",
                                  ".cfg", ".pro", ".bat", ".conf", ".template"}


def is_build_source(path):
    """Superset of is_makefile_source: also scripts/suites that name a source
    file directly (a CT suite opening "dead_port.c", a CI script, ...)."""
    if is_makefile_source(path):
        return True
    if path.name.lower() in ("configure.ac", "configure.src"):
        return True
    return path.suffix.lower() in {".sh", ".nix", ".toml", ".exs", ".escript",
                                  ".erl", ".hrl", ".yml", ".yaml"}


def guard_relevant(rel):
    """Is a *use* of a guard name in `rel` real preprocessor evidence?

    A preprocessor use can only live in a C-family source or in a build/config
    source (where -D/AC_DEFINE spell it).  A mention in a doc (CONTEXT.md, ...)
    or in a comment of the Rust rewrite is not a use, so it must NOT defeat the
    never-compiled-block detector.
    """
    p = Path(rel)
    return p.suffix.lower() in CFAMILY_EXTS or is_build_source(p)

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
    Makefile directory the pattern was written in.  A make `%` is treated as a
    wildcard too (source-naming registries like $(RYU_FILES:%=ryu/%.c)).
    """
    pat = re.sub(r"\$\([^)]*\)|\$\{[^}]*\}", "", pattern).strip().replace("%", "*")
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


def cond_text(txt):
    """Only the preprocessor condition/undef lines of `txt`."""
    return "".join(m.group(0) for m in COND_LINE_RE.finditer(txt))


def defined_names(txt):
    """Every macro name `txt` defines/undeclares (#define / #undef / -D / AC_DEFINE).

    Unanchored, so a generator-emitted or commented-out `#define NAME`, and a
    `-DNAME` (which the tokenizer would otherwise see as the word "DNAME"), all
    count as definable.
    """
    return {n for rx in DEF_NAME_RES for n in rx.findall(txt)}


def cheap_undefinable(name):
    """Name-only filter of the never-compiled-block detector (no tree data).

    Names in the compiler/platform/toolchain namespaces can always be defined
    outside the tracked sources (command line, compiler builtin, system or 3PP
    headers), so a region guarded by them is never reported - and this filter
    also keeps the whole-tree work limited to the names that can matter.
    """
    if name in KNOWN_MACROS or name in C_KEYWORDS:
        return False
    if name.startswith("_") or "__" in name or "HAVE" in name.split("_"):
        return False
    return not name.startswith(MACRO_BUILTIN_PREFIXES)


def single_macro_condition(kind, rest):
    """(macro, negated) for a simple single-macro condition, else (None, False).

    Anything else (arithmetic, &&/||, function-like, several identifiers) is
    undecidable and must stay SILENT.
    """
    rest = rest.strip()
    if kind in ("ifdef", "ifndef"):
        if IDENT_RE.match(rest):
            return rest, kind == "ifndef"
        return None, False
    m = re.match(r"^(!?)\s*defined\s*(?:\(\s*([A-Za-z_]\w*)\s*\)|([A-Za-z_]\w*))$", rest)
    if m:
        return (m.group(2) or m.group(3)), bool(m.group(1))
    if IDENT_RE.match(rest):
        return rest, False
    return None, False


def find_never_compiled_blocks(txt, undefinable):
    """Never-compiled `#if` regions of `txt`.

    Returns [{"macro", "start", "end"}] where start/end are 1-based line numbers
    of the `#if` directive and of the last line of its BODY (the directive line
    and the #endif/#else line are not part of the body).  Only a positive
    single-macro condition counts; `#ifndef M`/`#if !defined(M)` with such an M
    is always compiled and is therefore not reported, and an undecidable
    condition is skipped.  Nested regions inside an already-reported region are
    not reported again.
    """
    lines = txt.splitlines()
    frames = []                 # [[region or None, region_closed]]
    out = []
    for idx, line in enumerate(lines, 1):
        m = PP_LINE_RE.match(line)
        if not m:
            continue
        kind, rest = m.group(1), m.group(2)
        if kind in ("if", "ifdef", "ifndef"):
            macro, negated = single_macro_condition(kind, rest)
            region = None
            if (macro and not negated and not any(f[0] is not None for f in frames)
                    and undefinable(macro)):
                region = {"macro": macro, "start": idx, "end": idx}
                out.append(region)
            frames.append([region, False])
        elif kind in ("else", "elif"):
            if frames and frames[-1][0] is not None and not frames[-1][1]:
                frames[-1][0]["end"] = idx - 1      # alternative branch is compiled
                frames[-1][1] = True
        elif kind == "endif":
            if not frames:
                continue
            region, closed = frames.pop()
            if region is not None and not closed:
                region["end"] = idx - 1
    return [r for r in out if r["end"] >= r["start"] + 1]


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
    cand_set = {str(c) for c in candidates}
    src_text = {}          # candidate rel -> text (the ACTIVE text, see below)
    cand_text = {}         # candidate rel -> verbatim text
    own_tokens = {}        # candidate rel -> Counter of its own tokens
    live_tokens = {}       # own tokens minus never-compiled-block tokens
    globs = []             # (pattern_source_path, pattern, regex)
    tokens_all = Counter()
    tokens_cfamily = Counter()
    tokens_build = Counter()
    stem_refs_all = {}     # stem -> list of sources mentioning it
    stem_refs_build = {}   # stem -> list of *build* sources mentioning it
    stem_refs_make = {}    # stem -> list of *makefile* sources mentioning it
    basename_refs = {}     # "x.c" -> list of build sources mentioning it verbatim
    includes = {}          # basename -> list of "path" that #include it
    suite_tokens = {}      # "<x>_SUITE.erl" -> Counter (CT fixture ownership)
    object_res = []        # regex matching object basenames listed in a Makefile
    subst_decls = []       # (makefile rel, set(source items)) for synthesised lists
    guard_names = set()    # macro names used on #if conditions of candidates
    defined_macros = set() # every macro name defined anywhere in the tree
    guard_ok = {}          # guard name -> True iff used only on condition lines

    # Pass A - the candidate files: their text is needed by both tiers and to
    # decide which of their preprocessor regions can never be compiled.
    # guard_names only needs the macros that could be *reported*, i.e. the
    # controlling macro of a simple POSITIVE `#if`/`#ifdef`.
    cand_cnt = {}
    cand_by_base = {}
    for c in candidates:
        rel = str(c)
        txt = read_text(c) or ""
        cand_text[rel] = txt
        cand_cnt[rel] = Counter(TOKEN_RE.findall(txt))
        cand_by_base.setdefault(c.name, []).append(rel)
        for m in PP_LINE_RE.finditer(txt):
            if m.group(1) in ("if", "ifdef", "ifndef"):
                name, negated = single_macro_condition(m.group(1), m.group(2))
                if name and not negated:
                    guard_names.add(name)

    basename_re = re.compile(r"([A-Za-z_][\w.\-]*\.(?:c|h|cpp|hpp|cc|tab|inc|def))\b")
    obj_token_re = re.compile(r"([A-Za-z_][\w.\-]*\.(?:o|obj))\b")
    # Everything below is restricted to the names that could ever be reported,
    # so the whole-tree work stays a single pass: erts/*_HAVE_* / __*/_* / 3PP
    # namespaces are pruned by name alone, and only the files that mention one
    # of the surviving names are inspected in detail.  Pass B answers, for every
    # such name, "is it ever used outside a `#if` line?" and "is it ever
    # defined?" from the token counts it already computes - one pass over the
    # tracked files, no per-pattern tree grep (git grep -f over ~1000 patterns
    # degenerates to minutes).
    interesting_guards = {n for n in guard_names if cheap_undefinable(n)}

    def maybe_scan_basenames(txt):
        # Exact prefilter for basename_re (which needs one of these suffixes).
        return (".c" in txt or ".h" in txt or ".tab" in txt or ".inc" in txt
                or ".def" in txt)

    # Pass B - every tracked file.
    for p in files:
        rel = str(p)
        if rel in SELF_EXCLUDE:
            continue
        is_doc = p.suffix.lower() in DOC_EXTS
        txt = cand_text.get(rel)
        if txt is None:
            try:
                if (ROOT / p).stat().st_size > 4_000_000:
                    continue
            except OSError:
                continue
            txt = read_text(p)
            if txt is None:
                continue
        # A macro name is "defined" if ANY tracked text file spells
        # `#define`/`#undef`/`-D`/`AC_DEFINE` for it - scanned with the raw text
        # because `-DNAME` tokenizes as "DNAME", searched ANYWHERE in the line so
        # a generator-emitted or commented-out(disabled) `#define NAME` counts,
        # and covering docs too (this check errs towards "definable").
        if interesting_guards and ("#define" in txt or "#undef" in txt
                                   or "-D" in txt or "AC_DEFINE" in txt):
            for name in interesting_guards.intersection(defined_names(txt)):
                defined_macros.add(name)
                if guard_relevant(rel):
                    guard_ok[name] = False    # a definition is not a condition
        if is_doc:
            continue
        cnt = cand_cnt.get(rel)
        if cnt is None:
            cnt = Counter(TOKEN_RE.findall(txt))
        is_c = p.suffix.lower() in CFAMILY_EXTS
        is_b = is_build_source(p)
        is_m = is_makefile_source(p)
        tokens_all.update(cnt)
        if is_c:
            tokens_cfamily.update(cnt)
            if rel not in cand_set:
                # Candidate #includes are read from their ACTIVE text below.
                for m in INCLUDE_RE.finditer(txt):
                    includes.setdefault(m.group(1).rsplit("/", 1)[-1], []).append(rel)
        # "Used" means a guard name occurs in a C-family/build source somewhere
        # other than a `#if`/`#else`/`#endif`/`#undef` line (compared by token
        # count); a mention in a doc or in a Rust comment is not a use.
        if interesting_guards and guard_relevant(rel):
            g_hits = interesting_guards.intersection(cnt)
            if g_hits:
                gcnt = Counter(TOKEN_RE.findall(cond_text(txt)))
                for name in g_hits:
                    if cnt[name] != gcnt.get(name, 0):
                        guard_ok[name] = False    # also used by real code
                    else:
                        guard_ok.setdefault(name, True)
        if is_b:
            tokens_build.update(cnt)
            stems = candidate_stems.intersection(cnt)
            for stem in stems:
                stem_refs_build.setdefault(stem, []).append(rel)
                if is_m:
                    stem_refs_make.setdefault(stem, []).append(rel)
            if stems or maybe_scan_basenames(txt):
                for m in basename_re.finditer(txt):
                    base = m.group(1)
                    if base in cand_by_base:
                        basename_refs.setdefault(base, []).append(rel)
            if is_m:
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
                #   $(VAR:%=$(OBJDIR)/%.o) | $(VAR:.c=.o)
                #   $(patsubst %.c,$(OBJDIR)/%.o,$(VAR))
                #   $(addprefix D,$(notdir $(VAR:.c=.o)))
                # name no object token at all, so resolve the registry back to
                # the sources it names here.
                assigns = list_assignments(txt)
                for rx in (SUBST_O_RE, PATSUBST_O_RE, ADDPREFIX_NOTDIR_RE):
                    for m in rx.finditer(txt):
                        var = m.group(1)
                        if var in assigns:
                            subst_decls.append((rel, set(assigns[var])))
                #   $(VAR:%=prefix/%.c) - the reverse, source-naming form.
                for m in REGISTRY_SRC_RE.finditer(txt):
                    var, pat = m.group(1), m.group(2)
                    if var not in assigns:
                        continue
                    rx = glob_to_regex(pat)
                    if rx:
                        globs.append((rel, pat, rx))
        if rel.endswith("_SUITE.erl"):
            suite_tokens[rel] = cnt
        for stem in candidate_stems.intersection(cnt):
            stem_refs_all.setdefault(stem, []).append(rel)

    # ---- never-compiled #if regions (advisory findings) -------------------
    def macro_undefinable(name):
        """Conservatively "can never be defined here" (see the header comment)."""
        if not cheap_undefinable(name) or name in defined_macros:
            return False
        # Provable only if no source defines *or uses* the name outside #if lines.
        return guard_ok.get(name) is True

    blocks = {}            # candidate rel -> [(line, macro)]
    dead_lines = {}        # candidate rel -> set(1-based dead body line numbers)
    for c in candidates:
        rel = str(c)
        txt = cand_text[rel]
        if "#if" not in txt:
            continue
        found = find_never_compiled_blocks(txt, macro_undefinable)
        if not found:
            continue
        blocks[rel] = [(b["start"], b["macro"]) for b in found]
        dead = set()
        for b in found:
            dead.update(range(b["start"] + 1, b["end"] + 1))
        dead_lines[rel] = dead

    # ---- the active text ---------------------------------------------------
    # A reference that lives only inside a never-compiled block must not make a
    # file live, so candidate liveness evidence is taken from the text with
    # those bodies removed.
    for c in candidates:
        rel = str(c)
        own_tokens[rel] = cand_cnt[rel]
        dead = dead_lines.get(rel)
        if dead:
            parts, dead_parts = [], []
            for i, line in enumerate(cand_text[rel].splitlines(keepends=True), 1):
                (dead_parts if i in dead else parts).append(line)
            src_text[rel] = "".join(parts)
            live_tokens[rel] = own_tokens[rel] - Counter(TOKEN_RE.findall("".join(dead_parts)))
        else:
            src_text[rel] = cand_text[rel]
            live_tokens[rel] = own_tokens[rel]
        if c.suffix.lower() in CFAMILY_EXTS:
            for m in INCLUDE_RE.finditer(src_text[rel]):
                includes.setdefault(m.group(1).rsplit("/", 1)[-1], []).append(rel)

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

    def build_ref_seed(c):
        """Decisive class-A reference from a build/script/codegen source."""
        base, stem = c.name, c.name.rsplit(".", 1)[0]
        if basename_refs.get(base):
            return f"'{base}' named verbatim in {basename_refs[base][0]}"
        if stem_refs_make.get(stem):
            return f"stem '{stem}' named in {stem_refs_make[stem][0]}"
        return None

    def fixture_seed(c):
        """Driver/NIF CT fixture: built by its own Makefile and/or loaded by
        string name from the owning *_SUITE.erl - neither is an object list."""
        rel = str(c)
        i = rel.rfind("_SUITE_data/")
        if i < 0:
            return None
        cnt = suite_tokens.get(rel[:i] + "_SUITE.erl")
        if cnt is None:
            return None
        stem, base = c.name.rsplit(".", 1)[0], c.name
        if stem in cnt or base in cnt:
            return f"CT fixture named by {rel[:i]}_SUITE.erl"
        return None

    # ---- decide ----------------------------------------------------------
    def defined_symbols(c):
        """Externally visible symbols only.

        A translation unit's own static functions, file-local macros and local
        typedefs are invisible to the rest of the program, so a same-named
        symbol elsewhere is NOT evidence that this file is reachable (that is
        exactly the "bin_drv.c defines static read_fill()" false positive).
        Headers, in contrast, are included wholesale, so everything they define
        counts.  Symbols that exist only inside a never-compiled #if region are
        not defined either - they cannot make their users live.
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
        build-source mentions (an .erl suite, a CI script, a .def list) that are
        not a verbatim basename / whole-word stem reference.
        """
        rel, base, stem = str(c), c.name, c.name.rsplit(".", 1)[0]
        own_all = own_tokens.get(rel, Counter())
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

    # A synthesised object list ("$(VAR:%=$(OBJDIR)/%.o)", "$(VAR:.c=.o)",
    # "$(patsubst %.c,$(OBJDIR)/%.o,$(VAR))", "$(addprefix ... $(VAR:.c=.o))")
    # is built from VAR's items; resolve each to the candidate source(s) it
    # names, preferring a source in the directory of the consuming Makefile.
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

    seed_why = {}
    for c in candidates:
        rel = str(c)
        if strict_seed(c):
            seed_why[rel] = "object list / $(wildcard)"
        elif rel in subst_seeded:
            seed_why[rel] = "synthesised object list (registry resolved)"
        else:
            why = build_ref_seed(c) or fixture_seed(c)
            if why:
                seed_why[rel] = why

    # Reverse index: implies[G] = candidates that become live once G is live.
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
        for s in set(live_tokens.get(g, ())) & all_syms:
            for f in sym_owners[s]:
                if f != g:
                    implies.setdefault(g, set()).add(f)

    live = set(seed_why)
    worklist = deque(sorted(live))
    why_live = dict(seed_why)
    while worklist:
        g = worklist.popleft()
        for f in implies.get(g, ()):
            if f not in live:
                live.add(f)
                why_live[f] = f"reachable from {g}"
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
            if rel in live:
                print(f"   strict seed/reachability: {why_live.get(rel, '?')}")
            for cls, hits in ev.items():
                print(f"   {cls}:")
                for h in hits[:8]:
                    print(f"     - {h}")
            for line, macro in blocks.get(rel, ()):
                print(f"   never-compiled block: {rel}:{line}  #ifdef {macro}")
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
    lines.append("=== ADVISORY: never-compiled #if regions (NOT part of the deletion gate) ===")
    lines.append("A region `#ifdef M` / `#if defined(M)` is listed when M can never be defined here:")
    lines.append("no #define/#undef (even mid-line, generator-emitted or commented-out), no -D, no")
    lines.append("AC_DEFINE / config.h.in entry, not a compiler/platform builtin name, and every")
    lines.append("other occurrence of M is a preprocessor condition line (a doc or Rust-comment")
    lines.append("mention is not a use).  Undecidable conditions and `#ifndef M` (always compiled)")
    lines.append("are not reported.  These findings never affect the STRICT/ACTIONABLE verdict or")
    lines.append("the exit status; a removal pass must still verify each block against the toolchain.")
    if not blocks:
        lines.append("  (none)")
    else:
        n_blocks = sum(len(v) for v in blocks.values())
        lines.append(f"  regions: {n_blocks} in {len(blocks)} files")
        for rel in sorted(blocks):
            for line, macro in blocks[rel]:
                lines.append(f"    {rel}:{line}  guard macro '{macro}' is never definable here")
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
    lines.append("WEAK (retained, but the only loose reference is a non-makefile mention -")
    lines.append("      neither a verbatim basename nor a makefile-role stem; audit before deleting):")
    for rel, ev in weak:
        hits = ev.get("A build ref", [])
        lines.append(f"  {rel}  <- {hits[0] if hits else '?'}")
    lines.append("")
    lines.append("NOT IN AN OBJECT LIST (compilation units no object-list entry or $(wildcard)")
    lines.append("covers; they may still be STRICT LIVE via a script/codegen/fixture reference -")
    lines.append("check the STRICT verdict before deleting):")
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
