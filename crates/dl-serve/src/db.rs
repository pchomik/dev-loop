use rusqlite::{Connection, Row, params};
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::fs;

pub type Result<T> = std::result::Result<T, String>;

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL,
    issue TEXT,
    source TEXT,
    request TEXT NOT NULL,
    short_description TEXT,
    status TEXT NOT NULL,
    loop_name TEXT NOT NULL,
    step TEXT NOT NULL,
    iteration INTEGER NOT NULL,
    loop_elapsed INTEGER NOT NULL,
    cost REAL NOT NULL,
    worktree TEXT,
    review_note TEXT,
    message TEXT,
    created_at INTEGER NOT NULL,
    run_secs INTEGER NOT NULL DEFAULT 0,
    key TEXT
);";

const COLS: &str = "id, path, issue, source, request, short_description, status, loop_name, step, iteration, loop_elapsed, cost, worktree, review_note, message, created_at, run_secs, key";

#[derive(Serialize, Clone, Debug)]
pub struct Task {
    pub id: String,
    pub path: String,
    pub issue: Option<String>,
    pub source: Option<String>,
    pub request: String,
    pub short_description: Option<String>,
    /// new, running, waiting_review, done, failed, stopped
    pub status: String,
    pub loop_name: String,
    pub step: String,
    pub iteration: i64,
    /// Agent run seconds spent in the current loop.
    pub loop_elapsed: i64,
    pub cost: f64,
    pub worktree: Option<String>,
    pub review_note: Option<String>,
    pub message: Option<String>,
    pub created_at: i64,
    /// Agent run seconds of all completed steps, across loops.
    pub run_secs: i64,
    /// Folder name and commit key. Set on the first run and kept after a prefix change.
    pub key: Option<String>,
}

impl Task {
    pub fn new(path: &str, issue: Option<&str>, source: Option<&str>, request: &str, first_step: &str) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Task {
            id: String::new(),
            path: path.into(),
            issue: issue.map(Into::into),
            source: source.map(Into::into),
            request: request.into(),
            short_description: None,
            status: "new".into(),
            loop_name: "definition".into(),
            step: first_step.into(),
            iteration: 1,
            loop_elapsed: 0,
            cost: 0.0,
            worktree: None,
            review_note: None,
            message: None,
            created_at,
            run_secs: 0,
            key: None,
        }
    }

    pub fn label(&self) -> String {
        self.short_description.clone().unwrap_or_else(|| self.id.clone())
    }
}

fn from_row(r: &Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get::<_, i64>(0)?.to_string(),
        path: r.get(1)?,
        issue: r.get(2)?,
        source: r.get(3)?,
        request: r.get(4)?,
        short_description: r.get(5)?,
        status: r.get(6)?,
        loop_name: r.get(7)?,
        step: r.get(8)?,
        iteration: r.get(9)?,
        loop_elapsed: r.get(10)?,
        cost: r.get(11)?,
        worktree: r.get(12)?,
        review_note: r.get(13)?,
        message: r.get(14)?,
        created_at: r.get(15)?,
        run_secs: r.get(16)?,
        key: r.get(17)?,
    })
}

pub struct Db(Connection);

pub fn open(path: &Path) -> Result<Db> {
    fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).map_err(es)?;
    let c = Connection::open(path).map_err(es)?;
    c.busy_timeout(Duration::from_secs(5)).map_err(es)?;
    c.execute_batch(SCHEMA).map_err(es)?;
    // `CREATE TABLE IF NOT EXISTS` skips an existing table, so add the column it lacks.
    let has_run_secs: i64 = c
        .query_row("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='run_secs'", [], |r| r.get(0))
        .map_err(es)?;
    if has_run_secs == 0 {
        c.execute_batch("ALTER TABLE tasks ADD COLUMN run_secs INTEGER NOT NULL DEFAULT 0").map_err(es)?;
    }
    let has_key: i64 = c
        .query_row("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='key'", [], |r| r.get(0))
        .map_err(es)?;
    if has_key == 0 {
        // Tasks before the prefix change keep their bare ID. Only a prefix changes the key of no-issue, no-source tasks.
        c.execute_batch(
            "ALTER TABLE tasks ADD COLUMN key TEXT;
             UPDATE tasks SET key = CAST(id AS TEXT) WHERE issue IS NULL AND source IS NULL;",
        )
        .map_err(es)?;
    }
    Ok(Db(c))
}

impl Db {
    /// Stores the task and sets its ID to the next index, starting at 1.
    pub fn insert(&self, t: &mut Task) -> Result<()> {
        self.0
            .execute(
                &format!("INSERT INTO tasks ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)"),
                params![
                    None::<i64>, t.path, t.issue, t.source, t.request, t.short_description, t.status,
                    t.loop_name, t.step, t.iteration, t.loop_elapsed, t.cost, t.worktree,
                    t.review_note, t.message, t.created_at, t.run_secs, t.key
                ],
            )
            .map_err(es)?;
        t.id = self.0.last_insert_rowid().to_string();
        Ok(())
    }

