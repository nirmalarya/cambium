//! Versioned schema migrations.
//!
//! Version 1 is the base schema (`docs/schema*.sql`, idempotent, frozen).
//! Every later change is a numbered file under `docs/migrations/`, applied
//! once, in order, each inside its own transaction, and recorded in
//! `schema_migrations`. Postgres runs them under an advisory lock, so several
//! nodes starting together apply each migration exactly once. A database that
//! is newer than the binary is refused (no silent downgrade).
use crate::db::{Conn, Val};
use crate::engine::{Error, Result};

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    sqlite: &'static str,
    pg: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 2,
        name: "production",
        sqlite: include_str!("../../../docs/migrations/0002_production.sqlite.sql"),
        pg: include_str!("../../../docs/migrations/0002_production.pg.sql"),
    },
    Migration {
        version: 3,
        name: "blob_uploads",
        sqlite: include_str!("../../../docs/migrations/0003_blob_uploads.sqlite.sql"),
        pg: include_str!("../../../docs/migrations/0003_blob_uploads.pg.sql"),
    },
    Migration {
        version: 4,
        name: "session_token",
        sqlite: include_str!("../../../docs/migrations/0004_session_token.sqlite.sql"),
        pg: include_str!("../../../docs/migrations/0004_session_token.pg.sql"),
    },
];
pub fn latest() -> i64 {
    MIGRATIONS.last().map_or(1, |m| m.version)
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Versions recorded in the database (after the base schema has run).
pub fn applied(c: &mut dyn Conn) -> Result<Vec<i64>> {
    c.batch("CREATE TABLE IF NOT EXISTS schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)")?;
    c.exec("INSERT INTO schema_migrations(version,name,applied_at) VALUES(1,'base',?1) ON CONFLICT DO NOTHING", &[Val::Int(now())])?;
    Ok(c.query("SELECT version FROM schema_migrations ORDER BY version", &[])?.into_iter().map(|r| r.i(0)).collect())
}
/// Pending migrations, oldest first.
pub fn pending(c: &mut dyn Conn) -> Result<Vec<&'static Migration>> {
    let done = applied(c)?;
    if let Some(max) = done.iter().max() {
        if *max > latest() {
            return Err(Error::Db(format!(
                "database schema is v{max} but this cmbd only knows up to v{}: upgrade cmbd (refusing to downgrade)",
                latest()
            )));
        }
    }
    Ok(MIGRATIONS.iter().filter(|m| !done.contains(&m.version)).collect())
}
/// Apply pending migrations unless CAMBIUM_AUTO_MIGRATE=0, in which case pending
/// migrations are an error until `cmbd migrate` is run.
pub fn run(c: &mut dyn Conn) -> Result<Vec<i64>> {
    let todo = pending(c)?;
    if todo.is_empty() {
        return Ok(vec![]);
    }
    let auto = std::env::var("CAMBIUM_AUTO_MIGRATE").map(|v| v != "0").unwrap_or(true) || std::env::var("CAMBIUM_MIGRATING").is_ok();
    if !auto {
        return Err(Error::Db(format!("{} pending migration(s) (next: v{}); run `cmbd migrate` first", todo.len(), todo[0].version)));
    }
    let mut done = vec![];
    for m in todo {
        let sql = if c.is_pg() { m.pg } else { m.sqlite };
        c.batch("BEGIN")?;
        let r = c.batch(sql).and_then(|_| {
            c.exec(
                "INSERT INTO schema_migrations(version,name,applied_at) VALUES(?1,?2,?3)",
                &[Val::Int(m.version), Val::Text(m.name.into()), Val::Int(now())],
            )
            .map(|_| ())
        });
        match r {
            Ok(()) => c.batch("COMMIT")?,
            Err(e) => {
                let _ = c.batch("ROLLBACK");
                return Err(Error::Db(format!("migration v{} ({}) failed, rolled back: {e}", m.version, m.name)));
            }
        }
        crate::log::info("migration applied", serde_json::json!({"version": m.version, "name": m.name}));
        done.push(m.version);
    }
    Ok(done)
}
