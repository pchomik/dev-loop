use crate::agent;
use crate::config::{self, Config, Paths, StepKind};
use crate::db::{self, Db, Task};
use crate::integrations as ix;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const SOURCES: [&str; 4] = ["jira", "github", "gitlab", "file"];

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Blocker,
    Major,
    Minor,
    Nit,
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Status {
    Open,
    Fixed,
    Wontfix,
    Closed,
}

/// Written by each review step. Development sets `status` and `reply`. The review closes or reopens it.
#[derive(Deserialize, Serialize, Clone, Debug)]
struct Finding {
    id: String,
    severity: Severity,
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: Option<u32>,
    problem: String,
    fix: String,
    status: Status,
    #[serde(default)]
    reply: String,
}

#[derive(Deserialize, Serialize)]
struct Findings {
    findings: Vec<Finding>,
}

impl Finding {
    /// Blocker or major finding not yet closed by the review step. Blocks the task.
    fn blocks(&self) -> bool {
        matches!(self.severity, Severity::Blocker | Severity::Major) && self.status != Status::Closed
    }
}

fn read_findings(path: &Path) -> Result<Findings, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: invalid findings: {e}", path.display()))
}

/// Feedback for development: blocking findings only, with the file to update.
fn feedback(path: &Path, found: &Findings) -> String {
    let mut s = format!(
        "Fix each finding below. Then set its `status` to `fixed` or `wontfix` and add a `reply` in {}.\n",
        path.display()
    );
    for f in found.findings.iter().filter(|f| f.blocks()) {
        let line = f.line.map(|l| format!(":{l}")).unwrap_or_default();
        s += &format!("- {} [{:?}] {}{line}: {}. Fix: {}\n", f.id, f.severity, f.file, f.problem, f.fix);
    }
    s
}

fn field<'a>(req: &'a Value, key: &str) -> Result<&'a str, String> {
    req[key].as_str().ok_or_else(|| format!("missing `{key}`"))
}

pub fn create(p: &Paths, req: &Value) -> Result<Value, String> {
    let path = field(req, "path")?;
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!("path {path} is not a directory"));
    }
    let request = field(req, "request")?;
    let source = req["source"].as_str();
    if source.is_some_and(|s| !SOURCES.contains(&s)) {
        return Err(format!("source must be one of {SOURCES:?}"));
    }
    let cfg = config::load(p, Some(dir))?;
    let create_worktree = req["create_worktree"].as_bool() == Some(true);
    // A worktree branch is pushed by PR or MR creation, so its commits are not lost on cleanup.
    if create_worktree && !ix::auto_create_enabled(&cfg) {
        return Err("--create-worktree needs github.auto_create_pr or gitlab.auto_create_mr enabled".into());
    }
    let mut task = Task::new(path, req["issue"].as_str(), source, request, &cfg.loops.definition.steps[0]);
    let db = db::open(&p.db())?;
    db.insert(&mut task)?;
    if create_worktree {
        // Set before the add, so a failed add leaves a failed task that never runs in `path`.
        let wt = p.worktrees().join(&task.id);
        task.worktree = Some(wt.to_string_lossy().into_owned());
        db.save(&task)?;
        if let Err(e) = ix::worktree_add(dir, &wt, &branch(&task.id)) {
            db.set_status(&task.id, "failed", Some(&e))?;
            return Err(e);
        }
    }
    let (p2, id) = (p.clone(), task.id.clone());
    thread::spawn(move || describe(&p2, &id));
    Ok(json!({ "id": task.id }))
}

/// Hidden agent step: short description for a new task.
fn describe(p: &Paths, id: &str) {
    let res = (|| -> Result<(), String> {
        let db = db::open(&p.db())?;
        let t = db.get(id)?;
        let dir = PathBuf::from(&t.path);
        let cfg = config::load(p, Some(&dir))?;
        let prompt = format!(
            "Write a short description of this development task in at most 8 words. Do not use tools. Reply with only the description.\n\nRequest:\n{}",
            t.request
        );
        let out = agent::run(&cfg, &prompt, None, &dir, Duration::from_secs(300))?;
        db.add_cost(id, out.cost)?;
        match out.ok().then(|| out.text.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(text) => db.set_description(id, &text),
            None => Err(out.error.unwrap_or_else(|| "empty description".into())),
        }
    })();
    if let Err(e) = res {
        eprintln!("dev-loop: task {id}: short description failed: {e}");
    }
}

pub fn start(p: &Paths, id: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    check_parallel(p, &db, &db.get(id)?)?;
    let t = db.claim(id, &["new", "failed", "stopped"])?;
    spawn_run(p, t.id.clone());
    Ok(json!({ "id": t.id }))
}

