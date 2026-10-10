#!/usr/bin/env python3
# -*- coding: utf-8 -*-
#
# SPDX-License-Identifier: Apache-2.0
#
# Copyright Ericsson AB 2026. All Rights Reserved.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# host_env_xform.py -- rewrite a *scratch copy* of lib/compiler/src so that the
# host erlc (typically an older OTP, e.g. 29.x) can compile and run it.
#
#     Usage:  host_env_xform.py SRC_DIR OUT_DIR
#
# This OTP tree is OTP-30: its sources use the module-namespaced record syntax
# (`-record #Name {..}', `-export_record/1', `-import_record/2', the `#_{anno=V}'
# wildcard and `#rec.field' access, all backed by the OTP-29+ *native record*
# term type).  An older host erlc parses that syntax but resolves
# `-import_record' against the *host's* installed `compiler' application, so the
# resulting beams carry record layouts that disagree with the tree's own
# definitions and the compiler crashes inside its own SSA/records.  Building the
# OTP-30 emulator instead needs autoconf/m4 and is out of scope here.
#
# The rewrites (applied to the scratch copy only -- the repository is never
# written to):
#
#  1. `-record #Name {..}' -> classic `-record(Name, {..})', with the type
#     annotations dropped.  A native record is a *distinct term type* (not a
#     tuple), so after this rewrite every former native record is a plain tuple.
#     That is self-consistent as long as *all* modules are converted the same way
#     (they are), but it also means this rig exercises the compiler's logic, never
#     the native-record mechanism itself.
#  2. `-import_record(Mod, [R1,..])' -> `-include' of a generated header holding
#     exactly the converted definitions of R1,.. taken from Mod's own source.
#     Per-import-site filtering is essential (e.g. beam_core_to_ssa defines its
#     own `ifun' while core_parse.hrl pulls the `c_*' records from v3_core) and is
#     what makes the result independent of the host's installed compiler app.
#  3. `-export_record([..])' -> a comment (nothing to export any more).
#  4. The two constructs whose *semantics* depend on the native-record term type:
#     the `#_{anno=V}' wildcard (WILDCARD below) and arity-1 `is_record/1'
#     (IS_RECORD_1 below), which OTP-29 redefined to test for native records and
#     therefore now returns false for the flattened tuple records.
#
# Any occurrence that is not covered by the rules below aborts the transform, so
# that a source change can never silently produce a corrupt rig.

import os
import re
import shutil
import sys

OPENERS, CLOSERS = "{([", "})]"
RECORD_RE = re.compile(r'-record #(\w+)\s*\{')
IMPORT_RE = re.compile(r'-import_record\((\w+),\s*\[([^\]]*)\]\)\.', re.S)
EXPORT_RE = re.compile(r'-export_record\([^)]*\)\.', re.S)
WILDCARD_UPDATE_RE = re.compile(r'\b([A-Za-z_]\w*)#_\{anno=([^{}]*)\}')
WILDCARD_CLAUSE_RE = re.compile(r'(\b[a-z]\w*\()#_\{anno=([A-Za-z_]\w*)\}\)(\s*->)')


def split_top_level(text):
    """Split `text' on top-level commas (ignoring braces/brackets/parens)."""
    parts, depth, cur = [], 0, ""
    for ch in text:
        if ch in OPENERS:
            depth += 1
        elif ch in CLOSERS:
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur)
            cur = ""
        else:
            cur += ch
    parts.append(cur)
    return parts


def strip_type(field):
    """Cut a record field at its first top-level `::' type annotation."""
    depth = 0
    for i, ch in enumerate(field[:-1]):
        if ch in OPENERS:
            depth += 1
        elif ch in CLOSERS:
            depth -= 1
        elif ch == ":" and field[i + 1] == ":" and depth == 0:
            return field[:i].rstrip()
    return field.rstrip()


def field_name(field):
    return re.split(r"::", field)[0].split("=")[0].strip()


