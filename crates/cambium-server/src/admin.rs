//! Operator commands (run on a cmbd host or as a one-off pod with the same env):
//!
//!   cmbd token <user> [--groups a,b] [--ttl 90d|0] [--label text]   mint a static token
//!   cmbd token rotate <user> [--ttl 90d] [--grace 24h]              new token; old ones expire after the grace
//!   cmbd token revoke <user>                                        revoke all of a user's tokens
//!   cmbd token list                                                 metadata only, never secrets
//!   cmbd audit export [--since SEQ] [--out FILE]                    JSON lines for a SIEM
//!   cmbd audit verify                                               check the hash chain
//!   cmbd migrate [--status]                                         apply / show schema migrations
//!   cmbd backup <dir>                                               metadata + blobs, consistent snapshot
//!   cmbd restore <dir>                                              into an EMPTY database + blob store
//!   cmbd import-git <git-dir> <repo> [owner]                        one-time import with history
//!
//! Every command that changes state writes an audit event (actor `cli:<user>`).
use cambium_core::audit::AuditEvent;
use cambium_core::tokens::parse_ttl;
use cambium_core::Engine;
use serde_json::json;

fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    if i + 1 >= args.len() {
        eprintln!("{name} needs a value");
        std::process::exit(2);
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Some(v)
}
fn operator() -> String {
    format!("cli:{}", std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "operator".into()))
}
fn audit(eng: &Engine, action: &str, target: Option<String>, outcome: &str, detail: serde_json::Value) {
    if let Err(e) = eng.audit(AuditEvent {
        actor: operator(),
        action: action.into(),
        repo: None,
        target,
        outcome: outcome.into(),
        detail,
        request_id: None,
    }) {
        eprintln!("warning: audit write failed: {e}");
    }
}
fn ttl_or_exit(s: &str) -> Option<i64> {
    parse_ttl(s).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2)
    })
}
fn die(e: impl std::fmt::Display) -> i32 {
    eprintln!("error: {e}");
    1
}

