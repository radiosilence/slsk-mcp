//! A one-time copy of the Postgres earlier versions kept their state in.
//!
//! It runs only into a database that holds nothing yet, so once anything is
//! in SQLite it is never read again. Each row comes out of Postgres as JSON
//! and goes into the table of the same name, column for column; the few
//! columns whose types differ between the two (UUIDs, byte strings, JSON
//! documents) are converted on the way.

use anyhow::{Context, Result, bail};
use serde_json::Value;
// Table names come from `TABLES`, and column names from those tables'
// own rows: nothing here is text from outside.
use sqlx::{AssertSqlSafe, Connection, Executor};
use uuid::Uuid;

use crate::db::Db;

/// In foreign-key order.
const TABLES: &[&str] = &[
    "accounts",
    "jobs",
    "job_files",
    "bans",
    "messages",
    "rooms",
    "buddies",
    "wishes",
    "interests",
    "job_events",
    "ui_sessions",
    "uploads",
    "totals",
    "served_users",
];

const UUIDS: &[&str] = &["id", "job_id"];
const BYTES: &[&str] = &["remote", "id_hash"];

pub async fn run(db: &Db, url: &str) -> Result<()> {
    let mut held = 0i64;
    for t in TABLES {
        let n: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {t}")))
            .fetch_one(&db.read)
            .await?;
        held += n;
    }
    if held > 0 {
        return Ok(());
    }
    let mut pg = sqlx::PgConnection::connect(url)
        .await
        .context("connecting")?;
    // Timestamps come out as text; in UTC they sort as the SQLite schema
    // expects.
    pg.execute("SET TIME ZONE 'UTC'").await?;
    let mut tx = db.write.begin().await?;
    for t in TABLES {
        let rows: Vec<Value> =
            sqlx::query_scalar(AssertSqlSafe(format!("SELECT row_to_json(t) FROM {t} t")))
                .fetch_all(&mut pg)
                .await
                .with_context(|| format!("reading {t}"))?;
        for row in &rows {
            let Value::Object(cols) = row else {
                bail!("{t}: a row that is not an object");
            };
            let names: Vec<&str> = cols.keys().map(String::as_str).collect();
            let sql = format!(
                "INSERT INTO {t} ({}) VALUES ({})",
                names.join(", "),
                (1..=names.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let mut q = sqlx::query(AssertSqlSafe(sql));
            for (name, v) in cols {
                q = match v {
                    Value::Null => q.bind(None::<String>),
                    Value::Bool(b) => q.bind(*b),
                    Value::Number(n) => match n.as_i64() {
                        Some(i) => q.bind(i),
                        None => q.bind(n.as_f64()),
                    },
                    Value::String(s) if UUIDS.contains(&name.as_str()) => {
                        q.bind(Uuid::parse_str(s).with_context(|| format!("{t}.{name}"))?)
                    }
                    Value::String(s) if BYTES.contains(&name.as_str()) => {
                        q.bind(bytea(s).with_context(|| format!("{t}.{name}"))?)
                    }
                    Value::String(s) => q.bind(s.clone()),
                    Value::Array(_) | Value::Object(_) => q.bind(v.to_string()),
                };
            }
            q.execute(&mut *tx)
                .await
                .with_context(|| format!("writing {t}"))?;
        }
        tracing::info!(table = t, rows = rows.len(), "imported from Postgres");
    }
    tx.commit().await?;
    Ok(())
}

/// Postgres's hex form of a byte string, `\x48656c6c6f`.
fn bytea(s: &str) -> Result<Vec<u8>> {
    let hex = s.strip_prefix("\\x").context("not a hex byte string")?;
    if hex.len() % 2 != 0 {
        bail!("odd-length hex");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).context("not hex"))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_postgres_hex_bytes() {
        assert_eq!(super::bytea("\\x48690a").unwrap(), b"Hi\n");
        assert!(super::bytea("48690a").is_err());
        assert!(super::bytea("\\x486").is_err());
    }
}