    /// Task with ID `id`. Empty `id` returns all tasks.
    pub fn find(&self, id: &str) -> Result<Vec<Task>> {
        let mut s = self
            .0
            .prepare(&format!("SELECT {COLS} FROM tasks WHERE ?1 = '' OR id = ?1 ORDER BY created_at"))
            .map_err(es)?;
        let rows = s.query_map([id], from_row).map_err(es)?;
        rows.collect::<rusqlite::Result<_>>().map_err(es)
    }

    pub fn get(&self, prefix: &str) -> Result<Task> {
        let mut v = self.find(prefix)?;
        match v.len() {
            1 => Ok(v.remove(0)),
            0 => Err(format!("no task `{prefix}`")),
            n => Err(format!("`{prefix}` matches {n} tasks")),
        }
    }

    /// Writes run-owned columns. Cost and description have their own updates.
    /// A `stopped` status and its message, set by abort, are kept.
    pub fn save(&self, t: &Task) -> Result<()> {
        self.0
            .execute(
                "UPDATE tasks SET status=CASE WHEN status='stopped' THEN status ELSE ?2 END, loop_name=?3,
                 step=?4, iteration=?5, loop_elapsed=?6, worktree=?7, review_note=?8,
                 message=CASE WHEN status='stopped' THEN message ELSE ?9 END WHERE id=?1",
                params![
                    t.id, t.status, t.loop_name, t.step, t.iteration, t.loop_elapsed,
                    t.worktree, t.review_note, t.message
                ],
            )
            .map_err(es)?;
        Ok(())
    }

    /// Compare-and-set: moves a task from one of `from` to `running`.
    pub fn claim(&self, prefix: &str, from: &[&str]) -> Result<Task> {
        let t = self.get(prefix)?;
        if !from.contains(&t.status.as_str()) {
            return Err(format!("task {} is {}", t.id, t.status));
        }
        let n = self
            .0
            .execute(
                "UPDATE tasks SET status='running', message=NULL WHERE id=?1 AND status=?2",
                params![t.id, t.status],
            )
            .map_err(es)?;
        if n == 0 {
            return Err(format!("task {} changed, retry", t.id));
        }
        Ok(Task { status: "running".into(), message: None, ..t })
    }

    /// Moves a running task to `stopped`. Returns false when the task is not running.
    pub fn stop_running(&self, id: &str) -> Result<bool> {
        let n = self
            .0
            .execute(
                "UPDATE tasks SET status='stopped', message='aborted' WHERE id=?1 AND status='running'",
                params![id],
            )
            .map_err(es)?;
        Ok(n > 0)
    }

    pub fn set_status(&self, id: &str, status: &str, message: Option<&str>) -> Result<()> {
        self.0
            .execute("UPDATE tasks SET status=?2, message=?3 WHERE id=?1", params![id, status, message])
            .map_err(es)?;
        Ok(())
    }

    pub fn set_key(&self, id: &str, key: &str) -> Result<()> {
        self.0
            .execute("UPDATE tasks SET key=?2 WHERE id=?1", params![id, key])
            .map_err(es)?;
        Ok(())
    }

    pub fn set_description(&self, id: &str, text: &str) -> Result<()> {
        self.0
            .execute("UPDATE tasks SET short_description=?2 WHERE id=?1", params![id, text])
            .map_err(es)?;
        Ok(())
    }

    pub fn add_cost(&self, id: &str, cost: f64) -> Result<()> {
        self.0
            .execute("UPDATE tasks SET cost = cost + ?2 WHERE id=?1", params![id, cost])
            .map_err(es)?;
        Ok(())
    }

    pub fn add_run_secs(&self, id: &str, secs: i64) -> Result<()> {
        self.0
            .execute("UPDATE tasks SET run_secs = run_secs + ?2 WHERE id=?1", params![id, secs])
            .map_err(es)?;
        Ok(())
    }

