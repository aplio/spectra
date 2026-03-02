use rusqlite::{Connection, Result, params};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct CommandHistory {
    conn: Option<Connection>,
}

impl CommandHistory {
    pub fn from_xdg() -> Self {
        Self::new_with_data_dir(crate::xdg::app_data_dir())
    }

    pub fn new_with_data_dir(data_dir: PathBuf) -> Self {
        let conn = Self::init_db(data_dir).ok();
        Self { conn }
    }

    fn init_db(data_dir: PathBuf) -> Result<Connection> {
        std::fs::create_dir_all(&data_dir).ok();
        let db_path = data_dir.join("command-history.db");
        let conn = Connection::open(db_path)?;
        Self::init_schema(&conn)?;
        Ok(conn)
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS command_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                command_id TEXT NOT NULL UNIQUE,
                last_used_at INTEGER NOT NULL,
                use_count INTEGER NOT NULL DEFAULT 1
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_command_history_last_used
             ON command_history(last_used_at DESC)",
            [],
        )?;
        Ok(())
    }

    pub fn record_execution(&self, command_id: &str) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        conn.execute(
            "INSERT INTO command_history (command_id, last_used_at, use_count)
             VALUES (?1, ?2, 1)
             ON CONFLICT(command_id)
             DO UPDATE SET
                last_used_at = ?2,
                use_count = use_count + 1",
            params![command_id, timestamp],
        )?;
        Ok(())
    }

    pub fn get_recent_commands(&self, limit: usize) -> Vec<String> {
        let Some(conn) = &self.conn else {
            return Vec::new();
        };

        let mut stmt = match conn.prepare(
            "SELECT command_id
             FROM command_history
             ORDER BY last_used_at DESC
             LIMIT ?1",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };

        let rows = stmt.query_map([limit as i64], |row| row.get::<_, String>(0));
        match rows {
            Ok(rows) => rows.filter_map(|row| row.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    #[cfg(test)]
    fn db_path(data_dir: &std::path::Path) -> PathBuf {
        data_dir.join("command-history.db")
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::CommandHistory;

    #[test]
    fn records_and_orders_by_recency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let history = CommandHistory::new_with_data_dir(dir.path().to_path_buf());

        history
            .record_execution("session.new")
            .expect("record first");
        thread::sleep(Duration::from_millis(2));
        history
            .record_execution("window.new")
            .expect("record second");
        thread::sleep(Duration::from_millis(2));
        history
            .record_execution("session.new")
            .expect("record third");

        let recent = history.get_recent_commands(10);
        assert_eq!(recent.first().map(String::as_str), Some("session.new"));
        assert_eq!(recent.get(1).map(String::as_str), Some("window.new"));
    }

    #[test]
    fn persists_across_instances() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_path_buf();

        let first = CommandHistory::new_with_data_dir(data_dir.clone());
        first.record_execution("client.detach").expect("record");
        assert!(CommandHistory::db_path(&data_dir).exists());

        let second = CommandHistory::new_with_data_dir(data_dir);
        let recent = second.get_recent_commands(10);
        assert_eq!(recent.first().map(String::as_str), Some("client.detach"));
    }
}
