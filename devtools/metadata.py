#!/usr/bin/env python3
"""Read .NET metadata straight out of Timberborn's assemblies.

The splitter resolves everything by name, so "did a game update rename
something" is answerable offline, without launching the game at all. This is
the offline half of the version check; `src/probe.rs` is the runtime half.

    # list every class and field in an assembly
    ./metadata.py dump ~/.../Timberborn_Data/Managed/Timberborn.Wonders.dll

    # check every name src/probe.rs depends on against an install
    ./metadata.py check ~/.../Timberborn_Data/Managed

`check` is the one to run after switching Steam branches. A clean result means
any MISSING the runtime probe reports is a real change, not a typo here.

Parses the ECMA-335 metadata tables directly -- no mono or ilspy needed.
"""
import os
import re
import struct
import subprocess
import sys

def u2(b,o): return struct.unpack_from('<H',b,o)[0]
def u4(b,o): return struct.unpack_from('<I',b,o)[0]

def parse(path):
    d=open(path,'rb').read()
    pe=u4(d,0x3c)
    assert d[pe:pe+4]==b'PE\0\0'
    nsec=u2(d,pe+6); optsz=u2(d,pe+20); opt=pe+24
    magic=u2(d,opt)
    ddoff = opt + (96 if magic==0x10b else 112)
    cli_rva=u4(d,ddoff+14*8)
    if cli_rva==0: return None
    secs=[]
    so=opt+optsz
    for i in range(nsec):
        s=so+i*40
        secs.append((u4(d,s+12),u4(d,s+8),u4(d,s+20)))  # vaddr, vsize, praw
    def r2o(rva):
        for va,vs,pr in secs:
            if va<=rva<va+max(vs,1)+0x1000: return pr+(rva-va)
        return None
    cli=r2o(cli_rva)
    md_rva=u4(d,cli+8)
    md=r2o(md_rva)
    assert d[md:md+4]==b'BSJB'
    vlen=u4(d,md+12); base=md+16+vlen
    nstreams=u2(d,base+2); p=base+4
    streams={}
    for i in range(nstreams):
        off=u4(d,p); size=u4(d,p+4); p+=8
        e=d.index(b'\0',p); name=d[p:e].decode()
        p=e+1
        while (p-md)%4: p+=1
        streams[name]=(md+off,size)
    tso,_=streams['#~']; stro,_=streams['#Strings']
    heapsz=d[tso+6]
    strsz = 4 if heapsz&1 else 2
    guidsz= 4 if heapsz&2 else 2
    blobsz= 4 if heapsz&4 else 2
    valid=struct.unpack_from('<Q',d,tso+8)[0]
    sorted_=struct.unpack_from('<Q',d,tso+16)[0]
    p=tso+24
    rows={}
    for i in range(64):
        if valid>>i & 1:
            rows[i]=u4(d,p); p+=4
    def idx(t): return 4 if rows.get(t,0)>=65536 else 2
    def coded(tables,bits):
        m=max(rows.get(t,0) for t in tables)
        return 4 if m>= (1<<(16-bits)) else 2
    resscope=coded([0,26,35,1],2)
    typedeforref=coded([2,1,27],2)
    sizes={
      0: 2+strsz+3*guidsz,
      1: resscope+2*strsz,
      2: 4+2*strsz+typedeforref+idx(4)+idx(6),
      3: idx(4),
      4: 2+strsz+blobsz,
    }
    def readstr(o):
        e=d.index(b'\0',stro+o); return d[stro+o:e].decode('utf8','replace')
    def rd(b,o,sz): return u2(b,o) if sz==2 else u4(b,o)
    # locate table starts
    off=p
    tabs={}
    for t in sorted(rows):
        if t not in sizes: break
        tabs[t]=off; off+=sizes[t]*rows[t]
    if 2 not in tabs or 4 not in tabs: return None
    # fields
    fields=[]
    fo=tabs[4]; fs=sizes[4]
    for i in range(rows.get(4,0)):
        o=fo+i*fs
        flags=u2(d,o); name=readstr(rd(d,o+2,strsz))
        fields.append((flags,name))
    out=[]
    to=tabs[2]; ts=sizes[2]
    tdefs=[]
    for i in range(rows[2]):
        o=to+i*ts
        name=readstr(rd(d,o+4,strsz))
        ns=readstr(rd(d,o+4+strsz,strsz))
        fl=rd(d,o+4+2*strsz+typedeforref, idx(4))
        tdefs.append((ns,name,fl))
    for i,(ns,name,fl) in enumerate(tdefs):
        end = tdefs[i+1][2] if i+1<len(tdefs) else rows.get(4,0)+1
        for fi in range(fl-1, min(end-1, len(fields))):
            flags,fname=fields[fi]
            if True:  # static, not literal
                out.append((ns,name,fname,flags))
    return out