pub fn approve(p: &Paths, id: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    check_parallel(p, &db, &db.get(id)?)?;
    let mut t = db.claim(id, &["waiting_review"])?;
    let cfg = config::load(p, Some(Path::new(&t.path)))?;
    advance(&cfg, &mut t)?;
    db.save(&t)?;
    spawn_run(p, t.id.clone());
    Ok(json!({ "id": t.id }))
}

pub fn reject(p: &Paths, id: &str, note: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    check_parallel(p, &db, &db.get(id)?)?;
    let mut t = db.claim(id, &["waiting_review"])?;
    let cfg = config::load(p, Some(Path::new(&t.path)))?;
    t.iteration += 1;
    t.step = cfg.loop_named(&t.loop_name).steps[0].clone();
    t.review_note = (!note.is_empty()).then(|| note.to_string());
    db.save(&t)?;
    spawn_run(p, t.id.clone());
    Ok(json!({ "id": t.id }))
}

pub fn status(p: &Paths, prefix: &str) -> Result<Value, String> {
    let tasks = db::open(&p.db())?.find(prefix)?;
    serde_json::to_value(tasks).map_err(|e| e.to_string())
}

fn spawn_run(p: &Paths, id: String) {
    let p = p.clone();
    thread::spawn(move || {
        if let Err(e) = run(&p, &id) {
            eprintln!("dev-loop: task {id}: {e}");
            if let Ok(db) = db::open(&p.db()) {
                let _ = db.set_status(&id, "failed", Some(&e));
            }
        }
    });
}

/// Runs steps from the stored position until review wait, stop, failure, or done.
fn run(p: &Paths, id: &str) -> Result<(), String> {
    let db = db::open(&p.db())?;
    loop {
        let mut t = db.get(id)?;
        let dir = PathBuf::from(&t.path);
        let cfg = config::load(p, Some(&dir))?;
        let lp = cfg.loop_named(&t.loop_name).clone();
        let step = cfg
            .steps
            .get(&t.step)
            .ok_or_else(|| format!("step `{}` is not defined", t.step))?
            .clone();

        if step.kind == StepKind::UserReview {
            db.set_status(id, "waiting_review", Some("Waiting for review: run `approve` or `reject`."))?;
            let label = step.label.clone().unwrap_or_default();
            ix::notify(&cfg, &format!("Review needed [{label}]: {}", t.label()));
            return Ok(());
        }
        if t.iteration > lp.iterations {
            return halt(&db, &cfg, &t, "stopped", &format!("iteration limit {} reached", lp.iterations));
        }
        let limit = lp.time_limit_secs()?;
        if t.loop_elapsed >= limit {
            return halt(&db, &cfg, &t, "stopped", &format!("time limit reached in {} loop", t.loop_name));
        }

        let wt = PathBuf::from(t.worktree.clone().unwrap_or_else(|| t.path.clone()));

        let key = key(&t);
        // Documents go to the markdown repo, or to the worktree when `repo` is empty.
        let docs_root = match cfg.markdown.repo.as_str() {
            "" => wt.clone(),
            r if Path::new(r).is_absolute() => PathBuf::from(r),
            r => dir.join(r),
        };
        let output = step
            .output
            .as_ref()
            .map(|o| docs_root.join(&cfg.markdown.dir).join(&key).join(o.replace("<KEY>", &key)));
        let skill = step
            .skill
            .as_deref()
            .ok_or_else(|| format!("step `{}` has no skill", t.step))?;
        let output_str = output.as_ref().map(|o| o.to_string_lossy().into_owned());
        // Review output is removed before the run, so a stale file cannot pass the output check.
        let previous = match &output {
            Some(file) if step.kind == StepKind::Review && file.is_file() => {
                let text = fs::read_to_string(file).map_err(|e| e.to_string())?;
                fs::remove_file(file).map_err(|e| e.to_string())?;
                Some(text)
            }
            _ => None,
        };
        let prompt = build_prompt(&t, &key, output_str.as_deref(), step.kind == StepKind::Review, previous.as_deref());
        let remaining = (limit - t.loop_elapsed) as u64;
        let out = agent::run(&cfg, &prompt, Some(skill), &wt, Duration::from_secs(remaining))?;

        t.loop_elapsed += out.secs;
        db.save(&t)?;
        db.add_cost(id, out.cost)?;
        if out.timed_out {
            return halt(&db, &cfg, &t, "stopped", &format!("time limit reached in step `{}`", t.step));
        }
        if let Some(err) = &out.error {
            return halt(&db, &cfg, &t, "failed", &format!("step `{}` failed: {err}", t.step));
        }
        if let Some(file) = &output {
            if !file.is_file() {
                let msg = format!("step `{}` did not create {}", t.step, file.display());
                return halt(&db, &cfg, &t, "failed", &msg);
            }
            if cfg.markdown.auto_commit {
                ix::commit(&docs_root, file, &format!("dev-loop: {} {key}", t.step), cfg.markdown.auto_push)?;
            }
        }

        t.review_note = None;
        if let (StepKind::Review, Some(file)) = (step.kind, &output) {
            let found = match read_findings(file) {
                Ok(f) => f,
                Err(e) => return halt(&db, &cfg, &t, "failed", &e),
            };
            if found.findings.iter().any(Finding::blocks) {
                t.iteration += 1;
                t.step = lp.steps[0].clone();
                t.review_note = Some(feedback(file, &found));
                db.save(&t)?;
                continue;
            }
        }
        if advance(&cfg, &mut t)? {
            let mut notes = Vec::new();
            if let Err(e) = ix::create_pr(&cfg, &wt) {
                notes.push(format!("PR/MR creation failed: {e}"));
            }
            if let Some(w) = &t.worktree {
                // Remove only a pushed branch. Otherwise its commits exist only in the worktree.
                if notes.is_empty() && ix::auto_create_enabled(&cfg) {
                    if let Err(e) = ix::remove_worktree(&dir, Path::new(w), &branch(id)) {
                        notes.push(format!("worktree removal failed: {e}"));
                    }
                } else {
                    notes.push("worktree kept: PR/MR not created".into());
                }
            }
            let status = if notes.is_empty() { "done" } else { "error" };
            let msg = (!notes.is_empty()).then(|| notes.join("; "));
            db.set_status(id, status, msg.as_deref())?;
            ix::notify(&cfg, &format!("{status}: {}", t.label()));
            return Ok(());
        }
        db.save(&t)?;
    }
}

