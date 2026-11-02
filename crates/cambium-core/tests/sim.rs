//! Deterministic simulation: a seeded
//! workload of actors racing to land work on one engine, with restarts, checked after
//! every step against a simple reference model.
//!
//! Replay a failure exactly with its seed:
//!   CAMBIUM_SIM_SEED=1234 CAMBIUM_SIM_STEPS=2000 cargo test --test sim -- --nocapture
use cambium_core::{
    blob::{BlobStore, MemBlobStore},
    engine::split_entry,
    Engine, Error, FileChange,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// xorshift64*: tiny, deterministic, no dependencies
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

const SHARED_LINES: usize = 8;

struct Sim {
    e: Engine,
    db: std::path::PathBuf,
    blobs: Arc<MemBlobStore>,
    rng: Rng,
    /// reference model: the private files main must contain (path -> content)
    model: BTreeMap<String, String>,
    /// changesets that landed a private file, for reverts: (changeset on main, path)
    landed: Vec<(String, String)>,
    /// every changeset created, for ancestry checks
    all: Vec<String>,
    main: String,
    n: usize,
}

fn read(e: &Engine, cs: &str, path: &str) -> Option<String> {
    let m = e.manifest("r", cs).unwrap();
    m.get(path).map(|x| e.read_text(split_entry(x).0).unwrap())
}

impl Sim {
    fn put(&self, path: &str, s: &str) -> FileChange {
        FileChange { path: path.into(), blob: Some(self.e.put_blob(s.as_bytes()).unwrap()), mode: 0o100644 }
    }
    fn shared_edit(&mut self, base: &str) -> FileChange {
        let mut lines: Vec<String> = read(&self.e, base, "shared.txt").unwrap().lines().map(String::from).collect();
        let i = self.rng.below(SHARED_LINES);
        lines[i] = format!("line {i} edited in op {}", self.n);
        self.put("shared.txt", &(lines.join("\n") + "\n"))
    }
    /// One random operation; returns what it was, for failure messages.
    fn step(&mut self) -> String {
        self.n += 1;
        let op = self.rng.below(100);
        let actor = self.rng.below(4);
        let path = format!("a{actor}/f{}", self.n);
        let content = format!("op {}\n", self.n);
        // work is often based on a stale head (another actor landed meanwhile)
        let base = if self.rng.chance(40) && self.all.len() > 3 {
            // recent heads of main, never the empty root changeset (before shared.txt)
            let back: Vec<String> =
                self.e.log("r", &self.main, None, 4).unwrap().into_iter().map(|c| c.id).filter(|id| *id != self.all[0]).collect();
            back[self.rng.below(back.len())].clone()
        } else {
            self.main.clone()
        };
        match op {
            // draft + land: the server rebases onto the current head
            0..=44 => {
                let mut files = vec![self.put(&path, &content)];
                let shared = self.rng.chance(30);
                if shared {
                    files.push(self.shared_edit(&base));
                }
                let d = self.e.create_changeset("r", vec![base.clone()], "u", "draft", files).unwrap().id;
                self.all.push(d.clone());
                match self.e.land("r", &d, "main", &|_| Ok(())) {
                    Ok(tip) => {
                        self.model.insert(path.clone(), content);
                        self.landed.push((tip, path.clone()));
                        format!("land {path} (shared edit: {shared})")
                    }
                    Err(Error::Conflict(_)) => format!("land {path}: conflict on shared.txt, main unchanged"),
                    Err(e) => panic!("land failed unexpectedly: {e}"),
                }
            }
            // a race made explicit: rebase now, compare-and-swap after another actor landed
            45..=59 => {
                let d = self.e.create_changeset("r", vec![base.clone()], "u", "slow", vec![self.put(&path, &content)]).unwrap().id;
                let head = self.e.get_ref("r", "main").unwrap().unwrap();
                let tip = self.e.rebase_onto("r", &d, &head, &|_| Ok(())).unwrap();
                // the other actor wins the race
                let other = format!("a9/f{}", self.n);
                let o = self.e.create_changeset("r", vec![head.clone()], "u", "fast", vec![self.put(&other, "fast\n")]).unwrap().id;
                self.e.cas_ref("r", "main", Some(&head), Some(&o)).unwrap();
                self.model.insert(other.clone(), "fast\n".into());
                self.landed.push((o.clone(), other));
                self.all.extend([d.clone(), tip.clone(), o]);
                // the slow actor's CAS must fail, then a land retries properly
                assert!(matches!(self.e.cas_ref("r", "main", Some(&head), Some(&tip)), Err(Error::RefMoved { .. })), "stale CAS must lose");
                let t2 = self.e.land("r", &d, "main", &|_| Ok(())).unwrap();
                self.model.insert(path.clone(), content);
                self.landed.push((t2, path.clone()));
                format!("race on {path}")
            }
            // direct commit with whatever head the actor saw
            60..=74 => match self.e.commit("r", "main", &base, "u", "direct", vec![self.put(&path, &content)]) {
                Ok(c) => {
                    assert_eq!(base, self.main, "a commit with a stale expected head must not succeed");
                    self.model.insert(path.clone(), content);
                    self.landed.push((c.id.clone(), path.clone()));
                    self.all.push(c.id);
                    format!("direct commit {path}")
                }
                Err(Error::RefMoved { .. }) => {
                    assert_ne!(base, self.main);
                    format!("direct commit {path}: stale head refused")
                }
                Err(e) => panic!("commit failed unexpectedly: {e}"),
            },
            // revert a private file that landed earlier
            75..=84 if !self.landed.is_empty() => {
                let (cs, p) = self.landed.remove(self.rng.below(self.landed.len()));
                match self.e.revert("r", &cs, "main", "bot", &|_| Ok(())) {
                    Ok(_) => {
                        self.model.remove(&p);
                        format!("revert {p}")
                    }
                    // the changeset also edited shared.txt and later edits conflict
                    Err(Error::Conflict(_)) => {
                        self.landed.push((cs, p.clone()));
                        format!("revert {p}: conflict, nothing changed")
                    }
                    Err(e) => panic!("revert failed unexpectedly: {e}"),
                }
            }
            // release branch from an older main, fix on it, merge back
            85..=89 => {
                let name = format!("release/{}", self.n);
                self.e.cas_ref("r", &name, None, Some(&base)).unwrap();
                let fix = self.e.commit("r", &name, &base, "u", "fix", vec![self.put(&path, &content)]).unwrap().id;
                self.all.push(fix.clone());
                match self.e.merge("r", "main", &name, "u") {
                    Ok(m) => {
                        self.model.insert(path.clone(), content);
                        self.landed.retain(|_| true);
                        self.all.push(m.id);
                        format!("release fix {path} merged")
                    }
                    Err(Error::Conflict(_)) | Err(Error::RefMoved { .. }) => format!("release merge {path} refused"),
                    Err(e) => panic!("merge failed unexpectedly: {e}"),
                }
            }
            // crash and restart: the engine is reopened on the same database
            _ => {
                let fresh = Engine::open(self.db.to_str().unwrap(), self.blobs.clone() as Arc<dyn BlobStore>).unwrap();
                self.e = fresh;
                "restart".into()
            }
        }
    }
    fn check(&mut self, what: &str) {
        let new_main = self.e.get_ref("r", "main").unwrap().unwrap();
        // main only moves forward
        assert!(self.e.is_ancestor("r", &self.main, &new_main).unwrap(), "after {what}: main moved backwards");
        self.main = new_main.clone();
        // the tree-based manifest has exactly the private files the model expects
        let m = self.e.manifest("r", &new_main).unwrap();
        let actual: BTreeMap<String, String> =
            m.iter().filter(|(p, _)| p.starts_with('a')).map(|(p, e)| (p.clone(), self.e.read_text(split_entry(e).0).unwrap())).collect();
        assert_eq!(actual, self.model, "after {what}: landed work lost or extra");
        // shared.txt still has all its lines (merges never drop lines)
        assert_eq!(read(&self.e, &new_main, "shared.txt").unwrap().lines().count(), SHARED_LINES, "after {what}");
        // root tree verifies against its id
        let root = self.e.root_tree("r", &new_main).unwrap();
        assert_eq!(self.e.get_tree(&root).unwrap().id(), root, "after {what}");
    }
}

/// Naive ancestry by graph search, to check the skip-list fast path.
fn naive_ancestor(e: &Engine, a: &str, d: &str) -> bool {
    let mut seen = HashSet::new();
    let mut todo = vec![d.to_string()];
    while let Some(x) = todo.pop() {
        if x == a {
            return true;
        }
        if seen.insert(x.clone()) {
            todo.extend(e.get_changeset("r", &x).unwrap().parents);
        }
    }
    false
}

fn run(seed: u64, steps: usize) {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("sim.db");
    let blobs = Arc::new(MemBlobStore::default());
    let e = Engine::open(db.to_str().unwrap(), blobs.clone() as Arc<dyn BlobStore>).unwrap();
    let root = e.create_repo("r", "u").unwrap();
    let shared: String = (0..SHARED_LINES).map(|i| format!("line {i}\n")).collect();
    let b = e.put_blob(shared.as_bytes()).unwrap();
    let c = e
        .commit("r", "main", &root, "u", "shared", vec![FileChange { path: "shared.txt".into(), blob: Some(b), mode: 0o100644 }])
        .unwrap()
        .id;
    let mut sim =
        Sim { e, db, blobs, rng: Rng(seed.max(1)), model: BTreeMap::new(), landed: vec![], all: vec![root, c.clone()], main: c, n: 0 };
    let mut ops: HashMap<String, usize> = HashMap::new();
    for i in 0..steps {
        let what = sim.step();
        *ops.entry(what.split([' ', ':']).next().unwrap().to_string()).or_default() += 1;
        sim.check(&format!("step {i} (seed {seed}): {what}"));
    }
    // ancestry: the skip-list fast path agrees with a naive search on random pairs
    for _ in 0..200 {
        let (a, d) = (sim.all[sim.rng.below(sim.all.len())].clone(), sim.all[sim.rng.below(sim.all.len())].clone());
        assert_eq!(sim.e.is_ancestor("r", &a, &d).unwrap(), naive_ancestor(&sim.e, &a, &d), "seed {seed}: is_ancestor({a}, {d})");
    }
    eprintln!("sim seed {seed}: {steps} steps ok; ops {ops:?}; {} private files on main", sim.model.len());
}

#[test]
fn simulation_fixed_seeds() {
    for seed in [1, 42, 20261102] {
        run(seed, 150);
    }
}

#[test]
fn simulation_from_env() {
    let Some(seed) = std::env::var("CAMBIUM_SIM_SEED").ok().and_then(|s| s.parse().ok()) else { return };
    let steps = std::env::var("CAMBIUM_SIM_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(1000);
    run(seed, steps);
}
