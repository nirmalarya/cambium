//! Metadata store: SQLite (single node) or Postgres (several cmbd nodes on one DB).
//! SQL is written once in a portable subset (`?N` placeholders, ON CONFLICT, RETURNING)
//! and translated to `$N` for Postgres.
use crate::engine::{Error, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Null,
    Int(i64),
    Text(String),
}
impl From<&str> for Val {
    fn from(s: &str) -> Self {
        Val::Text(s.into())
    }
}
impl From<&String> for Val {
    fn from(s: &String) -> Self {
        Val::Text(s.clone())
    }
}
impl From<String> for Val {
    fn from(s: String) -> Self {
        Val::Text(s)
    }
}
impl From<i64> for Val {
    fn from(i: i64) -> Self {
        Val::Int(i)
    }
}
impl From<u32> for Val {
    fn from(i: u32) -> Self {
        Val::Int(i as i64)
    }
}
impl<T: Into<Val>> From<Option<T>> for Val {
    fn from(o: Option<T>) -> Self {
        o.map_or(Val::Null, Into::into)
    }
}

#[macro_export]
macro_rules! p { ($($x:expr),* $(,)?) => { vec![$($crate::db::Val::from($x)),*] }; }

pub struct Row(pub Vec<Val>);
impl Row {
    pub fn s(&self, i: usize) -> String {
        match &self.0[i] {
            Val::Text(s) => s.clone(),
            Val::Int(n) => n.to_string(),
            Val::Null => String::new(),
        }
    }
    pub fn os(&self, i: usize) -> Option<String> {
        match &self.0[i] {
            Val::Null => None,
            _ => Some(self.s(i)),
        }
    }
    pub fn i(&self, i: usize) -> i64 {
        match &self.0[i] {
            Val::Int(n) => *n,
            Val::Text(s) => s.parse().unwrap_or(0),
            Val::Null => 0,
        }
    }
    pub fn oi(&self, i: usize) -> Option<i64> {
        match &self.0[i] {
            Val::Null => None,
            _ => Some(self.i(i)),
        }
    }
}

/// One connection (or open transaction).
pub trait Conn {
    fn exec(&mut self, sql: &str, p: &[Val]) -> Result<u64>;
    fn query(&mut self, sql: &str, p: &[Val]) -> Result<Vec<Row>>;
    fn one(&mut self, sql: &str, p: &[Val]) -> Result<Option<Row>> {
        Ok(self.query(sql, p)?.into_iter().next())
    }
    fn is_pg(&self) -> bool;
    /// Several statements, no parameters (migrations).
    fn batch(&mut self, sql: &str) -> Result<()>;
}

// ---------- SQLite ----------
fn sq_params(p: &[Val]) -> Vec<rusqlite::types::Value> {
    p.iter()
        .map(|v| match v {
            Val::Null => rusqlite::types::Value::Null,
            Val::Int(i) => (*i).into(),
            Val::Text(s) => s.clone().into(),
        })
        .collect()
}
fn sq_query(c: &rusqlite::Connection, sql: &str, p: &[Val]) -> Result<Vec<Row>> {
    let mut st = c.prepare_cached(sql)?;
    let n = st.column_count();
    let mut rows = st.query(rusqlite::params_from_iter(sq_params(p)))?;
    let mut out = vec![];
    while let Some(r) = rows.next()? {
        out.push(Row((0..n)
            .map(|i| match r.get_ref(i).unwrap() {
                rusqlite::types::ValueRef::Null => Val::Null,
                rusqlite::types::ValueRef::Integer(x) => Val::Int(x),
                rusqlite::types::ValueRef::Real(x) => Val::Int(x as i64),
                rusqlite::types::ValueRef::Text(t) | rusqlite::types::ValueRef::Blob(t) => Val::Text(String::from_utf8_lossy(t).into()),
            })
            .collect()));
    }
    Ok(out)
}
impl Conn for rusqlite::Connection {
    fn exec(&mut self, sql: &str, p: &[Val]) -> Result<u64> {
        Ok(self.prepare_cached(sql)?.execute(rusqlite::params_from_iter(sq_params(p)))? as u64)
    }
    fn query(&mut self, sql: &str, p: &[Val]) -> Result<Vec<Row>> {
        sq_query(self, sql, p)
    }
    fn is_pg(&self) -> bool {
        false
    }
    fn batch(&mut self, sql: &str) -> Result<()> {
        Ok(self.execute_batch(sql)?)
    }
}
impl Conn for rusqlite::Transaction<'_> {
    fn exec(&mut self, sql: &str, p: &[Val]) -> Result<u64> {
        Ok(self.prepare_cached(sql)?.execute(rusqlite::params_from_iter(sq_params(p)))? as u64)
    }
    fn query(&mut self, sql: &str, p: &[Val]) -> Result<Vec<Row>> {
        sq_query(self, sql, p)
    }
    fn is_pg(&self) -> bool {
        false
    }
    fn batch(&mut self, sql: &str) -> Result<()> {
        Ok(self.execute_batch(sql)?)
    }
}