/// Tasks in one checkout run in parallel only when a PR or MR is created, so their changes are not lost.
fn check_parallel(p: &Paths, db: &Db, t: &Task) -> Result<(), String> {
    if t.worktree.is_some() || !db.busy_in_checkout(&t.path, &t.id)? {
        return Ok(());
    }
    let cfg = config::load(p, Some(Path::new(&t.path)))?;
    if ix::auto_create_enabled(&cfg) {
        return Ok(());
    }
    Err(format!("another task runs in {}; enable github.auto_create_pr or gitlab.auto_create_mr", t.path))
}

/// Moves to the next step. Returns true when the last execution step ends.
fn advance(cfg: &Config, t: &mut Task) -> Result<bool, String> {
    let steps = &cfg.loop_named(&t.loop_name).steps;
    let idx = steps
        .iter()
        .position(|s| *s == t.step)
        .ok_or_else(|| format!("step `{}` is not in loop `{}`", t.step, t.loop_name))?;
    if let Some(next) = steps.get(idx + 1) {
        t.step = next.clone();
        return Ok(false);
    }
    if t.loop_name == "definition" {
        t.loop_name = "execution".into();
        t.step = cfg.loops.execution.steps[0].clone();
        t.iteration = 1;
        t.loop_elapsed = 0;
        return Ok(false);
    }
    Ok(true)
}

fn halt(db: &Db, cfg: &Config, t: &Task, status: &str, msg: &str) -> Result<(), String> {
    db.set_status(&t.id, status, Some(msg))?;
    ix::notify(cfg, &format!("{status}: {} ({msg})", t.label()));
    Ok(())
}

fn branch(id: &str) -> String {
    format!("dev-loop/{id}")
}