def record_defs(text):
    """Find every `-record #Name {..}' block; return (name, first_line,
    last_line, classic_definition, [field_name]).  Comments inside a block are
    dropped (safe: record definitions contain neither strings nor `%')."""
    lines, out, i = text.split("\n"), [], 0
    while i < len(lines):
        m = RECORD_RE.match(lines[i])
        if not m:
            i += 1
            continue
        depth, j = 0, i
        while j < len(lines):
            code = lines[j].split("%")[0]
            depth += code.count("{") - code.count("}")
            if depth <= 0:
                break
            j += 1
        block = " ".join(l.split("%")[0] for l in lines[i:j + 1])
        body = block[block.index("{") + 1:block.rindex("}")]
        fields = [f for f in (strip_type(f).strip() for f in split_top_level(body)) if f]
        out.append((m.group(1), i, j,
                    "-record(%s, {%s})." % (m.group(1), ", ".join(fields)),
                    [field_name(f) for f in fields]))
        i = j + 1
    return out


def imported_names(text):
    """Record names named by `-import_record(Mod, [..])' in `text'."""
    names = set()
    for m in IMPORT_RE.finditer(text):
        names.update(n.strip() for n in m.group(2).split(",") if n.strip())
    return names


def convert_local_defs(text, defs):
    """Replace in place, computing all line ranges up front (rewrite 1)."""
    lines, out, i, by_start = text.split("\n"), [], 0, {d[1]: d for d in defs}
    while i < len(lines):
        if i in by_start:
            name, _, last, classic, _fields = by_start[i]
            out.append(classic)
            i = last + 1
        else:
            out.append(lines[i])
            i += 1
    return "\n".join(out)


def rewrite_imports(text, base, defs_by_mod, out_dir, own, counter):
    """Replace `-import_record' by an -include of a generated, filtered header
    (rewrite 2).  Records the importing file defines itself are skipped."""
    def repl(m):
        mod, names = m.group(1), [n.strip() for n in m.group(2).split(",") if n.strip()]
        counter[0] += 1
        hdr = "imp_%s_%s_%d.hrl" % (base, mod, counter[0])
        with open(os.path.join(out_dir, hdr), "w") as fh:
            fh.write("%% generated by host_env_xform.py: records imported from %s\n" % mod)
            for name in names:
                if name in own:
                    sys.stderr.write("host_env_xform: %s defines and imports record %s; "
                                     "using the local definition\n" % (base, name))
                elif name in defs_by_mod.get(mod, {}):
                    fh.write(defs_by_mod[mod][name] + "\n")
                else:
                    sys.exit("host_env_xform: %s imports %s from %s, which does not "
                             "define it" % (base, name, mod))
        return '-include("%s").' % hdr

    return IMPORT_RE.sub(repl, text)


# OTP >= 29 redefined `is_record/1' to test for *native* records.  All native
# records have been flattened to tuples above, so every arity-1 is_record/1 must
# be rewritten by hand.  There is no single generic substitute: the two kinds of
# call site want opposite things.
#   * set_anno/2 (v3_core) and set_ann/2 (cerl) assert that their argument is one
#     of our former native records -> is_tuple/1 is exactly right.
#   * beam_ssa_check's two `env_post' clauses handle *literal values that are
#     records* and convert them with records:get_name/1 (a native-record-only
#     API).  With no native records left those clauses are unreachable and must be
#     disabled so that the following tuple clauses do the work.
IS_RECORD_1 = {
    "v3_core.erl": [("set_anno(C, A) when is_record(C)",
                     "set_anno(C, A) when is_tuple(C)")],
    "cerl.erl": [("set_ann(Node, List) when is_record(Node)",
                  "set_ann(Node, List) when is_tuple(Node)")],
    "beam_ssa_check.erl": [("Env) when is_record(Ls) ->", "Env) when false ->"),
                           ("Tuple, Env) when is_record(Tuple) ->",
                            "Tuple, Env) when false ->")],
}


def code_only(text):
    """Drop comments; `%' inside a string literal would also be cut, which can
    only make the checks below miss something and never invent a match."""
    return "\n".join(l.split("%")[0] for l in text.split("\n"))