    /// True when another task in the same checkout (no worktree) is running or waits for review.
    pub fn busy_in_checkout(&self, path: &str, id: &str) -> Result<bool> {
        let n: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE path=?1 AND id!=?2 AND worktree IS NULL
                 AND status IN ('running','waiting_review')",
                params![path, id],
                |r| r.get(0),
            )
            .map_err(es)?;
        Ok(n > 0)
    }

    /// Runs left `running` by a previous daemon cannot continue.
    pub fn fail_running(&self) -> Result<()> {
        self.0
            .execute(
                "UPDATE tasks SET status='failed', message='interrupted by restart' WHERE status='running'",
                [],
            )
            .map_err(es)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Opens a fresh database in a temp dir. Caller removes the dir.
    fn open_temp(name: &str) -> (Db, PathBuf) {
        let dir = std::env::temp_dir().join(format!("dl-db-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let db = open(&dir.join("sub/state.db")).unwrap();
        (db, dir)
    }

    #[test]
    fn insert_get_and_save_round_trip() {
        let (db, dir) = open_temp("crud");
        let mut t = Task::new("proj", Some("PROJ-1"), Some("jira"), "do it", "research");
        db.insert(&mut t).unwrap();

        let got = db.get(&t.id).unwrap();
        assert_eq!(got.request, "do it");
        assert_eq!(got.issue.as_deref(), Some("PROJ-1"));
        assert_eq!(got.status, "new");
        assert_eq!(got.run_secs, 0);
        assert_eq!(got.loop_name, "definition");
        assert_eq!(got.step, "research");
        assert_eq!(got.label(), t.id);

        db.set_description(&t.id, "short").unwrap();
        db.add_cost(&t.id, 0.5).unwrap();
        db.add_cost(&t.id, 0.25).unwrap();
        let mut got = db.get(&t.id).unwrap();
        assert_eq!(got.label(), "short");
        assert_eq!(got.cost, 0.75);
        db.add_run_secs(&t.id, 12).unwrap();
        db.add_run_secs(&t.id, 3).unwrap();
        assert_eq!(db.get(&t.id).unwrap().run_secs, 15);

        got.status = "waiting_review".into();
        got.iteration = 2;
        got.worktree = Some("/wt".into());
        db.save(&got).unwrap();
        let saved = db.get(&t.id).unwrap();
        assert_eq!(saved.status, "waiting_review");
        assert_eq!(saved.iteration, 2);
        assert_eq!(saved.worktree.as_deref(), Some("/wt"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_adds_run_secs_to_old_table_once() {
        let dir = std::env::temp_dir().join(format!("dl-db-{}-migrate", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE tasks (id INTEGER PRIMARY KEY, issue TEXT, source TEXT, created_at INTEGER NOT NULL);
                 INSERT INTO tasks (id, issue, source, created_at) VALUES (1, NULL, NULL, 0), (2, 'PROJ-1', NULL, 0);",
            )
            .unwrap();
        open(&path).unwrap();
        let db = open(&path).unwrap();
        let n: i64 = db
            .0
            .query_row("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='run_secs'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let key = |id: i64| -> Option<String> { db.0.query_row("SELECT key FROM tasks WHERE id=?1", [id], |r| r.get(0)).unwrap() };
        // Only the no-issue, no-source row keeps its bare ID as key.
        assert_eq!(key(1).as_deref(), Some("1"));
        assert_eq!(key(2), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stop_running_only_stops_running_and_save_keeps_stopped() {
        let (db, dir) = open_temp("stop");
        let mut t = Task::new("p", None, None, "a", "research");
        db.insert(&mut t).unwrap();
        assert!(!db.stop_running(&t.id).unwrap());
        let mut running = db.claim(&t.id, &["new"]).unwrap();
        assert!(db.stop_running(&t.id).unwrap());
        running.loop_elapsed = 5;
        db.save(&running).unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!((got.status.as_str(), got.message.as_deref(), got.loop_elapsed), ("stopped", Some("aborted"), 5));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn get_reports_no_match_and_lists_all_on_empty_id() {
        let (db, dir) = open_temp("get");
        db.insert(&mut Task::new("p", None, None, "a", "research")).unwrap();
        db.insert(&mut Task::new("p", None, None, "b", "research")).unwrap();
        assert!(db.get("99").unwrap_err().contains("no task"));
        assert_eq!(db.get("1").unwrap().request, "a");
        assert_eq!(db.find("").unwrap().len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claim_checks_status_then_marks_running() {
        let (db, dir) = open_temp("claim");
        let mut t = Task::new("p", None, None, "a", "research");
        db.insert(&mut t).unwrap();
        assert!(db.claim(&t.id, &["failed"]).unwrap_err().contains("is new"));

        let claimed = db.claim(&t.id, &["new"]).unwrap();
        assert_eq!(claimed.status, "running");
        assert_eq!(db.get(&t.id).unwrap().status, "running");
        assert!(db.claim(&t.id, &["new"]).unwrap_err().contains("is running"));

        db.stop_running(&t.id).unwrap();
        let resumed = db.claim(&t.id, &["new", "failed", "stopped"]).unwrap();
        assert_eq!(resumed.status, "running");
        assert_eq!(db.get(&t.id).unwrap().status, "running");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn busy_in_checkout_ignores_own_task_and_worktree_tasks() {
        let (db, dir) = open_temp("busy");
        let mut a = Task::new("/repo", None, None, "a", "research");
        let mut b = Task::new("/repo", None, None, "b", "research");
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();
        db.claim(&a.id, &["new"]).unwrap();
        assert!(db.busy_in_checkout("/repo", &b.id).unwrap());
        assert!(!db.busy_in_checkout("/repo", &a.id).unwrap());
        assert!(!db.busy_in_checkout("/other", &b.id).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fail_running_marks_interrupted_tasks_failed() {
        let (db, dir) = open_temp("restart");
        let mut t = Task::new("p", None, None, "a", "research");
        db.insert(&mut t).unwrap();
        db.claim(&t.id, &["new"]).unwrap();
        db.fail_running().unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!(got.status, "failed");
        assert_eq!(got.message.as_deref(), Some("interrupted by restart"));
        fs::remove_dir_all(dir).unwrap();
    }
}
