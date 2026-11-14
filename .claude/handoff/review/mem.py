#!/usr/bin/env python3
# Measures the tree view process's peak memory and drain time for one file of N MiB.
import json, os, subprocess, sys, time, shutil, signal
BIN = "/home/user/agentfs/target/release/agentfs"
mib = int(sys.argv[1]); work = sys.argv[2]; cap = float(sys.argv[3]) if len(sys.argv) > 3 else 300
shutil.rmtree(work, ignore_errors=True); os.makedirs(work)
state = os.path.join(work, "state"); mnt = os.path.join(work, "mnt"); metrics = os.path.join(work, "m.json")
os.makedirs(mnt)
subprocess.run([BIN, "--state", state, "trees", "init", "--volume", "t"], check=True, stdout=subprocess.DEVNULL)
log = open(os.path.join(work, "log"), "w")
p = subprocess.Popen([BIN, "--state", state, "trees", "mount", mnt, "--volume", "t", "--no-allow-other",
                      "--metrics-output", metrics], stdout=log, stderr=log)
def tree_rss():
    tot = 0
    out = subprocess.run(["ps", "-o", "rss=", "--ppid", str(p.pid)], capture_output=True, text=True).stdout.split()
    kids = sum(int(x) for x in out)
    with open(f"/proc/{p.pid}/status") as f:
        st = dict(l.split(":", 1) for l in f)
    return int(st["VmRSS"].split()[0]), int(st["VmHWM"].split()[0]), kids
for _ in range(300):
    if os.path.ismount(mnt): break
    time.sleep(0.1)
assert os.path.ismount(mnt)
block = bytearray(os.urandom(1 << 20))
t0 = time.monotonic()
fd = os.open(os.path.join(mnt, "big"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
for n in range(mib):
    block[:8] = n.to_bytes(8, "little"); mv = memoryview(block)
    while mv: mv = mv[os.write(fd, mv):]
tc = time.monotonic(); os.close(fd); os.stat(os.path.join(mnt, "big")); te = time.monotonic()
asked = time.time()
def hist():
    try:
        if os.stat(metrics).st_mtime <= asked: return None
        return json.load(open(metrics)).get("history")
    except Exception: return None
h = None
while h is None: time.sleep(0.1); h = hist()
target = h["appended"]; peak_kids = 0; peak_rss = 0; done = None
while time.monotonic() - te < cap:
    if p.poll() is not None: break
    rss, hwm, kids = tree_rss(); peak_kids = max(peak_kids, kids); peak_rss = max(peak_rss, rss + kids)
    try: b = json.load(open(metrics)).get("history", {})
    except Exception: b = {}
    if b.get("built_through", 0) >= target: done = time.monotonic() - te; break
    time.sleep(0.2)
time.sleep(5); rss, hwm, kids = tree_rss()
du = subprocess.run(["du", "-sm", state], capture_output=True, text=True).stdout.split()[0]
print(json.dumps({"mib": mib, "write_s": round(tc - t0, 2), "close_s": round(te - tc, 2),
                  "drain_s": None if done is None else round(done, 2), "agentfs_hwm_mib": hwm // 1024,
                  "children_peak_rss_mib": peak_kids // 1024, "sampled_tree_peak_mib": peak_rss // 1024,
                  "state_du_mib": int(du), "agentfs_rss_after_drain_mib": rss // 1024}), flush=True)
subprocess.run(["fusermount3", "-u", mnt]); 
try: p.wait(timeout=cap)
except subprocess.TimeoutExpired: p.kill(); subprocess.run(["fusermount3", "-uz", mnt]); p.wait()
shutil.rmtree(work, ignore_errors=True)