// ---------- Postgres ----------
pub fn to_pg(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut it = sql.chars().peekable();
    while let Some(c) = it.next() {
        if c == '?' && it.peek().is_some_and(|d| d.is_ascii_digit()) {
            out.push('$');
        } else {
            out.push(c);
        }
    }
    out
}
impl postgres::types::ToSql for Val {
    fn to_sql(
        &self,
        ty: &postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> std::result::Result<postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        use postgres::types::Type;
        match self {
            Val::Null => Ok(postgres::types::IsNull::Yes),
            Val::Int(i) => match *ty {
                Type::INT2 => (*i as i16).to_sql(ty, out),
                Type::INT4 => (*i as i32).to_sql(ty, out),
                Type::TEXT | Type::VARCHAR => i.to_string().to_sql(ty, out),
                _ => i.to_sql(ty, out),
            },
            Val::Text(s) => s.to_sql(ty, out),
        }
    }
    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
    postgres::types::to_sql_checked!();
}
fn pg_rows(rows: Vec<postgres::Row>) -> Vec<Row> {
    use postgres::types::Type;
    rows.into_iter()
        .map(|r| {
            Row(r
                .columns()
                .iter()
                .enumerate()
                .map(|(i, c)| match *c.type_() {
                    // try_get: a column type we do not model (e.g. void) becomes NULL instead of panicking
                    Type::INT2 => r.try_get::<_, Option<i16>>(i).ok().flatten().map_or(Val::Null, |x| Val::Int(x as i64)),
                    Type::INT4 => r.try_get::<_, Option<i32>>(i).ok().flatten().map_or(Val::Null, |x| Val::Int(x as i64)),
                    Type::INT8 => r.try_get::<_, Option<i64>>(i).ok().flatten().map_or(Val::Null, Val::Int),
                    Type::BOOL => r.try_get::<_, Option<bool>>(i).ok().flatten().map_or(Val::Null, |x| Val::Int(x as i64)),
                    _ => r.try_get::<_, Option<String>>(i).ok().flatten().map_or(Val::Null, Val::Text),
                })
                .collect())
        })
        .collect()
}
fn refs(p: &[Val]) -> Vec<&(dyn postgres::types::ToSql + Sync)> {
    p.iter().map(|v| v as _).collect()
}
impl Conn for postgres::Client {
    fn exec(&mut self, sql: &str, p: &[Val]) -> Result<u64> {
        Ok(self.execute(to_pg(sql).as_str(), &refs(p))?)
    }
    fn query(&mut self, sql: &str, p: &[Val]) -> Result<Vec<Row>> {
        Ok(pg_rows(postgres::Client::query(self, to_pg(sql).as_str(), &refs(p))?))
    }
    fn is_pg(&self) -> bool {
        true
    }
    fn batch(&mut self, sql: &str) -> Result<()> {
        Ok(self.batch_execute(sql)?)
    }
}
impl Conn for postgres::Transaction<'_> {
    fn exec(&mut self, sql: &str, p: &[Val]) -> Result<u64> {
        Ok(self.execute(to_pg(sql).as_str(), &refs(p))?)
    }
    fn query(&mut self, sql: &str, p: &[Val]) -> Result<Vec<Row>> {
        Ok(pg_rows(postgres::Transaction::query(self, to_pg(sql).as_str(), &refs(p))?))
    }
    fn is_pg(&self) -> bool {
        true
    }
    fn batch(&mut self, sql: &str) -> Result<()> {
        Ok(self.batch_execute(sql)?)
    }
}

type Job = Box<dyn FnOnce(&mut postgres::Client) + Send>;

