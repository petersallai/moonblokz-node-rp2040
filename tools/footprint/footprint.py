#!/usr/bin/env python3
"""MoonBlokz node footprint: binary size, static RAM and stack, before/after.

Measures one workspace (a directory holding the `moonblokz-*` sibling crates):

* the `moonblokz-node-rp2040` release firmware: flash (`.vector_table` + `.boot2` +
  `.text` + `.rodata` + `.data`), static RAM (`.data` + `.bss`), and the Core 1
  stack reservation (`CORE1_STACK`, which hosts the blockchain task);
* the footprint probe (moonblokz-node-rp2040/tools/footprint/probe): the blockchain stack at the
  architecture 5 defaults, built with and without the blockchain, for the
  blockchain's flash and static RAM and its worst-case stack per API entry
  point (prologue frames summed along the deepest call path, stack_depth.py).

While the node firmware does not instantiate the blockchain (O-020), the node
estimate adds the probe's blockchain delta to the node build. "Other RAM" is
static RAM without the Core 1 stack reservation.

    footprint.py                         measure the live workspace
    footprint.py --scheme S              ... built for signature scheme S
                                         (schnorr, bls-min-sig, bls-min-pk;
                                         default schnorr): node and probe get
                                         --no-default-features --features S
    footprint.py --base REPO=REF,...     measure a BASE workspace (git
                                         worktrees at REF; HEAD for repos not
                                         named; repos with no commit copied)
                                         and the live one, print before/after/delta
                                         (schnorr only: the base is built with
                                         its defaults, older code has no BLS)

Needs the thumbv6m-none-eabi target, arm-none-eabi-size / -nm / -objdump.
"""
import argparse, json, math, os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
# The workspace root: <ws>/moonblokz-node-rp2040/tools/footprint (DEC-287).
LIVE = os.path.dirname(os.path.dirname(os.path.dirname(HERE)))
# The 264 KB inequality (system constraints §3.2, Story 6.17 AC3): what the probe and
# the node measure, plus the rows that are not linked or measured yet. Estimates.
RAM_LIMIT = 264 * 1024
ESTIMATED_ROWS = {
    "mempool (not linked yet)": 24883,     # about 24.3 KB
    "radio-lib memory-config-medium": 60 * 1024,
    "crypto-lib scratch": 4 * 1024,
    "storage page buffer": 4 * 1024,
    "other task stacks": 18 * 1024,
}
CORE1_MARGIN = 1.25  # DEC-137: Core 1 >= 1.25 x the deepest root
FIT_POINTS = (200, 300)  # node counts the per-node static RAM is measured at
ROOTS = ["probe_receive", "probe_restart", "probe_genesis", "probe_tick", "probe_support"]
ENTRY = {"probe_receive": "receive_block", "probe_restart": "initialize_from_storage",
         "probe_genesis": "process_genesis", "probe_tick": "on_tick",
         "probe_support": "receive_support"}


def run(cmd, cwd, env=None):
    r = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    if r.returncode:
        sys.exit("failed: %s (in %s)\n%s" % (" ".join(cmd), cwd, r.stderr[-3000:]))
    return r.stdout


def sections(elf):
    out = {}
    for line in run(["arm-none-eabi-size", "-A", elf], "/").splitlines():
        m = re.match(r"^(\.\S+)\s+(\d+)\s", line)
        if m:
            out[m.group(1)] = int(m.group(2))
    return out


def symbol_size(elf, pattern):
    for line in run(["arm-none-eabi-nm", "-S", "-C", elf], "/").splitlines():
        p = line.split(None, 3)
        if len(p) == 4 and re.search(pattern, p[3]):
            return int(p[1], 16)
    return 0


def flash_of(s):
    return sum(s.get(k, 0) for k in (".vector_table", ".boot2", ".text", ".rodata", ".data"))


def measure(ws, scratch, scheme=None):
    # `scheme=None` builds with each crate's defaults (the base workspace).
    flags = ["--no-default-features", "--features", scheme] if scheme else []
    env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(scratch, "target-node"))
    node_dir = os.path.join(ws, "moonblokz-node-rp2040")
    run(["cargo", "build", "--release"] + flags, node_dir, env)
    elf = os.path.join(scratch, "target-node", "thumbv6m-none-eabi", "release", "moonblokz-node-rp2040")
    s = sections(elf)
    node = {"flash": flash_of(s), "ram_static": s.get(".data", 0) + s.get(".bss", 0),
            "core1_stack": symbol_size(elf, r"CORE1_STACK"),
            "links_blockchain": symbol_size(elf, r"moonblokz_blockchain") > 0}

    probe = os.path.join(scratch, "probe")
    shutil.copytree(os.path.join(HERE, "probe"), probe, dirs_exist_ok=True)
    with open(os.path.join(probe, "Cargo.toml.in")) as fh:
        toml = fh.read().replace("@WS@", ws)
    with open(os.path.join(probe, "Cargo.toml"), "w") as fh:
        fh.write(toml)
    penv = dict(os.environ, FOOTPRINT_WS=ws, CARGO_TARGET_DIR=os.path.join(scratch, "target-probe"))
    pelf = os.path.join(scratch, "target-probe", "thumbv6m-none-eabi", "release", "footprint-probe")
    if scheme:
        run(["cargo", "build", "--release"] + flags, probe, penv)
        base = sections(pelf)
        run(["cargo", "build", "--release", "--no-default-features", "--features",
             scheme + ",with-chain"], probe, penv)
    else:
        run(["cargo", "build", "--release"], probe, penv)
        base = sections(pelf)
        run(["cargo", "build", "--release", "--features", "with-chain"], probe, penv)
    chain = sections(pelf)
    dis = os.path.join(scratch, "probe.dis")
    with open(dis, "w") as fh:
        fh.write(run(["arm-none-eabi-objdump", "-d", "--no-show-raw-insn", pelf], "/"))
    depth = run([sys.executable, "-I", os.path.join(HERE, "stack_depth.py"), dis] + ROOTS, "/")
    stack = {ENTRY[r]: int(n) for r, n in re.findall(r"^== (\w+): worst-case (\d+) B", depth, re.M)}
    bc = {"flash": flash_of(chain) - flash_of(base), "ram": symbol_size(pelf, r"\bSLOT\b"),
          "stack": stack}

    add = not node["links_blockchain"]
    other = node["ram_static"] - node["core1_stack"] + (bc["ram"] if add else 0)
    budget = fit(scheme, probe, penv, pelf, node, bc, stack) if scheme and add else None
    return {"node": node, "blockchain": bc, "budget": budget,
            "estimate": {"flash": node["flash"] + (bc["flash"] if add else 0),
                         "ram_other": other,
                         "stack_reserved": node["core1_stack"],
                         "stack_peak": max(stack.values()) if stack else 0,
                         "blockchain_added": add}}


