#!/bin/bash
set -u
DS="$PWD/erts_rust/target/release/differential"
OUT="$TMPDIR/gate56"
mkdir -p "$OUT"
"$DS" list 2>/dev/null | sed 's/ —.*//' > "$OUT/ids.txt"
echo "ids: $(wc -l < "$OUT/ids.txt")"
date +%s > "$OUT/t0"
cat "$OUT/ids.txt" | xargs -d '\n' -P 16 -I ID "$DS" run ID > "$OUT/all.txt" 2>&1
echo "sweep rc=$? elapsed=$(( $(date +%s) - $(cat "$OUT/t0") ))s"
echo "--- summary totals ---"
grep -o 'summary: .*' "$OUT/all.txt" | awk '
{ for (i=1;i<=NF;i++) { split($i,a,"="); if (a[1]=="total") t+=a[2]; if (a[1]=="agree") g+=a[2]; if (a[1]=="mismatch") m+=a[2]; if (a[1]=="unsupported") u+=a[2]; if (a[1]=="skipped") s+=a[2]; } }
END { printf "total=%d agree=%d mismatch=%d unsupported=%d skipped=%d\n", t,g,m,u,s }'
echo "--- non-agreeing ---"
grep -B3 -E 'summary: .*mismatch=[1-9]|summary: .*unsupported=[1-9]|summary: .*skipped=[1-9]' "$OUT/all.txt" | grep -E 'rust:|summary:|case:|^differential:' | head -40