/// Store handle. Postgres work runs on a pool of worker threads (the sync client
/// must not run inside the server's async runtime). Workers share one job queue,
/// so an idle connection picks up the next job; a dropped connection is
/// re-established before the next job instead of failing every later request.
pub enum Db {
    Sqlite(Mutex<rusqlite::Connection>),
    Pg { queue: Mutex<mpsc::Sender<Job>>, stats: Arc<PoolStats> },
}
#[derive(Default, Debug)]
pub struct PoolStats {
    pub size: AtomicUsize,
    pub busy: AtomicUsize,
    pub queued: AtomicUsize,
    pub reconnects: AtomicUsize,
    pub healthy: AtomicUsize,
    pub panics: AtomicUsize,
}

pub const PG_SCHEMA: &str = include_str!("../../../docs/schema-pg.sql");

/// How to reach Postgres. TLS: CAMBIUM_DB_TLS=require (verify against the system
/// roots, or CAMBIUM_DB_CA=/path/ca.pem for an internal CA); default is no TLS for
/// a database on the same host or a private network segment.
pub fn pg_connect(url: &str) -> Result<postgres::Client> {
    let mut c = match std::env::var("CAMBIUM_DB_TLS").as_deref() {
        Ok("require") => {
            let mut roots = rustls::RootCertStore::empty();
            match std::env::var("CAMBIUM_DB_CA") {
                Ok(p) => {
                    let pem = std::fs::read(&p).map_err(|e| Error::Db(format!("CAMBIUM_DB_CA {p}: {e}")))?;
                    for c in <rustls_pki_types::CertificateDer as rustls_pki_types::pem::PemObject>::pem_slice_iter(&pem) {
                        roots
                            .add(c.map_err(|e| Error::Db(format!("CAMBIUM_DB_CA: {e}")))?)
                            .map_err(|e| Error::Db(format!("CAMBIUM_DB_CA: {e}")))?;
                    }
                }
                Err(_) => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            }
            let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| Error::Db(e.to_string()))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            postgres::Client::connect(url, tokio_postgres_rustls::MakeRustlsConnect::new(cfg))?
        }
        _ => postgres::Client::connect(url, postgres::NoTls)?,
    };
    // bound every statement so a stuck query cannot hold a worker forever
    let ms: u64 = std::env::var("CAMBIUM_DB_STATEMENT_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30_000);
    c.batch_execute(&format!("SET statement_timeout = {ms}"))?;
    Ok(c)
}

