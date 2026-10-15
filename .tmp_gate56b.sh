#!/bin/bash
set -u
DS="$PWD/erts_rust/target/release/differential"
OUT="$TMPDIR/gate56"
cat "$OUT/ids.txt" | xargs -d '\n' -P 16 -I{} sh -c "$DS run '{}' 2>&1 | grep '^summary:' | sed 's|^|{} |'" > "$OUT/labeled.txt" 2>&1
echo "labeled lines: $(wc -l < "$OUT/labeled.txt")"
echo "--- non-agreeing ---"
grep -vE 'agree=1 mismatch=0 unsupported=0 skipped=0' "$OUT/labeled.txt"