targets=sys.argv[1:]
for f in targets:
    try: r=parse(f)
    except Exception as ex:
        continue
    if not r: continue
    for ns,tn,fn,fl in r:
        acc = fl&7
        print(f"{os.path.basename(f)}\t{ns}.{tn}\t{fn}\tacc={acc}")

def _fields_by_class(managed_dir, assembly):
    """{class_name: {field_names}} for one assembly, or None if unreadable."""
    path = os.path.join(managed_dir, assembly + ".dll")
    if not os.path.exists(path):
        return None
    try:
        rows = parse(path)
    except Exception:
        return None
    if rows is None:
        return None
    out = {}
    for _ns, type_name, field_name, _flags in rows:
        out.setdefault(type_name.split(".")[-1], set()).add(field_name)
    return out


def _probe_subjects(probe_rs):
    """(image, class, [fields]) triples declared in src/probe.rs."""
    src = open(probe_rs, encoding="utf8").read()
    found = re.findall(
        r'Subject \{\s*image: "([^"]+)",\s*class: "([^"]+)",(.*?)fields: &\[(.*?)\],',
        src,
        re.S,
    )
    for image, cls, _mid, raw in found:
        fields = [f.strip().strip('"') for f in raw.split(",")]
        yield image, cls, [f for f in fields if f]


def check(managed_dir, probe_rs):
    problems = 0
    for image, cls, fields in _probe_subjects(probe_rs):
        by_class = _fields_by_class(managed_dir, image)
        if by_class is None:
            print(f"  MISSING ASSEMBLY  {image}")
            problems += 1
            continue
        if cls not in by_class:
            print(f"  MISSING CLASS     {image}/{cls}")
            problems += 1
            continue
        have = by_class[cls]
        # Auto-properties are stored as <Name>k__BackingField.
        missing = [
            f for f in fields if f not in have and f"<{f}>k__BackingField" not in have
        ]
        if missing:
            print(f"  MISSING FIELD     {image}/{cls}: {', '.join(missing)}")
            problems += 1
        else:
            listed = ", ".join(fields) if fields else "(class only)"
            print(f"  ok                {image}/{cls}: {listed}")
    print()
    print("ALL RESOLVED" if not problems else f"{problems} PROBLEM(S)")
    return 1 if problems else 0


def dump(paths):
    for path in paths:
        try:
            rows = parse(path)
        except Exception as exc:
            print(f"{os.path.basename(path)}\tERROR\t{exc}", file=sys.stderr)
            continue
        if not rows:
            continue
        for ns, type_name, field_name, flags in rows:
            static = "static" if flags & 0x0010 else "instance"
            print(f"{os.path.basename(path)}\t{ns}.{type_name}\t{field_name}\t{static}")


def main():
    if len(sys.argv) >= 3 and sys.argv[1] == "dump":
        dump(sys.argv[2:])
    elif len(sys.argv) == 3 and sys.argv[1] == "check":
        here = os.path.dirname(os.path.abspath(__file__))
        probe_rs = os.path.join(here, "..", "src", "probe.rs")
        sys.exit(check(sys.argv[2], probe_rs))
    else:
        print(__doc__.strip(), file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
