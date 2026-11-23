"""Worst-case stack per root from an `arm-none-eabi-objdump -d` listing.

Frames are read from the prologues (`push`, `sub sp`, `add sp, rX` with a
literal). Calls are `bl`, `blx` through a register loaded from a literal
(also through `mov` and stack spill slots), and `b` / `b.w` tail calls.
An unresolved `blx` is listed, not followed.

    stack_depth.py LISTING ROOT...
"""
import re, sys
dis = open(sys.argv[1]).read().split('\n')
fn_re = re.compile(r'^([0-9a-f]+) <(.+)>:$')
ins_re = re.compile(r'^\s*([0-9a-f]+):\s+(\S+)\s*(.*)$')
words = {}
addr2fn = {}
unresolved = {}
funcs = {}  # name -> list of (addr, op, args)
cur = None
for line in dis:
    m = fn_re.match(line)
    if m:
        cur = m.group(2); funcs[cur] = []; addr2fn[int(m.group(1),16)] = cur; continue
    m = ins_re.match(line)
    if m and cur:
        addr, op, args = int(m.group(1), 16), m.group(2), m.group(3)
        funcs[cur].append((addr, op, args))
        if op == '.word':
            words[addr] = int(args.split()[0], 16)
frames, edges, indirect = {}, {}, {}
for name, ins in funcs.items():
    frame = 0; lits = {}; slots = {}; e = set(); ind = False
    for addr, op, args in ins:
        if op == 'push':
            frame += 4 * len(args.strip('{} ').split(','))
        elif op.startswith('sub') and args.startswith('sp, #'):
            frame += int(args.split('#')[1].split()[0])
        # propagate tracked function-pointer literals through mov and sp spill slots
        a_ = [x.strip() for x in args.split(';')[0].split(',')]
        newv = None; dst = None
        if op in ('mov', 'movs') and len(a_) == 2 and re.fullmatch(r'r\d+', a_[1]):
            dst, newv = a_[0], lits.get(a_[1])
        elif op == 'str' and len(a_) >= 2 and a_[1].startswith('[sp') and a_[0] in lits:
            slots[a_[2].rstrip(']') if len(a_) > 2 else '#0'] = lits[a_[0]]
        elif op == 'ldr' and len(a_) >= 2 and a_[1].startswith('[sp'):
            k = a_[2].rstrip(']') if len(a_) > 2 else '#0'
            dst, newv = a_[0], slots.get(k)
        # invalidate literal-tracked registers on any other write
        if not (op == 'ldr' and '[pc' in args):
            if op in ('pop', 'ldmia', 'ldm', 'ldmia.w'):
                for r_ in re.findall(r'r\d+', args.split('{')[-1]): lits.pop(r_, None)
            elif not (op.startswith('str') or op.startswith('stm') or op in ('cmp', 'cmn', 'tst', 'push', 'blx', 'bx') or op.startswith('b')):
                mm = re.match(r'(r\d+|fp|ip|lr)\b', args)
                if mm: lits.pop(mm.group(1), None)
        if dst is not None and newv is not None:
            lits[dst] = newv
        if op == 'ldr' and '[pc' in args:
            m = re.search(r'[;@] \(([0-9a-f]+)', args)
            if m: lits[args.split(',')[0]] = words.get(int(m.group(1), 16))
        elif op == 'add' and args.startswith('sp, r'):
            v = lits.get(args.split(',')[1].strip())
            if v is not None:
                v = v - (1 << 32) if v & 0x80000000 else v
                if v < 0: frame += -v
        elif op in ('bl', 'b', 'b.w') or op.startswith('b') and '<' in args and op not in ('bx','blx'):
            m = re.search(r'<([^>+]+)>', args)
            # a branch into another function is a call (`bl`) or a tail call
            # (`b` / `b.w`); a tail call is counted on top of the caller's frame,
            # which over-reports slightly and never under-reports
            if m and m.group(1) != name and m.group(1) in funcs:
                e.add(m.group(1))
        if op == 'blx':
            reg = args.strip().split()[0]
            v = lits.get(reg)
            if v is not None and (v & ~1) in addr2fn:
                tgt = addr2fn[v & ~1]
                if tgt != name: e.add(tgt)
            else:
                ind = True
                unresolved[name] = unresolved.get(name, 0) + 1
        if op in ('bl', 'blx'):
            for r_ in ('r0', 'r1', 'r2', 'r3', 'r12', 'ip'): lits.pop(r_, None)
    frames[name] = frame; edges[name] = e; indirect[name] = ind
memo = {}
def depth(n, stack=()):
    if n in stack: return (0, ['<cycle ' + n + '>'])
    if n in memo: return memo[n]
    best = (0, [])
    for c in edges[n]:
        d = depth(c, stack + (n,))
        if d[0] > best[0]: best = d
    r = (frames[n] + best[0], [n] + best[1])
    # a depth cut short by a cycle depends on the path that reached it: memoize
    # only complete results, so a later root never reuses an under-count
    if not any(p.startswith('<cycle') for p in r[1]):
        memo[n] = r
    return r
for root in sys.argv[2:]:
    d, path = depth(root)
    print(f'== {root}: worst-case {d} B')
    for p in path:
        print(f'   {frames.get(p, 0):6}  {p[:110]}' + ('  [indirect call]' if indirect.get(p) else ''))
big = sorted(frames.items(), key=lambda kv: -kv[1])[:12]
print('== largest frames')
for n, f in big: print(f'   {f:6}  {n[:110]}')
print('== unresolved blx (function: count)')
for n, c in sorted(unresolved.items(), key=lambda kv: -kv[1])[:20]: print(f'   {c:4}  {n[:110]}')