impl Db {
    pub fn open_pg(url: &str, pool: usize) -> Result<Db> {
        {
            let mut c = pg_connect(url)?;
            // serialize schema setup across nodes starting at the same time; separate
            // round trips so the DDL commits before the lock is released
            c.batch_execute("SELECT pg_advisory_lock(7171)")?;
            let r = c.batch_execute(PG_SCHEMA).map_err(Error::from).and_then(|_| crate::migrate::run(&mut c));
            c.batch_execute("SELECT pg_advisory_unlock(7171)")?;
            r?;
        }
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let stats = Arc::new(PoolStats::default());
        for _ in 0..pool.max(1) {
            let mut c = pg_connect(url)?;
            let (rx, st, url) = (rx.clone(), stats.clone(), url.to_string());
            st.size.fetch_add(1, Ordering::Relaxed);
            st.healthy.fetch_add(1, Ordering::Relaxed);
            std::thread::spawn(move || loop {
                let job = match rx.lock().unwrap().recv() {
                    Ok(j) => j,
                    Err(_) => return,
                };
                st.queued.fetch_sub(1, Ordering::Relaxed);
                if c.is_closed() {
                    st.healthy.fetch_sub(1, Ordering::Relaxed);
                    for i in 0..5u64 {
                        if let Ok(n) = pg_connect(&url) {
                            c = n;
                            st.reconnects.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200 * (i + 1)));
                    }
                    if !c.is_closed() {
                        st.healthy.fetch_add(1, Ordering::Relaxed);
                    }
                }
                st.busy.fetch_add(1, Ordering::Relaxed);
                // a panicking job must not take the pool worker down with it; the caller
                // sees its reply channel close ("worker gone") and the connection is reset
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut c))).is_err() {
                    st.panics.fetch_add(1, Ordering::Relaxed);
                    let _ = c.batch_execute("ROLLBACK");
                }
                st.busy.fetch_sub(1, Ordering::Relaxed);
            });
        }
        Ok(Db::Pg { queue: Mutex::new(tx), stats })
    }
    pub fn pool_stats(&self) -> Option<&PoolStats> {
        match self {
            Db::Pg { stats, .. } => Some(stats),
            _ => None,
        }
    }
    /// Cheap liveness probe of the store (readiness checks).
    pub fn ping(&self) -> Result<()> {
        self.with(|c| c.query("SELECT 1", &[]).map(|_| ()))
    }
    /// Run `f` with a plain connection (each statement autocommits).
    pub fn with<R: Send + 'static>(&self, f: impl FnOnce(&mut dyn Conn) -> Result<R> + Send + 'static) -> Result<R> {
        match self {
            Db::Sqlite(m) => f(&mut *m.lock().unwrap()),
            Db::Pg { .. } => self.pg(move |c| f(c)),
        }
    }
    /// Run `f` in one transaction; commit on Ok, roll back on Err.
    pub fn tx<R: Send + 'static>(&self, f: impl FnOnce(&mut dyn Conn) -> Result<R> + Send + 'static) -> Result<R> {
        match self {
            Db::Sqlite(m) => {
                let mut c = m.lock().unwrap();
                let mut t = c.transaction()?;
                let r = f(&mut t)?;
                t.commit()?;
                Ok(r)
            }
            Db::Pg { .. } => self.pg(move |c| {
                let mut t = c.transaction()?;
                let r = f(&mut t)?;
                t.commit()?;
                Ok(r)
            }),
        }
    }
    /// A long read-only transaction over a consistent snapshot that does not block
    /// other requests. SQLite: a separate read-only connection (WAL mode lets it read
    /// a snapshot while writers continue). Postgres: one pool connection.
    pub fn snapshot_tx<R: Send + 'static>(&self, f: impl FnOnce(&mut dyn Conn) -> Result<R> + Send + 'static) -> Result<R> {
        match self {
            Db::Sqlite(m) => {
                let path = m.lock().unwrap().path().filter(|p| !p.is_empty()).map(String::from);
                let Some(path) = path else { return self.tx(f) }; // in-memory: no second connection
                let mut c = rusqlite::Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                c.busy_timeout(std::time::Duration::from_secs(5))?;
                let mut t = c.transaction()?;
                let r = f(&mut t)?;
                t.commit()?;
                Ok(r)
            }
            Db::Pg { .. } => self.tx(f),
        }
    }
    fn pg<R: Send + 'static>(&self, f: impl FnOnce(&mut postgres::Client) -> Result<R> + Send + 'static) -> Result<R> {
        let Db::Pg { queue, stats } = self else { unreachable!() };
        let (rtx, rrx) = mpsc::channel();
        stats.queued.fetch_add(1, Ordering::Relaxed);
        queue
            .lock()
            .unwrap()
            .send(Box::new(move |c: &mut postgres::Client| {
                let _ = rtx.send(f(c));
            }))
            .map_err(|_| Error::Db("postgres pool gone".into()))?;
        rrx.recv().map_err(|_| Error::Db("postgres worker gone".into()))?
    }
    pub fn exec(&self, sql: &str, p: Vec<Val>) -> Result<u64> {
        let s = sql.to_string();
        self.with(move |c| c.exec(&s, &p))
    }
    pub fn query(&self, sql: &str, p: Vec<Val>) -> Result<Vec<Row>> {
        let s = sql.to_string();
        self.with(move |c| c.query(&s, &p))
    }
    pub fn one(&self, sql: &str, p: Vec<Val>) -> Result<Option<Row>> {
        Ok(self.query(sql, p)?.into_iter().next())
    }
    pub fn is_pg(&self) -> bool {
        matches!(self, Db::Pg { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A snapshot read does not hold the main connection: writes proceed during it
    /// (the old shared-transaction backup would deadlock here) and stay invisible
    /// to the snapshot.
    #[test]
    fn snapshot_tx_does_not_block_writers() {
        let d = tempfile::tempdir().unwrap();
        let c = rusqlite::Connection::open(d.path().join("s.db")).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x INTEGER); INSERT INTO t VALUES(1);").unwrap();
        let db = std::sync::Arc::new(Db::Sqlite(Mutex::new(c)));
        let w = db.clone();
        let seen = db
            .snapshot_tx(move |s| {
                let before = s.query("SELECT count(*) FROM t", &[])?[0].i(0);
                w.exec("INSERT INTO t VALUES(2)", vec![])?; // main connection, during the snapshot
                let after = s.query("SELECT count(*) FROM t", &[])?[0].i(0);
                Ok((before, after))
            })
            .unwrap();
        assert_eq!(seen, (1, 1), "snapshot is stable");
        assert_eq!(db.one("SELECT count(*) FROM t", vec![]).unwrap().unwrap().i(0), 2);
    }
}
