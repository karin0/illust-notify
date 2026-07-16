use crate::{AppState, Item};
use anyhow::Result;
use pixiv::client::AuthedState;
use pixiv::model::IllustId;
use rusqlite::{Connection, params};
use time::OffsetDateTime;

pub fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;",
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS State (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            iid INTEGER NOT NULL,
            since TEXT NOT NULL,
            api TEXT NOT NULL
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS Seen (
            illust_id INTEGER PRIMARY KEY
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS Illust (
            id INTEGER PRIMARY KEY,
            data TEXT NOT NULL,
            archived_at TEXT NOT NULL
        )",
        [],
    )?;

    conn.execute(
        "INSERT OR IGNORE INTO State (id, iid, since, api)
         VALUES (1, 0, '1970-01-01T00:00:00Z', '')",
        [],
    )?;

    Ok(())
}

pub fn load_state(conn: &Connection) -> Result<(AppState, AuthedState)> {
    let mut stmt = conn.prepare("SELECT iid, since, api FROM State WHERE id = 1")?;

    let state_row = stmt.query_row([], |row| {
        let iid: IllustId = row.get(0)?;
        let since_str: String = row.get(1)?;
        let api_str: String = row.get(2)?;
        Ok((iid, since_str, api_str))
    });

    let (iid, since_str, api_str) = match state_row {
        Ok(data) => data,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            anyhow::bail!("No state found in database");
        }
        Err(e) => return Err(e.into()),
    };

    // Deserialize API state
    if api_str.is_empty() {
        anyhow::bail!("Empty state");
    }

    // Parse since
    let since = OffsetDateTime::parse(
        &since_str,
        &time::format_description::well_known::Iso8601::DEFAULT,
    )?;

    let api: AuthedState = serde_json::from_str(&api_str)?;

    Ok((
        AppState {
            iid,
            since,
            remain: false,
            skip: false,
        },
        api,
    ))
}

pub fn save_state(conn: &Connection, state: &AppState) -> Result<()> {
    let since_str = state
        .since
        .format(&time::format_description::well_known::Iso8601::DEFAULT)?;

    conn.execute(
        "UPDATE State SET iid = ?, since = ? WHERE id = 1",
        params![state.iid, since_str],
    )?;

    Ok(())
}

pub fn save_token(conn: &Connection, api_state: &AuthedState) -> Result<()> {
    let api_str = serde_json::to_string(api_state)?;

    conn.execute("UPDATE State SET api = ? WHERE id = 1", params![api_str])?;

    Ok(())
}

pub fn get_illust_data(conn: &Connection, id: IllustId) -> Result<Option<String>> {
    let mut stmt = conn.prepare_cached("SELECT data FROM Illust WHERE id = ?")?;
    match stmt.query_row(params![id], |row| row.get::<_, String>(0)) {
        Ok(data) => Ok(Some(data)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn is_seen(conn: &Connection, id: IllustId) -> Result<bool> {
    let mut stmt = conn.prepare_cached("SELECT 1 FROM Seen WHERE illust_id = ?")?;
    let exists = stmt.exists(params![id])?;
    Ok(exists)
}

pub fn get_seen_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM Seen", [], |row| row.get(0))?;
    let count = usize::try_from(count).unwrap_or(0);
    Ok(count)
}

pub fn reset_seen(conn: &Connection, ids: impl Iterator<Item = IllustId>) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;

    tx.execute("DELETE FROM Seen", [])?;

    let mut inserted = 0;
    {
        let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO Seen (illust_id) VALUES (?)")?;

        for id in ids {
            inserted += stmt.execute(params![id])?;
        }
    }

    tx.commit()?;
    Ok(inserted)
}

pub fn extend_seen(conn: &Connection, ids: impl Iterator<Item = IllustId>) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;

    let mut inserted = 0;
    {
        let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO Seen (illust_id) VALUES (?)")?;

        for id in ids {
            inserted += stmt.execute(params![id])?;
        }
    }

    tx.commit()?;
    Ok(inserted)
}

pub fn archive_illusts(conn: &Connection, illusts: &[Item]) -> Result<()> {
    debug!("archiving {} illusts", illusts.len());
    if illusts.is_empty() {
        return Ok(());
    }

    let now = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Iso8601::DEFAULT)?;

    let tx = conn.unchecked_transaction()?;

    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO Illust (id, data, archived_at) VALUES (?, ?, ?)",
        )?;

        for item in illusts {
            stmt.execute(params![item.iid, item.data.get(), now])?;
        }
    }

    tx.commit()?;
    Ok(())
}
