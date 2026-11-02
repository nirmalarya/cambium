#!/usr/bin/env python3
"""Load test for the changeset workflow against a running cmbd (stdlib only).

Phases:
  seed   create a monorepo of --files files spread over --dirs directories
  mixed  for --duration seconds, run together:
           landers  agents/CI landing work: upload blobs, create a changeset on the
                    current head (or, --stale-pct of the time, a slightly stale one),
                    POST /land (the server rebases + compare-and-swaps)
           readers  CI checkouts: GET /root, walk every tree, fetch --blobs-per-checkout
                    blobs (a sparse CI checkout)
Prints one JSON object with throughput, latency percentiles and error counts.

Usage: load.py --server URL --token T [--landers 32] [--readers 16] [--duration 120]
"""
import argparse, http.client, json, os, random, threading, time, urllib.parse

ap = argparse.ArgumentParser()
ap.add_argument("--server", required=True)
ap.add_argument("--token", required=True)
ap.add_argument("--repo", default="mono")
ap.add_argument("--files", type=int, default=20000)
ap.add_argument("--dirs", type=int, default=500)
ap.add_argument("--landers", type=int, default=32)
ap.add_argument("--readers", type=int, default=16)
ap.add_argument("--duration", type=float, default=120)
ap.add_argument("--stale-pct", type=int, default=30)
ap.add_argument("--blobs-per-checkout", type=int, default=200)
ap.add_argument("--label", default="")
a = ap.parse_args()
u = urllib.parse.urlparse(a.server)
H = {"Authorization": f"Bearer {a.token}"}


class Client:
    def __init__(self):
        self.c = None

    def req(self, method, path, body=None, ctype="application/json"):
        for attempt in range(3):
            try:
                if self.c is None:
                    self.c = http.client.HTTPConnection(u.hostname, u.port, timeout=120)
                h = dict(H)
                h["content-type"] = ctype
                data = body if isinstance(body, (bytes, type(None))) else json.dumps(body).encode()
                self.c.request(method, path, body=data, headers=h)
                r = self.c.getresponse()
                b = r.read()
                try:
                    return r.status, json.loads(b)
                except Exception:
                    return r.status, b
            except (ConnectionError, http.client.HTTPException, OSError):
                self.c = None
                if attempt == 2:
                    return "conn_error", None


def blob(c, data):
    s, b = c.req("POST", "/v1/blobs", data, "application/octet-stream")
    assert s == 200, (s, b)
    return b["id"]


def head(c):
    s, b = c.req("GET", f"/v1/repos/{a.repo}/resolve/main")
    return b["cs"]


