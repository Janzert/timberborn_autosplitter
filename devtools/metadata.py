#!/usr/bin/env python3
"""Read .NET metadata straight out of Timberborn's assemblies.

The splitter resolves everything by name, so "did a game update rename
something" is answerable offline, without launching the game at all. This is
the offline half of the version check; `src/probe.rs` is the runtime half.

    # list every class, field and field type in an assembly
    ./metadata.py dump ~/.../Timberborn_Data/Managed/Timberborn.Wonders.dll

    # check every name src/probe.rs depends on against an install
    ./metadata.py check ~/.../Timberborn_Data/Managed

    # the names, types and static flags a test fixture needs, as JSON
    ./metadata.py facts ~/.../Timberborn_Data/Managed

`check` is the one to run after switching Steam branches. A clean result means
any MISSING the runtime probe reports is a real change, not a typo here.

`facts` is half of a test fixture: everything about the splitter's subjects
that an assembly knows. It cannot know field offsets -- Mono assigns those when
it lays a class out, so they exist only in a running process -- and `tb-fixture`
merges those in from a memory snapshot.

Parses the ECMA-335 metadata tables directly -- no mono or ilspy needed.
"""
import json
import os
import re
import struct
import subprocess
import sys
from pathlib import Path

# ECMA-335 II.23.1.16, the primitives a field signature can name outright.
ELEMENT_TYPES = {
    0x01: 'void', 0x02: 'bool', 0x03: 'char', 0x04: 'sbyte', 0x05: 'byte',
    0x06: 'short', 0x07: 'ushort', 0x08: 'int', 0x09: 'uint', 0x0a: 'long',
    0x0b: 'ulong', 0x0c: 'float', 0x0d: 'double', 0x0e: 'string',
    0x18: 'IntPtr', 0x19: 'UIntPtr', 0x1c: 'object',
}


def compressed(b, o):
    """An ECMA-335 compressed integer: (value, bytes consumed)."""
    first = b[o]
    if first & 0x80 == 0:
        return first, 1
    if first & 0x40 == 0:
        return ((first & 0x3f) << 8) | b[o+1], 2
    return ((first & 0x1f) << 24) | (b[o+1] << 16) | (b[o+2] << 8) | b[o+3], 4


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
    tso,_=streams['#~']; stro,_=streams['#Strings']; blobo,_=streams['#Blob']
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
        fields.append((flags,name,rd(d,o+2+strsz,blobsz)))
    out=[]
    to=tabs[2]; ts=sizes[2]
    tdefs=[]
    for i in range(rows[2]):
        o=to+i*ts
        name=readstr(rd(d,o+4,strsz))
        ns=readstr(rd(d,o+4+strsz,strsz))
        fl=rd(d,o+4+2*strsz+typedeforref, idx(4))
        tdefs.append((ns,name,fl))

    # TypeRef, so a signature naming a type from another assembly -- which is
    # most of them -- renders as a name rather than a table index.
    typerefs=[]
    if 1 in tabs:
        for i in range(rows[1]):
            o=tabs[1]+i*sizes[1]
            rname=readstr(rd(d,o+resscope,strsz))
            rns=readstr(rd(d,o+resscope+strsz,strsz))
            typerefs.append(f"{rns}.{rname}" if rns else rname)

    def typedeforref_name(tok):
        tag=tok&3; row=tok>>2
        if tag==0:
            if 0<row<=len(tdefs):
                tns,tn,_=tdefs[row-1]
                return f"{tns}.{tn}" if tns else tn
            return f"TypeDef#{row}"
        if tag==1:
            return typerefs[row-1] if 0<row<=len(typerefs) else f"TypeRef#{row}"
        # A TypeSpec is itself a signature blob. Field signatures spell generic
        # instantiations out inline, so reaching one here is rare enough to name
        # rather than chase.
        return f"TypeSpec#{row}"

    def sigtype(o):
        """One Type in a signature blob: (rendered, offset after it)."""
        e=d[o]; o+=1
        if e in ELEMENT_TYPES: return ELEMENT_TYPES[e], o
        if e in (0x11,0x12):                       # VALUETYPE, CLASS
            tok,n=compressed(d,o)
            return typedeforref_name(tok), o+n
        if e==0x1d:                                # SZARRAY
            inner,o=sigtype(o)
            return inner+'[]', o
        if e==0x15:                                # GENERICINST
            o+=1                                   # CLASS or VALUETYPE
            tok,n=compressed(d,o); o+=n
            base=typedeforref_name(tok).split('`')[0]
            argc,n=compressed(d,o); o+=n
            args=[]
            for _ in range(argc):
                a,o=sigtype(o)
                args.append(a)
            return f"{base}<{', '.join(args)}>", o
        if e==0x13:                                # VAR
            v,n=compressed(d,o)
            return f"!{v}", o+n
        return f"<element 0x{e:02x}>", o

    def field_type(blobidx):
        o=blobo+blobidx
        _length,n=compressed(d,o); o+=n
        if d[o]!=0x06:                             # FIELD
            return "<not a field signature>"
        try:
            return sigtype(o+1)[0]
        except Exception as exc:
            return f"<undecodable: {exc}>"

    for i,(ns,name,fl) in enumerate(tdefs):
        end = tdefs[i+1][2] if i+1<len(tdefs) else rows.get(4,0)+1
        for fi in range(fl-1, min(end-1, len(fields))):
            flags,fname,sig=fields[fi]
            out.append((ns,name,fname,flags,field_type(sig)))
    return out

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
    for _ns, type_name, field_name, _flags, _type in rows:
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