/// Some(exit code) when argv names an operator command; None = run the server.
pub fn dispatch(eng: &Engine, _data: &str, argv: &[String]) -> Option<i32> {
    let mut a: Vec<String> = argv.to_vec();
    let cmd = a.get(1).cloned()?;
    Some(match cmd.as_str() {
        "token" => match a.get(2).map(String::as_str) {
            Some("list") => {
                for t in eng.list_tokens().unwrap_or_default() {
                    println!("{t}");
                }
                0
            }
            Some("revoke") => {
                let Some(u) = a.get(3).cloned() else {
                    eprintln!("usage: cmbd token revoke <user>");
                    return Some(2);
                };
                match eng.revoke_tokens(&u) {
                    Ok(n) => {
                        audit(eng, "token.revoke", Some(u.clone()), "success", json!({"count": n}));
                        println!("revoked {n} token(s) for {u}");
                        0
                    }
                    Err(e) => die(e),
                }
            }
            Some("rotate") => {
                let ttl =
                    ttl_or_exit(&flag(&mut a, "--ttl").unwrap_or_else(|| std::env::var("CAMBIUM_TOKEN_TTL").unwrap_or_else(|_| "90d".into())));
                let grace = ttl_or_exit(&flag(&mut a, "--grace").unwrap_or_else(|| "24h".into())).unwrap_or(0);
                let Some(u) = a.get(3).cloned() else {
                    eprintln!("usage: cmbd token rotate <user> [--ttl 90d] [--grace 24h]");
                    return Some(2);
                };
                match eng.rotate_token(&u, ttl, grace) {
                    Ok(t) => {
                        audit(eng, "token.rotate", Some(u.clone()), "success", json!({"ttl_s": ttl, "grace_s": grace}));
                        println!("{t}");
                        0
                    }
                    Err(e) => die(e),
                }
            }
            _ => {
                let groups: Vec<String> = flag(&mut a, "--groups")
                    .map(|g| g.split(',').filter(|x| !x.is_empty()).map(String::from).collect())
                    .unwrap_or_default();
                let ttl =
                    ttl_or_exit(&flag(&mut a, "--ttl").unwrap_or_else(|| std::env::var("CAMBIUM_TOKEN_TTL").unwrap_or_else(|_| "90d".into())));
                let label = flag(&mut a, "--label").unwrap_or_default();
                let u = a.get(2).cloned().unwrap_or_else(|| "admin".into());
                match eng.create_token_full(&u, &groups, ttl, &label) {
                    Ok(t) => {
                        audit(eng, "token.create", Some(u.clone()), "success", json!({"groups": groups, "ttl_s": ttl, "label": label}));
                        println!("{t}");
                        0
                    }
                    Err(e) => die(e),
                }
            }
        },
        "audit" => match a.get(2).map(String::as_str) {
            Some("export") => {
                let since: i64 = flag(&mut a, "--since").and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut out: Box<dyn std::io::Write> = match flag(&mut a, "--out") {
                    Some(f) => Box::new(std::fs::OpenOptions::new().create(true).append(true).open(f).expect("open --out")),
                    None => Box::new(std::io::stdout()),
                };
                let mut cur = since;
                let mut n = 0;
                loop {
                    let rows = match eng.audit_export(cur, 5000) {
                        Ok(r) => r,
                        Err(e) => return Some(die(e)),
                    };
                    if rows.is_empty() {
                        break;
                    }
                    for r in rows {
                        cur = r["seq"].as_i64().unwrap_or(cur);
                        writeln!(out, "{r}").expect("write");
                        n += 1;
                    }
                }
                eprintln!("exported {n} event(s); next --since {cur}");
                0
            }
            Some("verify") => match eng.verify_audit() {
                Ok((n, None)) => {
                    println!("audit chain intact: {n} event(s)");
                    0
                }
                Ok((n, Some(bad))) => {
                    println!("AUDIT CHAIN BROKEN at seq {bad} (after {n} good event(s))");
                    3
                }
                Err(e) => die(e),
            },
            _ => {
                eprintln!("usage: cmbd audit export [--since SEQ] [--out FILE] | cmbd audit verify");
                2
            }
        },
        "migrate" => {
            // the engine already applied pending migrations on open (CAMBIUM_MIGRATING=1)
            let rows = eng.db.query("SELECT version,name,applied_at FROM schema_migrations ORDER BY version", vec![]).unwrap_or_default();
            for r in &rows {
                println!("v{} {} applied_at={}", r.i(0), r.s(1), cambium_core::audit::rfc3339(r.i(2)));
            }
            println!("schema at v{} (binary supports up to v{})", rows.last().map_or(0, |r| r.i(0)), cambium_core::migrate::latest());
            if a.iter().all(|x| x != "--status") {
                audit(eng, "admin.migrate", None, "success", json!({"version": rows.last().map_or(0, |r| r.i(0))}));
            }
            0
        }
        "backup" => {
            let Some(dir) = a.get(2) else {
                eprintln!("usage: cmbd backup <dir>");
                return Some(2);
            };
            match eng.backup(std::path::Path::new(dir)) {
                Ok(r) => {
                    audit(eng, "admin.backup", Some(dir.clone()), "success", json!({"rows": r.rows, "blobs": r.blobs, "bytes": r.bytes}));
                    println!("backup ok: {} rows, {} blobs ({} bytes), schema v{} -> {dir}", r.rows, r.blobs, r.bytes, r.schema_version);
                    0
                }
                Err(e) => {
                    audit(eng, "admin.backup", Some(dir.clone()), "failure", json!({"error": e.to_string()}));
                    die(e)
                }
            }
        }
        "import-git" => {
            let (Some(dir), Some(repo)) = (a.get(2), a.get(3)) else {
                eprintln!("usage: cmbd import-git <git-dir> <repo> [owner]");
                return Some(2);
            };
            let owner = a.get(4).cloned().unwrap_or_else(|| "admin".into());
            match eng.import_git(std::path::Path::new(dir), repo, &owner) {
                Ok(r) => {
                    println!(
                        "imported {} commits ({} blobs) into {repo}; refs: {}",
                        r.commits,
                        r.blobs,
                        r.refs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                    );
                    for s in &r.skipped {
                        println!("skipped: {s}");
                    }
                    audit(eng, "admin.import_git", Some(repo.clone()), "success", json!({"commits": r.commits, "blobs": r.blobs}));
                    0
                }
                Err(e) => {
                    eprintln!("import failed: {e}");
                    1
                }
            }
        }
        _ => return None,
    })
}
use std::io::Write;

/// `cmbd restore <dir>`: opens the configured (empty) stores and loads the backup.
pub fn restore_cmd(data: &str) {
    let Some(dir) = std::env::args().nth(2) else {
        eprintln!("usage: cmbd restore <dir>");
        std::process::exit(2)
    };
    let eng = crate::open_engine(data, crate::blob_store(data));
    match eng.restore(std::path::Path::new(&dir)) {
        Ok(r) => {
            let chain = eng.verify_audit();
            audit(&eng, "admin.restore", Some(dir.clone()), "success", json!({"rows": r.rows, "blobs": r.blobs}));
            println!(
                "restore ok: {} rows, {} blobs ({} bytes); audit chain: {}",
                r.rows,
                r.blobs,
                r.bytes,
                match chain {
                    Ok((n, None)) => format!("intact ({n} events)"),
                    Ok((_, Some(s))) => format!("BROKEN at {s}"),
                    Err(e) => e.to_string(),
                }
            );
        }
        Err(e) => {
            eprintln!("restore failed (nothing was written to the database): {e}");
            std::process::exit(1)
        }
    }
}