def check_no_is_record_1(base, text):
    left = re.findall(r"\bis_record\(\s*[A-Za-z_]\w*\s*\)", code_only(text))
    if left:
        sys.exit("host_env_xform: unhandled arity-1 is_record/1 in %s: %s\n"
                 "Add a rule to IS_RECORD_1 (OTP>=29 is_record/1 tests for native "
                 "records, not tuples)." % (base, ", ".join(sorted(set(left)))))


def helper_functions(anno_pos):
    """Dispatch on the record tag instead of the `#_{anno=..}' wildcard.  The
    wildcard matches *any* native record that has an `anno' field, so the global
    tag table is used (a module-visible subset would be stricter than the
    original).  The field is not always the first one (#a has it third,
    #asm_module seventh), hence the per-record element position."""
    get = "".join("        %s -> element(%d, T);\n" % (t, p)
                  for t, p in sorted(anno_pos.items()))
    put = "".join("        %s -> setelement(%d, T, V);\n" % (t, p)
                  for t, p in sorted(anno_pos.items()))
    return ("\n%%--- begin helpers generated by host_env_xform.py ---\n"
            "xform_anno_get(T) ->\n    case element(1, T) of\n" + get +
            "        Other -> erlang:error({not_an_annotated_record, Other})\n    end.\n"
            "xform_anno_set(T, V) ->\n    case element(1, T) of\n" + put +
            "        Other -> erlang:error({not_an_annotated_record, Other})\n    end.\n")


def rewrite_wildcards(text):
    """`Rec#_{anno=V}' -> xform_anno_set(Rec, V); `f(#_{anno=V}) ->' becomes
    `f(XformAnnoRec) -> V = xform_anno_get(XformAnnoRec),' (rewrite 4a)."""
    text = WILDCARD_UPDATE_RE.sub(r"xform_anno_set(\1, \2)", text)
    text = WILDCARD_CLAUSE_RE.sub(r"\1XformAnnoRec)\3 \2 = xform_anno_get(XformAnnoRec),",
                                  text)
    left = code_only(text).count("#_{")
    if left:
        sys.exit("host_env_xform: %d unhandled #_{..} wildcard record(s) remain;\n"
                 "extend rewrite_wildcards() (only a `f(#_{anno=V}) ->' clause head "
                 "and `Rec#_{anno=V}' are handled)" % left)
    return text


def main(src_dir, out_dir):
    if os.path.exists(out_dir):
        shutil.rmtree(out_dir)
    os.makedirs(out_dir)
    files = sorted(f for f in os.listdir(src_dir) if f.endswith((".erl", ".hrl")))
    sources = {f: open(os.path.join(src_dir, f)).read() for f in files}

    defs_by_mod, own_defs, anno_pos = {}, {}, {}
    for f in files:
        defs = record_defs(sources[f])
        own_defs[f] = {d[0]: d[3] for d in defs}
        for name, _s, _e, _classic, fields in defs:
            if "anno" in fields:
                # classic tuple: field n is element n+1
                anno_pos[name] = fields.index("anno") + 2
        m = re.search(r"-module\((\w+)\)", sources[f])
        if m:
            defs_by_mod[m.group(1)] = own_defs[f]

    counter = [0]
    rewritten = 0
    for f in files:
        base = os.path.splitext(f)[0]
        text = convert_local_defs(sources[f], record_defs(sources[f]))
        text = rewrite_imports(text, base, defs_by_mod, out_dir, own_defs[f], counter)
        text = EXPORT_RE.sub("%% export_record dropped by host_env_xform.py", text)
        text = rewrite_wildcards(text)
        for old, new in IS_RECORD_1.get(f, []):
            if old not in text:
                sys.exit("host_env_xform: expected %r in %s" % (old, f))
            text = text.replace(old, new)
        check_no_is_record_1(f, text)
        if "xform_anno_get(" in text or "xform_anno_set(" in text:
            text = text.rstrip("\n") + "\n" + helper_functions(anno_pos)
            rewritten += 1
        with open(os.path.join(out_dir, f), "w") as fh:
            fh.write(text)
    print("host_env_xform: %d files -> %s (%d import headers, %d wildcard-helper "
          "modules, %d anno-bearing record tags)"
          % (len(files), out_dir, counter[0], rewritten, len(anno_pos)))


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
