import re,sys,statistics as st
rows={}   # wl -> tier -> [ns]
order=[]
for rep in (1,2,3):
    wl=None
    for line in open(f'.bench/jit_bench_rep{rep}.txt'):
        m=re.search(r'workload = (\S+) ',line)
        if m:
            wl=m.group(1)
            if wl not in rows: rows[wl]={}; order.append(wl)
        m=re.search(r'tier=(\S+)\s+runs=\d+\s+total=\s*[\d.]+ ms \|\s+([\d.]+) ns/op',line)
        if m and wl:
            rows[wl].setdefault(m.group(1),[]).append(float(m.group(2)))
print(f"{'workload':<22}{'interp ns/op':>14}{'baseline ns/op':>16}{'cranelift ns/op':>17}{'base/int':>10}{'cl/int':>9}{'cl/base':>9}")
def med(v): return st.median(v)
summ={}
for wl in order:
    t=rows[wl]
    i,b,c=med(t['interpreter']),med(t['baseline-jit']),med(t['jit-cranelift'])
    summ[wl]=(i,b,c)
    print(f"{wl:<22}{i:>14.1f}{b:>16.1f}{c:>17.1f}{i/b:>10.2f}{i/c:>9.2f}{b/c:>9.2f}")
# geometric means
import math
gi=math.exp(st.mean([math.log(v[0]) for v in summ.values()]))
gb=math.exp(st.mean([math.log(v[1]) for v in summ.values()]))
gc=math.exp(st.mean([math.log(v[2]) for v in summ.values()]))
print(f"\ngeomean ns/op: interp={gi:.1f} baseline={gb:.1f} cranelift={gc:.1f}  -> int/base={gi/gb:.2f}x int/cl={gi/gc:.2f}x base/cl={gb/gc:.2f}x")
# per-rep stability of the ratio cl/base
for rep in (1,2,3):
    wl=None; r={}
    for line in open(f'.bench/jit_bench_rep{rep}.txt'):
        m=re.search(r'workload = (\S+) ',line)
        if m: wl=m.group(1)
        m=re.search(r'tier=(\S+)\s+runs=\d+\s+total=\s*[\d.]+ ms \|\s+([\d.]+) ns/op',line)
        if m and wl: r.setdefault(wl,{})[m.group(1)]=float(m.group(2))
    if all(len(v)==3 for v in r.values()):
        g=lambda k: math.exp(st.mean([math.log(v[k]) for v in r.values()]))
        print(f"rep{rep}: geomean int={g('interpreter'):.0f} base={g('baseline-jit'):.0f} cl={g('jit-cranelift'):.0f} | int/base={g('interpreter')/g('baseline-jit'):.2f} base/cl={g('baseline-jit')/g('jit-cranelift'):.2f}")
