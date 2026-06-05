use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags};

use crate::{
    calendar::local_date_from_unix_seconds,
    error::{AppError, AppResult},
    profile::DailyUsage,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadActivityReport {
    pub daily: Vec<DailyUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostThread {
    pub id: String,
    pub rollout_path: PathBuf,
    pub model: Option<String>,
    pub parent_thread_id: Option<String>,
    pub parent_rollout_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostThreadMetadata {
    pub id: String,
    pub model: Option<String>,
    pub parent_thread_id: Option<String>,
}

pub fn load_thread_activity(codex_home: &Path) -> AppResult<Option<ThreadActivityReport>> {
    let path = codex_home.join("state_5.sqlite");
    if !path.exists() {
        return Ok(None);
    }

    let connection = open_state_db(&path)?;

    let mut statement = connection
        .prepare(
            "
            SELECT
                created_at,
                CASE WHEN tokens_used > 0 THEN tokens_used ELSE 0 END AS tokens
            FROM threads
            WHERE created_at > 0
              AND (tokens_used > 0 OR has_user_event != 0)
            ORDER BY created_at
            ",
        )
        .map_err(|source| {
            AppError::sqlite(
                format!(
                    "failed to prepare thread activity query for {}",
                    path.display()
                ),
                source,
            )
        })?;

    let rows = statement
        .query_map([], |row| {
            let created_at: i64 = row.get(0)?;
            let tokens: i64 = row.get(1)?;
            Ok((created_at, tokens))
        })
        .map_err(|source| {
            AppError::sqlite(
                format!("failed to read thread activity from {}", path.display()),
                source,
            )
        })?;

    let mut by_day = BTreeMap::<String, DailyUsageAccumulator>::new();
    for row in rows {
        let (created_at, tokens) = row.map_err(|source| {
            AppError::sqlite(
                format!("failed to decode thread activity from {}", path.display()),
                source,
            )
        })?;
        let Some(date) = u64::try_from(created_at)
            .ok()
            .and_then(local_date_from_unix_seconds)
        else {
            continue;
        };
        let day = by_day.entry(date).or_default();
        day.tokens = day
            .tokens
            .saturating_add(u64::try_from(tokens.max(0)).unwrap_or(0));
        day.sessions = day.sessions.saturating_add(1);
    }

    let daily = by_day
        .into_iter()
        .map(|(date, day)| DailyUsage {
            date,
            tokens: day.tokens,
            cost_usd: None,
            sessions: day.sessions,
        })
        .collect::<Vec<_>>();

    if daily.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ThreadActivityReport { daily }))
    }
}

#[derive(Default)]
struct DailyUsageAccumulator {
    tokens: u64,
    sessions: u64,
}

pub fn load_cost_thread_metadata(
    codex_home: &Path,
) -> AppResult<HashMap<PathBuf, CostThreadMetadata>> {
    let path = codex_home.join("state_5.sqlite");
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let connection = open_state_db(&path)?;
    let mut statement = connection
        .prepare(
            "
            SELECT t.id, t.rollout_path, t.model, e.parent_thread_id
            FROM threads t
            LEFT JOIN thread_spawn_edges e ON e.child_thread_id = t.id
            WHERE t.rollout_path != ''
            ",
        )
        .map_err(|source| {
            AppError::sqlite(
                format!(
                    "failed to prepare cost thread metadata query for {}",
                    path.display()
                ),
                source,
            )
        })?;

    let rows = statement
        .query_map([], |row| {
            let rollout_path: String = row.get(1)?;
            Ok((
                PathBuf::from(rollout_path),
                CostThreadMetadata {
                    id: row.get(0)?,
                    model: row.get(2)?,
                    parent_thread_id: row.get(3)?,
                },
            ))
        })
        .map_err(|source| {
            AppError::sqlite(
                format!(
                    "failed to read cost thread metadata from {}",
                    path.display()
                ),
                source,
            )
        })?;

    let mut metadata = HashMap::new();
    for row in rows {
        let (path, thread) = row.map_err(|source| {
            AppError::sqlite(
                format!(
                    "failed to decode cost thread metadata from {}",
                    path.display()
                ),
                source,
            )
        })?;
        metadata.insert(path, thread);
    }
    Ok(metadata)
}

fn open_state_db(path: &Path) -> AppResult<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| AppError::sqlite(format!("failed to open {}", path.display()), source))?;
    connection
        .busy_timeout(Duration::from_millis(250))
        .map_err(|source| {
            AppError::sqlite(
                format!("failed to configure SQLite timeout for {}", path.display()),
                source,
            )
        })?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn loads_daily_thread_activity_from_state_db() {
        let root = unique_temp_dir("codex-meter-state-db");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("state_5.sqlite");
        let connection = Connection::open(&path).expect("open db");
        connection
            .execute_batch(
                "
                CREATE TABLE threads (
                    created_at INTEGER NOT NULL,
                    tokens_used INTEGER NOT NULL DEFAULT 0,
                    has_user_event INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO threads VALUES (1780444800, 10, 1);
                INSERT INTO threads VALUES (1780444900, 20, 0);
                INSERT INTO threads VALUES (1780531200, 0, 1);
                INSERT INTO threads VALUES (1780617600, 0, 0);
                ",
            )
            .expect("seed db");
        drop(connection);

        let report = load_thread_activity(&root).expect("load").expect("report");
        let first_day = expected_local_date_from_unix_seconds(1780444800);
        let second_day = expected_local_date_from_unix_seconds(1780531200);

        assert_eq!(
            report.daily,
            vec![
                DailyUsage {
                    date: first_day,
                    tokens: 30,
                    cost_usd: None,
                    sessions: 2,
                },
                DailyUsage {
                    date: second_day,
                    tokens: 0,
                    cost_usd: None,
                    sessions: 1,
                },
            ]
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn loads_cost_thread_metadata_by_rollout_path() {
        let root = unique_temp_dir("codex-meter-cost-metadata");
        fs::create_dir_all(&root).expect("create root");
        let rollout_path = root.join("session.jsonl");
        let path = root.join("state_5.sqlite");
        let connection = Connection::open(&path).expect("open db");
        connection
            .execute_batch(&format!(
                "
                CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    model TEXT
                );
                CREATE TABLE thread_spawn_edges (
                    parent_thread_id TEXT NOT NULL,
                    child_thread_id TEXT NOT NULL PRIMARY KEY,
                    status TEXT NOT NULL
                );
                INSERT INTO threads VALUES ('thread-one', '{}', 'gpt-5.5');
                ",
                rollout_path.display()
            ))
            .expect("seed db");
        drop(connection);

        let metadata = load_cost_thread_metadata(&root).expect("load");

        assert_eq!(
            metadata.get(&rollout_path),
            Some(&CostThreadMetadata {
                id: "thread-one".to_string(),
                model: Some("gpt-5.5".to_string()),
                parent_thread_id: None,
            })
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }

    fn expected_local_date_from_unix_seconds(seconds: i64) -> String {
        chrono::DateTime::from_timestamp(seconds, 0)
            .expect("timestamp")
            .with_timezone(&chrono::Local)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    }
}