def fit(scheme, probe, penv, pelf, node, bc, stack):
    """The 264 KB inequality, the Core 1 gate and the largest MAX_NODES that fits."""
    slots = []
    for n in FIT_POINTS:
        run(["cargo", "build", "--release", "--no-default-features", "--features",
             scheme + ",with-chain"], probe, dict(penv, FOOTPRINT_MAX_NODES=str(n)))
        slots.append(symbol_size(pelf, r"\bSLOT\b"))
    per_node = (slots[1] - slots[0]) // (FIT_POINTS[1] - FIT_POINTS[0])
    core1 = node["core1_stack"]
    # The node's static RAM already holds its Core 1 stack and the executor statics.
    fixed = node["ram_static"] + sum(ESTIMATED_ROWS.values())
    total = bc["ram"] + fixed
    peak = max(stack.values()) if stack else 0
    return {"limit": RAM_LIMIT, "total": total, "margin": RAM_LIMIT - total,
            "estimated_rows": ESTIMATED_ROWS,
            "slot_per_node": per_node,
            "max_nodes_fit": FIT_POINTS[0] + (RAM_LIMIT - fixed - slots[0]) // per_node,
            "core1_required": math.ceil(peak * CORE1_MARGIN / 1024) * 1024,
            "core1_ok": core1 >= peak * CORE1_MARGIN}


def base_workspace(spec, scratch):
    refs = dict(kv.split("=", 1) for kv in spec.split(",") if kv)
    ws = os.path.join(scratch, "ws")
    os.makedirs(ws)
    trees = []
    for name in sorted(os.listdir(LIVE)):
        src = os.path.join(LIVE, name)
        if not (name.startswith("moonblokz-") or name == "rp-usb-console") or not os.path.isdir(src):
            continue
        has_commit = subprocess.run(["git", "rev-parse", "--verify", "-q", "HEAD"], cwd=src,
                                    capture_output=True).returncode == 0
        if os.path.isdir(os.path.join(src, ".git")) and has_commit:
            run(["git", "worktree", "add", "--detach", "-q", os.path.join(ws, name),
                 refs.get(name, "HEAD")], src)
            trees.append(src)
        elif name == "moonblokz-node-rp2040" or name in refs:
            shutil.copytree(src, os.path.join(ws, name), ignore=shutil.ignore_patterns("target"))
    return ws, trees


def kb(n):
    return "%.1f KiB" % (n / 1024)


def delta(a, b):
    d = b - a
    return "%+.1f KiB" % (d / 1024) if d else "0"


def table(before, after):
    rows = [("Binary size (flash, estimated node)", "flash"),
            ("RAM without stack (static, estimated node)", "ram_other"),
            ("Stack: Core 1 reservation (blockchain task)", "stack_reserved"),
            ("Stack: blockchain worst-case peak", "stack_peak")]
    out = ["| Metric | Before | After | Change |", "|---|---:|---:|---:|"]
    for label, key in rows:
        a, b = before["estimate"][key], after["estimate"][key]
        out.append("| %s | %s | %s | %s |" % (label, kb(a), kb(b), delta(a, b)))
    for entry in ENTRY.values():
        a, b = before["blockchain"]["stack"].get(entry, 0), after["blockchain"]["stack"].get(entry, 0)
        out.append("| Stack peak: `%s` | %s | %s | %s |" % (entry, kb(a), kb(b), delta(a, b)))
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--base", help="REPO=REF,... for the before workspace")
    ap.add_argument("--json", action="store_true", help="print raw measurements")
    ap.add_argument("--scheme", choices=["schnorr", "bls-min-sig", "bls-min-pk"],
                    default="schnorr", help="signature scheme of the live build")
    args = ap.parse_args()
    if args.base and args.scheme != "schnorr":
        sys.exit("--base compares schnorr builds only (older code has no BLS)")
    scratch = tempfile.mkdtemp(prefix="footprint-")
    trees = []
    try:
        after = measure(LIVE, os.path.join(scratch, "after"), args.scheme)
        after["scheme"] = args.scheme
        if not args.base:
            print(json.dumps(after, indent=2))
            return
        ws, trees = base_workspace(args.base, scratch)
        before = measure(ws, os.path.join(scratch, "before"))
        print(json.dumps({"before": before, "after": after}, indent=2) if args.json
              else table(before, after))
    finally:
        for src in trees:
            name = os.path.basename(src)
            subprocess.run(["git", "worktree", "remove", "--force", os.path.join(scratch, "ws", name)],
                           cwd=src, capture_output=True)
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