def pct(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    return round(1000 * xs[min(len(xs) - 1, int(p / 100 * len(xs)))], 1)


# ---------- seed ----------
c = Client()
c.req("POST", "/v1/repos", {"name": a.repo})
t0 = time.perf_counter()
batch = 2000
for start in range(0, a.files, batch):
    files = []
    for i in range(start, min(a.files, start + batch)):
        d = i % a.dirs
        data = (f"// file {i} in dir {d}\n" + "fn line() {}\n" * 40).encode()
        files.append({"path": f"svc{d // 50}/d{d}/f{i}.rs", "blob": blob(c, data), "mode": 33188})
    s, cs = c.req("POST", f"/v1/repos/{a.repo}/changesets", {"parent": head(c), "message": f"seed {start}", "files": files})
    assert s == 200, (s, cs)
    s, r = c.req("POST", f"/v1/repos/{a.repo}/land", {"cs": cs["id"], "onto": "main"})
    assert s == 200, (s, r)
seed_s = time.perf_counter() - t0

# ---------- mixed ----------
lock = threading.Lock()
land_lat, land_only, checkout_lat, trees_per_checkout = [], [], [], []
statuses = {}
recent_heads = [head(c)]
stop = time.perf_counter() + a.duration
start_barrier = threading.Barrier(a.landers + a.readers)


def note(k):
    with lock:
        statuses[k] = statuses.get(k, 0) + 1


def lander(wid):
    cl = Client()
    rnd = random.Random(wid)
    start_barrier.wait()
    n = 0
    while time.perf_counter() < stop:
        n += 1
        t = time.perf_counter()
        d = rnd.randrange(a.dirs)
        files = []
        for k in range(rnd.randint(1, 3)):
            data = f"// agent {wid} change {n}.{k}\n{os.urandom(16).hex()}\n".encode()
            files.append({"path": f"svc{d // 50}/d{d}/agent{wid}_{k}.rs", "blob": blob(cl, data), "mode": 33188})
        with lock:
            parent = rnd.choice(recent_heads[-8:]) if rnd.randrange(100) < a.stale_pct else None
        parent = parent or head(cl)
        s, cs = cl.req("POST", f"/v1/repos/{a.repo}/changesets", {"parent": parent, "message": f"agent {wid} #{n}", "files": files})
        if s != 200:
            note(f"changeset_{s}")
            continue
        tl = time.perf_counter()
        s, r = cl.req("POST", f"/v1/repos/{a.repo}/land", {"cs": cs["id"], "onto": "main"})
        now = time.perf_counter()
        note(f"land_{s}")
        if s == 200:
            with lock:
                land_lat.append(now - t)
                land_only.append(now - tl)
                recent_heads.append(r["cs"])


def reader(wid):
    cl = Client()
    rnd = random.Random(1000 + wid)
    start_barrier.wait()
    while time.perf_counter() < stop:
        t = time.perf_counter()
        s, r = cl.req("GET", f"/v1/repos/{a.repo}/root/main")
        if s != 200:
            note(f"root_{s}")
            continue
        todo, files, ntrees = [("", r["root"])], [], 0
        while todo:
            path, tid = todo.pop()
            s, tr = cl.req("GET", f"/v1/repos/{a.repo}/trees/{tid}?path={path}")
            ntrees += 1
            if s != 200:
                note(f"tree_{s}")
                break
            for e in tr["entries"]:
                p = f"{path}/{e['name']}" if path else e["name"]
                (todo.append((p, e["id"])) if e["kind"] == "tree" else files.append(e["id"]))
        for bid in rnd.sample(files, min(len(files), a.blobs_per_checkout)):
            s, _ = cl.req("GET", f"/v1/blobs/{bid}")
            if s != 200:
                note(f"blob_{s}")
        with lock:
            checkout_lat.append(time.perf_counter() - t)
            trees_per_checkout.append(ntrees)
        note("checkout_ok")


threads = [threading.Thread(target=lander, args=(i,)) for i in range(a.landers)]
threads += [threading.Thread(target=reader, args=(i,)) for i in range(a.readers)]
t1 = time.perf_counter()
for t in threads:
    t.start()
for t in threads:
    t.join()
elapsed = time.perf_counter() - t1
lands = len(land_lat)
errors = {k: v for k, v in statuses.items() if not (k.endswith("_200") or k == "checkout_ok")}
print(json.dumps({
    "label": a.label,
    "repo_files": a.files, "repo_dirs": a.dirs,
    "seed_seconds": round(seed_s, 1),
    "landers": a.landers, "readers": a.readers, "duration_s": round(elapsed, 1),
    "stale_parent_pct": a.stale_pct,
    "lands_ok": lands,
    "lands_per_s": round(lands / elapsed, 2),
    "merges_per_day_capacity": int(lands / elapsed * 86400),
    "land_total_ms": {"p50": pct(land_lat, 50), "p95": pct(land_lat, 95), "p99": pct(land_lat, 99)},
    "land_call_ms": {"p50": pct(land_only, 50), "p95": pct(land_only, 95), "p99": pct(land_only, 99)},
    "checkouts_ok": len(checkout_lat),
    "checkout_ms": {"p50": pct(checkout_lat, 50), "p95": pct(checkout_lat, 95), "p99": pct(checkout_lat, 99)},
    "trees_per_checkout": max(trees_per_checkout) if trees_per_checkout else 0,
    "blobs_per_checkout": a.blobs_per_checkout,
    "errors": errors,
}))
