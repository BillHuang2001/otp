M = 0xFFFFFFFF
HCONST = 0x9e3779b9
def mix(a,b,c):
    a=(a-b)&M; a=(a-c)&M; a^= (c>>13)
    b=(b-c)&M; b=(b-a)&M; b^= ((a<<8)&M)
    c=(c-a)&M; c=(c-b)&M; c^= (b>>13)
    a=(a-b)&M; a=(a-c)&M; a^= (c>>12)
    b=(b-c)&M; b=(b-a)&M; b^= ((a<<16)&M)
    c=(c-a)&M; c=(c-b)&M; c^= (b>>5)
    a=(a-b)&M; a=(a-c)&M; a^= (c>>3)
    b=(b-c)&M; b=(b-a)&M; b^= ((a<<10)&M)
    c=(c-a)&M; c=(c-b)&M; c^= (b>>15)
    return c
def h2(hash_, e1, e2, con):
    a=(con+e1)&M; b=(con+e2)&M
    return mix(a,b,hash_)
def h1(hash_, e, con): return h2(hash_, e, 0, con)
def sint32(hash_, y, con):
    if y < 0: hash_ = h1(hash_, -y, con)
    return h1(hash_, y, con)
def not_ssmall28(hash_, s):
    if s < 0: con = 0x2e2ac13a; t = (-s) & 0xFFFFFFFFFFFFFFFF
    else: con = 0xcc623af3; t = s
    x = t & M; y = (t>>32) & M
    return h2(hash_, x, y, con)
def is_ssmall28(x): return (( (x >> 27) + 1) & M) < 2

def block_hash(buf, initval):
    a=HCONST; b=HCONST; c=initval
    n=len(buf)
    noloop = n % 12
    looplen = n - noloop
    k=0
    while k < looplen:
        a=(a+ (buf[k] | (buf[k+1]<<8) | (buf[k+2]<<16) | (buf[k+3]<<24)))&M
        b=(b+ (buf[k+4] | (buf[k+5]<<8) | (buf[k+6]<<16) | (buf[k+7]<<24)))&M
        c=(c+ (buf[k+8] | (buf[k+9]<<8) | (buf[k+10]<<16) | (buf[k+11]<<24)))&M
        c=mix(a,b,c)
        k+=12
    fin = buf[looplen:]
    c=(c+n)&M
    L=len(fin)
    # fallthrough switch on len
    if L==11: c=(c+(fin[10]<<24))&M
    if L>=10: c=(c+(fin[9]<<16))&M
    if L>=9: c=(c+(fin[8]<<8))&M
    if L>=8: b=(b+(fin[7]<<24))&M
    if L>=7: b=(b+(fin[6]<<16))&M
    if L>=6: b=(b+(fin[5]<<8))&M
    if L>=5: b=(b+fin[4])&M
    if L>=4: a=(a+(fin[3]<<24))&M
    if L>=3: a=(a+(fin[2]<<16))&M
    if L>=2: a=(a+(fin[1]<<8))&M
    if L>=1: a=(a+fin[0])&M
    return mix(a,b,c)

def hashpjw(name):
    h=0
    b=name.encode() if isinstance(name,str) else name
    for v in b:
        h=((h<<4)+v)&0xFFFFFFFFFFFFFFFF
        g=h & 0xf0000000
        if g: h^= (g>>24); h^=g
    return h & M

# term model: ('atom',name) ('int',v) ('tuple',[...]) ('list',[...]) ('nil',) ('map',[(k,v)]) ('bin',bytes,bits)
def hash2(term, h=0, top=True):
    k=term[0]
    if k=='atom':
        hv=hashpjw(term[1])
        if h==0: return hv
        return h1(h,hv,0xdaa66d2b)
    if k=='int':
        s=term[1]
        if not is_ssmall28(s): return not_ssmall28(h,s)
        return sint32(h,s,HCONST)
    if k=='nil':
        if h==0: return 3468870702
        return h1(h,0x02,0x3c6ef372)
    if k=='tuple':
        el=term[1]; h=h1(h,len(el),0x8ff34781)
        for e in el: h=hash2(e,h,False)
        return h
    if k=='list':
        el=term[1]; tail=term[2]
        c=0; sh=0; i=0
        ended_with_tail=None
        while i < len(el):
            car=el[i]
            if not (car[0]=='int' and 0<=car[1]<256):
                ended_with_tail=None
                break
            sh=((sh<<8)+car[1])&M
            if c==3: h=h1(h,sh,0x78dde6e4); c=0; sh=0
            else: c+=1
            if i+1 < len(el):
                i+=1
            else:
                ended_with_tail=tail
                break
        if c>0: h=h1(h,sh,0x78dde6e4)
        if ended_with_tail is None:
            # the CAR of cell i is not a byte: process CAR, then the CDR chain
            car=el[i]
            if i+1 < len(el):
                rest=('list',el[i+1:],tail)
            else:
                rest=tail
            h=hash2(car,h,False)
            return hash2(rest,h,False)
        return hash2(ended_with_tail,h,False)
    if k=='map':
        pairs=term[1]; h=h1(h,len(pairs),0xe3779b90)
        if len(pairs)==0: return h
        xor=0
        for (kk,vv) in pairs:
            ph=hash2(kk,0,False); ph=hash2(vv,ph,False); xor^=ph
        return h1(h,xor,0xbe1e08bb)
    if k=='bin':
        data, nbits = term[1],term[2]
        con=(0x08d12e65 + h)&M
        sz=nbits//8; bitsize=nbits%8
        if sz==0 and bitsize==0: h=con
        else:
            h=block_hash(data[:sz],con)
            if bitsize>0:
                b=data[sz]>>(8-bitsize)
                h=h2(h,bitsize,b,0x454021d7)
        return h
    if k=='big':
        neg,limbs=term[1],term[2]
        con=0x2e2ac13a if neg else 0xcc623af3
        for d in limbs:
            h=h2(h,d&M,(d>>32)&M,con)
        return h
    if k=='float':
        import struct
        bits=struct.unpack('<Q',struct.pack('<d',term[1]))[0]
        lo=bits&M; hi=(bits>>32)&M
        return h2(h,hi,lo,0x6a99b4ac)
    raise Exception(k)

def phash2_1(t): return hash2(t,0) & 0x07FFFFFF
def phash2_2(t,r):
    h=hash2(t,0)
    return (h % r) if r else h

tests = {
 'foo': (('atom','foo'),),
}
print('foo', phash2_1(('atom','foo')), 'expect 27999')
print('{a,1}', phash2_1(('tuple',[('atom','a'),('int',1)])), 'expect 72425156')
print('[1,2,3]', phash2_1(('list',[('int',1),('int',2),('int',3)],('nil',))), 'expect 25788620')
print('<<"ab">>', phash2_1(('bin',b'ab',16)), 'expect 127597802')
print('#{a=>1}', phash2_1(('map',[(('atom','a'),('int',1))])), 'expect 59617982')
print('1000000', phash2_1(('int',1000000)), 'expect 109607396')
print('-5', phash2_1(('int',-5)), 'expect 115015680')
print('[]', phash2_1(('nil',)), 'expect 113427502')
