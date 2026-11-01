#!/usr/bin/env python3
"""Load test: N concurrent committers against a running cmbd.

Most committers own a branch and make C commits each (upload a blob, then
commit), which is the usual pattern for a busy monorepo. A smaller group
(--hot) commits to one shared branch and retries on ref-moved (409) to
measure contention. Prints p50/p95/p99 commit latency, throughput and error
rate as JSON.
Usage: committers.py --server URL --token T [--workers 180] [--hot 20] [--commits 10]
"""
import argparse, http.client, json, os, statistics, threading, time, urllib.parse, uuid

ap = argparse.ArgumentParser()
ap.add_argument("--server", required=True); ap.add_argument("--token", required=True)
ap.add_argument("--repo", default="load"); ap.add_argument("--workers", type=int, default=180)
ap.add_argument("--commits", type=int, default=10); ap.add_argument("--hot", type=int, default=20)
ap.add_argument("--label", default="")
a = ap.parse_args()
u = urllib.parse.urlparse(a.server)
H = {"Authorization": f"Bearer {a.token}"}

def conn():
    return http.client.HTTPConnection(u.hostname, u.port, timeout=60)

def req(c, method, path, body=None, ctype="application/json", extra=None):
    h = dict(H); h["content-type"] = ctype
    if extra: h.update(extra)
    data = body if isinstance(body, (bytes, type(None))) else json.dumps(body).encode()
    c.request(method, path, body=data, headers=h)
    r = c.getresponse(); b = r.read()
    try: return r.status, json.loads(b)
    except Exception: return r.status, b

c = conn()
req(c, "POST", "/v1/repos", {"name": a.repo})
_, refs = req(c, "GET", f"/v1/repos/{a.repo}/refs")
root = [r["cs"] for r in refs if r["name"] == "main"][0]

lat, errors, conflicts, lock, statuses = [], [], [0], threading.Lock(), {}

def commit_once(c, branch, head, i, wid):
    blob = os.urandom(8).hex().encode() + b" line\n" * 20
    t0 = time.perf_counter()
    s1, b1 = req(c, "POST", "/v1/blobs", blob, "application/octet-stream")
    if s1 != 200: return s1, None, time.perf_counter() - t0
    body = {"ref": branch, "expected": head, "message": f"w{wid} c{i}",
            "files": [{"path": f"w{wid}/f{i % 3}.txt", "blob": b1["id"], "mode": 33188}]}
    s2, b2 = req(c, "POST", f"/v1/repos/{a.repo}/commit", body, extra={"Idempotency-Key": str(uuid.uuid4())})
    return s2, (b2.get("id") if s2 == 200 else b2), time.perf_counter() - t0

barrier = threading.Barrier(a.workers + a.hot)
def note(s):
    statuses[str(s)] = statuses.get(str(s), 0) + 1

def own_branch(wid):
    c = conn(); br = f"w{wid}"
    req(c, "POST", f"/v1/repos/{a.repo}/refs/{br}", {"expected": None, "new": root})
    head = root
    barrier.wait()
    for i in range(a.commits):
        try:
            s, r, dt = commit_once(c, br, head, i, wid)
        except Exception as e:
            s, r, dt = f"exc:{type(e).__name__}", None, 0; c = conn()
        with lock:
            note(s)
            if s == 200: lat.append(dt); head = r
            else: errors.append(s)

def hot(wid):
    c = conn(); barrier.wait()
    for i in range(a.commits):
        for attempt in range(100):
            try:
                _, refs = req(c, "GET", f"/v1/repos/{a.repo}/refs")
                head = [r["cs"] for r in refs if r["name"] == "main"][0]
                s, r, dt = commit_once(c, "main", head, i, 10000 + wid)
            except Exception as e:
                s, dt = f"exc:{type(e).__name__}", 0; c = conn()
            if s == 200:
                with lock: lat.append(dt); note(200)
                break
            if s == 409:
                with lock: conflicts[0] += 1
                continue
            with lock: errors.append(s); note(s)
            break
        else:
            with lock: errors.append("gave-up"); note("gave-up")

ts = [threading.Thread(target=own_branch, args=(w,)) for w in range(a.workers)] + \
     [threading.Thread(target=hot, args=(w,)) for w in range(a.hot)]
t0 = time.perf_counter()
for t in ts: t.start()
for t in ts: t.join()
wall = time.perf_counter() - t0
lat.sort()
q = lambda p: round(lat[min(len(lat) - 1, int(len(lat) * p))] * 1000, 1) if lat else None
total = len(lat) + len(errors)
print(json.dumps({"label": a.label, "concurrent_committers": a.workers + a.hot, "own_branch_workers": a.workers,
    "shared_branch_workers": a.hot, "commits_ok": len(lat), "errors": len(errors),
    "error_rate": round(len(errors) / total, 4) if total else None, "cas_conflicts_retried": conflicts[0],
    "p50_ms": q(.50), "p95_ms": q(.95), "p99_ms": q(.99), "max_ms": q(1.0),
    "mean_ms": round(statistics.mean(lat) * 1000, 1) if lat else None,
    "throughput_commits_per_s": round(len(lat) / wall, 1), "wall_s": round(wall, 2), "statuses": statuses}))