/// `<KEY>` is the issue ID, or the task ID when no issue is set.
fn key(t: &Task) -> String {
    t.issue
        .as_deref()
        .unwrap_or(&t.id)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

fn build_prompt(t: &Task, key: &str, output: Option<&str>, review: bool, previous: Option<&str>) -> String {
    let mut s = format!("Task key: {key}\nPath: {}\n", t.path);
    if let Some(issue) = &t.issue {
        s += &format!("Issue reference: {issue}\n");
    }
    s += &format!("\nOriginal request:\n{}\n", t.request);
    if let Some(note) = &t.review_note {
        s += &format!("\nFeedback to address:\n{note}\n");
    }
    if let Some(o) = output {
        s += &format!("\nWrite the output document to: {o}\n");
    }
    if let Some(prev) = previous {
        s += &format!("\nPrevious findings. Keep each id. Set status to `closed` when fixed, `open` when not fixed. Add new findings with new ids:\n{prev}\n");
    }
    if review {
        s += "\nWrite findings as JSON to the output file: {\"findings\": [{\"id\", \"severity\": \"blocker|major|minor|nit\", \"file\", \"line\", \"problem\", \"fix\", \"status\": \"open\"}]}. Blocker and major block the change. Minor and nit do not. Report only problems that development can fix by changing code in this task's worktree, with skills and tools development has.\n";
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Paths};
    use std::{env, fs, process};

    fn temp(name: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("dl-runner-{}-{name}", process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn defaults() -> Config {
        let paths = Paths { home: env::temp_dir(), user_config: PathBuf::from("/nonexistent/dl.toml") };
        config::load(&paths, None).unwrap()
    }

    #[test]
    fn key_is_issue_or_task_id_with_unsafe_chars_replaced() {
        let with_issue = Task::new("p", Some("PROJ-12"), None, "r", "research");
        assert_eq!(key(&with_issue), "PROJ-12");
        assert_eq!(key(&Task::new("p", Some("a/b c"), None, "r", "research")), "a-b-c");
        let mut no_issue = Task::new("p", None, None, "r", "research");
        no_issue.id = "7".into();
        assert_eq!(key(&no_issue), "7");
    }

    #[test]
    fn branch_uses_task_id() {
        assert_eq!(branch("abc"), "dev-loop/abc");
    }

    #[test]
    fn advance_moves_definition_into_execution_then_ends() {
        let cfg = defaults();
        let mut t = Task::new("p", None, None, "r", "research");
        assert!(!advance(&cfg, &mut t).unwrap());
        assert_eq!(t.step, "frd");

        t.step = "user-review".into();
        t.iteration = 2;
        t.loop_elapsed = 100;
        assert!(!advance(&cfg, &mut t).unwrap());
        assert_eq!((t.loop_name.as_str(), t.step.as_str(), t.iteration, t.loop_elapsed), ("execution", "development", 1, 0));

        for next in ["code-review", "rules-review", "complexity-review", "qa-review"] {
            assert!(!advance(&cfg, &mut t).unwrap());
            assert_eq!(t.step, next);
        }
        assert!(advance(&cfg, &mut t).unwrap());
    }

    #[test]
    fn advance_rejects_step_outside_loop() {
        let mut t = Task::new("p", None, None, "r", "development");
        assert!(advance(&defaults(), &mut t).is_err());
    }

    #[test]
    fn build_prompt_includes_request_feedback_output_and_findings() {
        let mut t = Task::new("proj", Some("PROJ-1"), None, "add login", "research");
        t.review_note = Some("fix tests".into());
        let p = build_prompt(&t, "PROJ-1", Some("/docs/x.md"), true, Some("{\"findings\":[]}"));
        assert!(p.contains("Task key: PROJ-1\nPath: proj\n"));
        assert!(p.contains("Issue reference: PROJ-1"));
        assert!(p.contains("Original request:\nadd login"));
        assert!(p.contains("Feedback to address:\nfix tests"));
        assert!(p.contains("Write the output document to: /docs/x.md"));
        assert!(p.contains("Write findings as JSON"));
        assert!(p.contains("Previous findings"));

        let plain = build_prompt(&Task::new("proj", None, None, "r", "research"), "k", None, false, None);
        assert!(!plain.contains("Issue reference"));
        assert!(!plain.contains("Write findings as JSON"));
        assert!(!plain.contains("Write the output document"));
    }

    #[test]
    fn only_open_blocker_or_major_blocks() {
        let f = |sev: &str, status: &str| {
            let text = format!(r#"{{"findings":[{{"id":"1","severity":"{sev}","problem":"p","fix":"f","status":"{status}"}}]}}"#);
            serde_json::from_str::<Findings>(&text).unwrap().findings.iter().any(Finding::blocks)
        };
        assert!(f("blocker", "open"));
        assert!(f("major", "fixed"));
        assert!(!f("blocker", "closed"));
        assert!(!f("minor", "open"));
        assert!(!f("nit", "open"));
    }

    #[test]
    fn field_reports_missing_key() {
        let req = json!({ "a": "x" });
        assert_eq!(field(&req, "a").unwrap(), "x");
        assert_eq!(field(&req, "b").unwrap_err(), "missing `b`");
    }

    #[test]
    fn halt_stores_status_and_message() {
        let dir = temp("halt");
        let db = db::open(&dir.join("state.db")).unwrap();
        let mut t = Task::new("p", None, None, "r", "research");
        db.insert(&mut t).unwrap();
        halt(&db, &defaults(), &t, "stopped", "iteration limit 3 reached").unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!(got.status, "stopped");
        assert_eq!(got.message.as_deref(), Some("iteration limit 3 reached"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_rejects_missing_path_and_bad_source() {
        let dir = temp("create");
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };

        let err = create(&p, &json!({ "path": dir.to_string_lossy(), "request": "r", "create_worktree": true })).unwrap_err();
        assert!(err.contains("--create-worktree needs"), "{err}");

        let err = create(&p, &json!({ "path": "/nonexistent/dl", "request": "r" })).unwrap_err();
        assert!(err.contains("is not a directory"), "{err}");

        let err = create(&p, &json!({ "path": dir.to_string_lossy(), "request": "r", "source": "slack" })).unwrap_err();
        assert!(err.contains("source must be one of"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }
}