def _validator_sites(src_dir):
    """(image, class, field) for every Locatable construction in the source.

    Locatable::new validates through _eventBus; with_validator names its own
    field. A class lacking the field it is validated through can never be
    located, and the failure is silent, so it is worth checking statically."""
    for path in sorted(Path(src_dir).glob("*.rs")):
        text = path.read_text(encoding="utf8")
        for image, cls in re.findall(
            r'Locatable::new\(\s*process,\s*module,\s*"([^"]+)",\s*"([^"]+)"', text
        ):
            yield path.name, image, cls, "_eventBus"
        for image, cls, field in re.findall(
            r'Locatable::with_validator\(\s*process,\s*module,\s*"([^"]+)",\s*"([^"]+)",\s*"([^"]+)"',
            text,
        ):
            yield path.name, image, cls, field


def check_validators(managed_dir, src_dir):
    problems = 0
    sites = list(_validator_sites(src_dir))
    if not sites:
        return 0
    print()
    print("validator fields:")
    for where, image, cls, field in sites:
        by_class = _fields_by_class(managed_dir, image)
        have = (by_class or {}).get(cls)
        if have is None:
            print(f"  MISSING CLASS     {image}/{cls}  ({where})")
            problems += 1
        elif field not in have and f"<{field}>k__BackingField" not in have:
            print(
                f"  MISSING FIELD     {image}/{cls} has no {field}, so it can "
                f"never be located  ({where})"
            )
            problems += 1
        else:
            print(f"  ok                {image}/{cls} via {field}")
    return problems


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
    problems += check_validators(managed_dir, Path(probe_rs).parent)

    print()
    print("ALL RESOLVED" if not problems else f"{problems} PROBLEM(S)")
    return 1 if problems else 0



def _class_rows(managed_dir, assembly):
    """Every (namespace, class, field, flags, type) row of one assembly."""
    path = os.path.join(managed_dir, assembly + ".dll")
    if not os.path.exists(path):
        return None
    try:
        return parse(path)
    except Exception:
        return None


def _subjects(probe_rs):
    """Every (image, class, [fields]) the splitter depends on, deduplicated.

    Two sources, because the splitter has two kinds of dependency on a name:
    `SUBJECTS` in probe.rs is what it reads, and the `Locatable` sites are what
    it validates an instance through. A class can appear in only the second --
    GameInitializer does -- and a fixture missing it would build a world the
    splitter cannot find its way around.
    """
    wanted = {}
    for image, cls, fields in _probe_subjects(probe_rs):
        wanted.setdefault((image, cls), [])
        for field in fields:
            if field not in wanted[(image, cls)]:
                wanted[(image, cls)].append(field)
    for _where, image, cls, field in _validator_sites(Path(probe_rs).parent):
        entry = wanted.setdefault((image, cls), [])
        if field not in entry:
            entry.append(field)
    return wanted


def facts(managed_dir, probe_rs):
    """The half of a fixture that lives in the assemblies: names and types.

    Field *offsets* are deliberately absent. Mono assigns them when it lays a
    class out, so they exist only in a running process and no amount of reading
    the assemblies will produce them; `tb-fixture` merges them in from a
    snapshot. See TEST_HARNESS_PLAN.md in the parent repository.
    """
    classes = []
    problems = []
    for (image, cls), fields in sorted(_subjects(probe_rs).items()):
        rows = _class_rows(managed_dir, image)
        if rows is None:
            problems.append(f"{image}: assembly missing or unreadable")
            continue

        # Nested types carry the enclosing name; the splitter asks by the short
        # one, the same way asr matches it.
        declared = [row for row in rows if row[1].split(".")[-1] == cls]
        if not declared:
            problems.append(f"{image}/{cls}: no such class")
            continue

        by_name = {row[2]: row for row in declared}
        out_fields = []
        for field in fields:
            backing = f"<{field}>k__BackingField"
            row = by_name.get(field) or by_name.get(backing)
            if row is None:
                problems.append(f"{image}/{cls}: no field {field}")
                continue
            _ns, _cls, name, flags, field_type = row
            entry = {"name": name, "type": field_type, "static": bool(flags & 0x0010)}
            # An auto-property is stored under a mangled name and asked for by
            # the plain one. The fixture writes the mangled name into memory, so
            # a lookup goes through asr's backing-name path exactly as it does
            # against the game.
            if name != field:
                entry["requested"] = field
            out_fields.append(entry)

        classes.append(
            {
                "image": image,
                "namespace": declared[0][0],
                "name": cls,
                "fields": out_fields,
            }
        )

    if problems:
        for problem in problems:
            print(f"  PROBLEM  {problem}", file=sys.stderr)
        print(
            f"{len(problems)} problem(s); the assemblies do not match src/probe.rs",
            file=sys.stderr,
        )
        return 1

    json.dump({"classes": classes}, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0

def dump(paths):
    for path in paths:
        try:
            rows = parse(path)
        except Exception as exc:
            print(f"{os.path.basename(path)}\tERROR\t{exc}", file=sys.stderr)
            continue
        if not rows:
            continue
        for ns, type_name, field_name, flags, field_type in rows:
            static = "static" if flags & 0x0010 else "instance"
            print(
                f"{os.path.basename(path)}\t{ns}.{type_name}\t{field_name}"
                f"\t{field_type}\t{static}"
            )


def main():
    if len(sys.argv) >= 3 and sys.argv[1] == "dump":
        dump(sys.argv[2:])
    elif len(sys.argv) == 3 and sys.argv[1] in ("check", "facts"):
        here = os.path.dirname(os.path.abspath(__file__))
        probe_rs = os.path.join(here, "..", "src", "probe.rs")
        command = check if sys.argv[1] == "check" else facts
        sys.exit(command(sys.argv[2], probe_rs))
    else:
        print(__doc__.strip(), file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
