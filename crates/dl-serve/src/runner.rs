use crate::agent;
use crate::config::{self, Config, Loop, Paths, StepKind};
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
    let first = &cfg.loops[0];
    let mut task = Task::new(path, req["issue"].as_str(), source, request, &first.steps[0]);
    task.loop_name = first.name.clone();
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
        let Some(out) = agent::run(&cfg, &prompt, None, None, &dir, id, &p.log(id), Duration::from_secs(300), || Ok(true))? else {
            return Ok(());
        };
        db.add_cost(id, out.cost)?;
        match out.ok().then(|| out.text.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(text) => db.set_description(id, &text),
            None => Err(out.error.unwrap_or_else(|| "empty description".into())),
        }
    })();
    if let Err(e) = res {
        eprintln!("dl-serve: task {id}: short description failed: {e}");
    }
}

pub fn start(p: &Paths, id: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    check_parallel(p, &db, &db.get(id)?)?;
    let t = db.claim(id, &["new", "failed", "stopped", "aborted"])?;
    spawn_run(p, t.id.clone());
    Ok(json!({ "id": t.id }))
}

pub fn approve(p: &Paths, id: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    check_parallel(p, &db, &db.get(id)?)?;
    let mut t = db.claim(id, &["waiting_review"])?;
    let res = config::load(p, Some(Path::new(&t.path)))
        .and_then(|cfg| {
            if !cfg.steps.contains_key(&t.step) {
                return Err(format!("step `{}` is not defined", t.step));
            }
            advance(&cfg, &mut t)
        })
        .and_then(|ended| db.save(&t).map(|()| ended));
    let ended = fail_claimed(&db, id, res)?;
    if ended {
        spawn_job(p, t.id.clone(), finish);
    } else {
        spawn_run(p, t.id.clone());
    }
    Ok(json!({ "id": t.id }))
}

pub fn reject(p: &Paths, id: &str, note: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    check_parallel(p, &db, &db.get(id)?)?;
    let mut t = db.claim(id, &["waiting_review"])?;
    let res = config::load(p, Some(Path::new(&t.path))).and_then(|cfg| {
        if !cfg.steps.contains_key(&t.step) {
            return Err(format!("step `{}` is not defined", t.step));
        }
        t.iteration += 1;
        t.step = cfg.loop_named(&t.loop_name)?.steps[0].clone();
        t.review_note = (!note.is_empty()).then(|| note.to_string());
        db.save(&t)
    });
    fail_claimed(&db, id, res)?;
    spawn_run(p, t.id.clone());
    Ok(json!({ "id": t.id }))
}

/// Aborts a task in any status. `start` can resume it. The worktree and branch stay.
pub fn abort(p: &Paths, id: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    let t = db.get(id)?;
    // Status is set before the kill, so the run loop sees `aborted` when the step ends.
    db.set_aborted(&t.id)?;
    agent::kill(&t.id);
    Ok(json!({ "id": t.id }))
}

pub fn status(p: &Paths, prefix: &str) -> Result<Value, String> {
    let tasks = db::open(&p.db())?.find(prefix)?;
    serde_json::to_value(tasks).map_err(|e| e.to_string())
}

/// Fails a claimed task when its post-claim work errors. Otherwise the task stays `running`.
fn fail_claimed<T>(db: &Db, id: &str, res: Result<T, String>) -> Result<T, String> {
    if let Err(e) = &res {
        let _ = db.set_status(id, "failed", Some(e));
    }
    res
}

fn spawn_run(p: &Paths, id: String) {
    spawn_job(p, id, run);
}

fn spawn_job(p: &Paths, id: String, job: fn(&Paths, &str) -> Result<(), String>) {
    let p = p.clone();
    thread::spawn(move || {
        if let Err(e) = job(&p, &id) {
            eprintln!("dl-serve: task {id}: {e}");
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
        if t.status != "running" {
            return Ok(());
        }
        let dir = PathBuf::from(&t.path);
        let cfg = config::load(p, Some(&dir))?;
        let lp = cfg.loop_named(&t.loop_name)?.clone();
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
        let limit = lp.time_limit_secs()?;
        if let Some(msg) = limit_halt(&lp, &t, limit) {
            return halt(&db, &cfg, &t, "stopped", &msg);
        }

        let wt = PathBuf::from(t.worktree.clone().unwrap_or_else(|| t.path.clone()));

        let key = key(&t, &cfg.task.prefix);
        if t.key.is_none() {
            db.set_key(id, &key)?;
        }
        let docs_root = docs_root(&cfg, &dir, &wt);
        let json_routed = step.output.as_deref().is_some_and(|o| is_json_routed(&cfg, o));
        let output = step.output.as_ref().map(|o| output_file(p, &cfg, &t, o));
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
        let prompt = build_prompt(
            &t,
            &key,
            output_str.as_deref(),
            step.kind == StepKind::Review,
            previous.as_deref(),
            &cfg.markdown.user_name,
        );
        let timeout = step_timeout(limit, t.loop_elapsed);
        // Refuses to spawn when an abort set the task to `aborted` before the spawn.
        let alive = || Ok(db.get(id)?.status == "running");
        let Some(out) = agent::run(&cfg, &prompt, Some(skill), step.model.as_deref(), &wt, id, &p.log(id), timeout, alive)? else {
            return Ok(());
        };
        // Aborted during the step: keep the `aborted` status and do not save.
        if db.get(id)?.status != "running" {
            return Ok(());
        }

        t.loop_elapsed += out.secs;
        db.save(&t)?;
        db.add_cost(id, out.cost)?;
        db.add_run_secs(id, out.secs)?;
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
            if cfg.markdown.auto_commit && !json_routed {
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
            db.save(&t)?;
            return finish(p, id);
        }
        db.save(&t)?;
    }
}

/// Ends a task whose last step is done: creates the PR or MR and removes the worktree when safe.
fn finish(p: &Paths, id: &str) -> Result<(), String> {
    let db = db::open(&p.db())?;
    let t = db.get(id)?;
    let dir = PathBuf::from(&t.path);
    let cfg = config::load(p, Some(&dir))?;
    let wt = PathBuf::from(t.worktree.clone().unwrap_or_else(|| t.path.clone()));
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
    Ok(())
}

/// Halt message when a loop limit is reached, else None. `0` means no limit.
fn limit_halt(lp: &Loop, t: &Task, limit: i64) -> Option<String> {
    if lp.iterations > 0 && t.iteration > lp.iterations {
        return Some(format!("iteration limit {} reached", lp.iterations));
    }
    if limit > 0 && t.loop_elapsed >= limit {
        return Some(format!("time limit reached in {} loop", t.loop_name));
    }
    None
}

/// Agent timeout: the time left in the loop. `Duration::MAX` is no timeout, so the step runs until it ends or is aborted.
fn step_timeout(limit: i64, loop_elapsed: i64) -> Duration {
    if limit > 0 {
        Duration::from_secs((limit - loop_elapsed) as u64)
    } else {
        Duration::MAX
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

/// Moves to the next step, or to the first step of the next loop. Returns true when the last step of the last loop ends.
fn advance(cfg: &Config, t: &mut Task) -> Result<bool, String> {
    let lp = cfg.loop_named(&t.loop_name)?;
    let idx = lp
        .steps
        .iter()
        .position(|s| *s == t.step)
        .ok_or_else(|| format!("step `{}` is not in loop `{}`", t.step, t.loop_name))?;
    if let Some(next) = lp.steps.get(idx + 1) {
        t.step = next.clone();
        return Ok(false);
    }
    let Some(next) = cfg.loops.iter().position(|l| l.name == lp.name).and_then(|i| cfg.loops.get(i + 1)) else {
        return Ok(true);
    };
    t.loop_name = next.name.clone();
    t.step = next.steps[0].clone();
    t.iteration = 1;
    t.loop_elapsed = 0;
    Ok(false)
}

fn halt(db: &Db, cfg: &Config, t: &Task, status: &str, msg: &str) -> Result<(), String> {
    db.set_status(&t.id, status, Some(msg))?;
    ix::notify(cfg, &format!("{status}: {} ({msg})", t.label()));
    Ok(())
}

fn branch(id: &str) -> String {
    format!("dev-loop/{id}")
}

/// `<KEY>` is the stored key, else the issue ID. With no issue and no source it is `<prefix><task ID>`. Otherwise it is the task ID.
fn key(t: &Task, prefix: &str) -> String {
    if let Some(stored) = &t.key {
        return stored.clone();
    }
    let raw = match &t.issue {
        Some(issue) => issue.clone(),
        None if t.source.is_none() => format!("{prefix}{}", t.id),
        None => t.id.clone(),
    };
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

fn expand_home_with(s: &str, home: Option<&str>) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        let base = home.unwrap_or(".");
        return PathBuf::from(base).join(rest);
    }
    PathBuf::from(s)
}

fn expand_home(s: &str) -> PathBuf {
    expand_home_with(s, std::env::var("HOME").ok().as_deref())
}

fn is_json_routed(cfg: &Config, template: &str) -> bool {
    !cfg.json.dir.is_empty() && template.ends_with(".json")
}

/// Documents go to the markdown repo, or to the worktree when `repo` is empty.
fn docs_root(cfg: &Config, dir: &Path, wt: &Path) -> PathBuf {
    match cfg.markdown.repo.as_str() {
        "" => wt.to_path_buf(),
        r => {
            let p = expand_home(r);
            if p.is_absolute() { p } else { dir.join(p) }
        }
    }
}

/// File a step writes. `template` is the step's `output`, with `<KEY>` replaced by the key.
fn output_file(p: &Paths, cfg: &Config, t: &Task, template: &str) -> PathBuf {
    let key = key(t, &cfg.task.prefix);
    let name = template.replace("<KEY>", &key);
    if is_json_routed(cfg, template) {
        let base = expand_home(&cfg.json.dir);
        let root = if base.is_absolute() { base } else { p.home.join(base) };
        return root.join(&key).join(name);
    }
    let dir = PathBuf::from(&t.path);
    let wt = PathBuf::from(t.worktree.clone().unwrap_or_else(|| t.path.clone()));
    docs_root(cfg, &dir, &wt).join(&cfg.markdown.dir).join(&key).join(name)
}

/// Output file of a step, for `dl cat`. Fails for an unknown step or a step without `output`.
pub fn output(p: &Paths, id: &str, step: &str) -> Result<Value, String> {
    let t = db::open(&p.db())?.get(id)?;
    let cfg = config::load(p, Some(Path::new(&t.path)))?;
    let Some(template) = cfg.steps.get(step).and_then(|s| s.output.as_deref()) else {
        let with_output: Vec<&str> = cfg.steps.iter().filter(|(_, s)| s.output.is_some()).map(|(n, _)| n.as_str()).collect();
        return Err(format!("step `{step}` is not defined or has no output. Steps with output: {}", with_output.join(", ")));
    };
    Ok(json!({ "path": output_file(p, &cfg, &t, template) }))
}

fn build_prompt(t: &Task, key: &str, output: Option<&str>, review: bool, previous: Option<&str>, user_name: &str) -> String {
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
    let owner = user_name.trim();
    if !review && !owner.is_empty() {
        s += &format!("\nDocument owner: {owner}. Use this as the document Author and default Owner.\n");
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

    /// Serializes tests that abort or spawn agents. `agent::RUNNING` is keyed by task ID, and these tests use ID `1`.
    static AGENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn agent_lock() -> std::sync::MutexGuard<'static, ()> {
        AGENT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn defaults() -> Config {
        let paths = Paths { home: env::temp_dir(), user_config: PathBuf::from("/nonexistent/dl.toml") };
        config::load(&paths, None).unwrap()
    }

    #[test]
    fn key_is_issue_or_task_id_with_unsafe_chars_replaced() {
        let with_issue = Task::new("p", Some("PROJ-12"), None, "r", "research");
        assert_eq!(key(&with_issue, "TASK-"), "PROJ-12");
        assert_eq!(key(&Task::new("p", Some("a/b c"), None, "r", "research"), "TASK-"), "a-b-c");
        let mut no_issue = Task::new("p", None, None, "r", "research");
        no_issue.id = "7".into();
        assert_eq!(key(&no_issue, "TASK-"), "TASK-7");
        assert_eq!(key(&no_issue, "a/b "), "a-b-7");
        let mut source_only = Task::new("p", None, Some("file"), "r", "research");
        source_only.id = "7".into();
        assert_eq!(key(&source_only, "TASK-"), "7");
        // A key stored on the first run wins over a later prefix change.
        no_issue.key = Some("7".into());
        assert_eq!(key(&no_issue, "PROJ-"), "7");
    }

    #[test]
    fn run_uses_prefixed_key_for_folder_prompt_and_commit() {
        use crate::agent::tests::script;
        let _lock = agent_lock();
        let dir = temp("prefixed-key");
        let argv = dir.join("argv.txt");
        // Writes the documents of the first two steps under the expected folder and records the prompt.
        let bin = script(
            "prefixed-key",
            &format!(
                r#"mkdir -p .features/TASK-1 && echo x > .features/TASK-1/TASK-1-research.md && echo x > .features/TASK-1/TASK-1-frd.md && printf '%s\n' "$@" > {}; echo '{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"x"}}]}}}}'"#,
                argv.display()
            ),
        );
        fs::write(dir.join("user.toml"), format!("[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\n[markdown]\nauto_commit = true\n")).unwrap();
        let git = |args: &[&str]| process::Command::new("git").current_dir(&dir).args(args).output().unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "research");
        db.insert(&mut t).unwrap();
        db.claim(&t.id, &["new"]).unwrap();
        run(&p, &t.id).unwrap();
        let got = db.get(&t.id).unwrap();
        // Stops at the user review after the frd step, so both documents were created.
        assert_eq!((got.status.as_str(), got.key.as_deref()), ("waiting_review", Some("TASK-1")));
        // The first prompt line follows the `/skill:` prefix, so match the line end.
        assert!(fs::read_to_string(&argv).unwrap().contains("Task key: TASK-1\n"));
        let log = git(&["log", "--format=%s"]);
        assert_eq!(String::from_utf8_lossy(&log.stdout), "dev-loop: frd TASK-1\ndev-loop: research TASK-1\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn output_returns_step_path_and_lists_steps_with_output() {
        let dir = temp("output");
        fs::write(dir.join("user.toml"), "[steps.a]\nskill = \"s\"\noutput = \"<KEY>-a.md\"\n[steps.b]\nskill = \"s\"\n[[loops]]\nname = \"build\"\nsteps = [\"a\", \"b\"]\n").unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), Some("PROJ-1"), None, "r", "a");
        db.insert(&mut t).unwrap();
        assert_eq!(output(&p, &t.id, "a").unwrap(), json!({ "path": dir.join(".features/PROJ-1/PROJ-1-a.md") }));
        let err = output(&p, &t.id, "b").unwrap_err();
        assert!(err.starts_with("step `b` is not defined or has no output. Steps with output: a, code-review"), "{err}");
        fs::remove_dir_all(dir).unwrap();
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
    fn advance_runs_custom_loops_in_array_order_and_fails_removed_loop() {
        let paths = Paths { home: env::temp_dir(), user_config: PathBuf::from("/nonexistent/dl.toml") };
        let user = "[steps.a]\nskill = \"s\"\n[steps.b]\nskill = \"s\"\n[steps.c]\nskill = \"s\"\n[[loops]]\nname = \"zeta\"\nsteps = [\"a\", \"b\"]\n[[loops]]\nname = \"alpha\"\nsteps = [\"c\"]\niterations = 5\n";
        let dir = temp("custom-advance");
        fs::write(dir.join("user.toml"), user).unwrap();
        let cfg = config::load(&Paths { user_config: dir.join("user.toml"), ..paths }, None).unwrap();
        let mut t = Task::new("p", None, None, "r", "a");
        t.loop_name = "zeta".into();
        t.iteration = 2;
        assert!(!advance(&cfg, &mut t).unwrap());
        assert_eq!((t.loop_name.as_str(), t.step.as_str()), ("zeta", "b"));
        assert!(!advance(&cfg, &mut t).unwrap());
        assert_eq!((t.loop_name.as_str(), t.step.as_str(), t.iteration), ("alpha", "c", 1));
        assert!(advance(&cfg, &mut t).unwrap());
        t.loop_name = "removed".into();
        assert!(advance(&cfg, &mut t).unwrap_err().contains("loop `removed` is not defined"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn approve_and_reject_fail_task_when_its_loop_is_removed() {
        let dir = temp("removed-loop-review");
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        for approve_it in [true, false] {
            let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "user-review");
            t.status = "waiting_review".into();
            t.loop_name = "removed".into();
            db.insert(&mut t).unwrap();
            let res = if approve_it { approve(&p, &t.id) } else { reject(&p, &t.id, "") };
            assert!(res.unwrap_err().contains("loop `removed` is not defined"));
            let got = db.get(&t.id).unwrap();
            assert_eq!(got.status, "failed");
            assert!(got.message.unwrap().contains("loop `removed` is not defined"));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn approve_fails_task_when_its_review_step_is_removed() {
        let dir = temp("removed-step-approve");
        // The task waits at `hold`, which the user config no longer defines.
        fs::write(dir.join("user.toml"), "[steps.dev]\nskill = \"s\"\n[[loops]]\nname = \"build\"\nsteps = [\"dev\"]\n").unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "hold");
        t.loop_name = "build".into();
        t.status = "waiting_review".into();
        db.insert(&mut t).unwrap();
        let err = approve(&p, &t.id).unwrap_err();
        assert!(err.contains("step `hold` is not defined"), "{err}");
        let got = db.get(&t.id).unwrap();
        assert_eq!((got.status.as_str(), got.step.as_str()), ("failed", "hold"));
        assert!(got.message.unwrap().contains("step `hold` is not defined"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reject_fails_task_when_its_review_step_is_removed() {
        let dir = temp("removed-step-reject");
        // The task waits at `hold`, which the user config no longer defines.
        fs::write(dir.join("user.toml"), "[steps.dev]\nskill = \"s\"\n[[loops]]\nname = \"build\"\nsteps = [\"dev\"]\n").unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "hold");
        t.loop_name = "build".into();
        t.status = "waiting_review".into();
        db.insert(&mut t).unwrap();
        let err = reject(&p, &t.id, "").unwrap_err();
        assert!(err.contains("step `hold` is not defined"), "{err}");
        let got = db.get(&t.id).unwrap();
        assert_eq!((got.status.as_str(), got.step.as_str()), ("failed", "hold"));
        assert!(got.message.unwrap().contains("step `hold` is not defined"));
        fs::remove_dir_all(dir).unwrap();
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
        let p = build_prompt(&t, "PROJ-1", Some("/docs/x.md"), true, Some("{\"findings\":[]}"), "Alice");
        assert!(p.contains("Task key: PROJ-1\nPath: proj\n"));
        assert!(p.contains("Issue reference: PROJ-1"));
        assert!(p.contains("Original request:\nadd login"));
        assert!(p.contains("Feedback to address:\nfix tests"));
        assert!(p.contains("Write the output document to: /docs/x.md"));
        assert!(p.contains("Write findings as JSON"));
        assert!(p.contains("Previous findings"));
        assert!(!p.contains("Document owner:"));
        assert!(!p.contains("Alice"));

        let plain = build_prompt(&Task::new("proj", None, None, "r", "research"), "k", None, false, None, "");
        assert!(!plain.contains("Issue reference"));
        assert!(!plain.contains("Write findings as JSON"));
        assert!(!plain.contains("Write the output document"));
        assert!(!plain.contains("Document owner:"));

        let blank = build_prompt(&Task::new("proj", None, None, "r", "research"), "k", None, false, None, "   ");
        assert!(!blank.contains("Document owner:"));

        let owned = build_prompt(&Task::new("proj", None, None, "r", "research"), "k", None, false, None, "Alice");
        assert!(owned.contains("Document owner: Alice. Use this as the document Author and default Owner."));
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
    fn abort_sets_aborted_in_any_status() {
        let _lock = agent_lock();
        let dir = temp("abort");
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new("p", None, None, "r", "research");
        let wt = dir.join("wt");
        fs::create_dir_all(&wt).unwrap();
        t.worktree = Some(wt.to_string_lossy().into_owned());
        db.insert(&mut t).unwrap();
        abort(&p, &t.id).unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!((got.status.as_str(), got.message.as_deref()), ("aborted", None));
        for status in ["waiting_review", "failed", "stopped", "done"] {
            db.set_status(&t.id, status, None).unwrap();
            abort(&p, &t.id).unwrap();
            assert_eq!(db.get(&t.id).unwrap().status, "aborted", "from {status}");
        }
        assert!(wt.is_dir());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zero_limits_do_not_halt_and_set_no_timeout() {
        let lp = |iterations, time_limit: &str| Loop { name: "x".into(), steps: vec![], iterations, time_limit: time_limit.into() };
        let mut t = Task::new("p", None, None, "r", "research");
        t.iteration = 99;
        t.loop_elapsed = 99_999;
        let limit = lp(0, "0").time_limit_secs().unwrap();
        assert!(limit_halt(&lp(0, "0"), &t, limit).is_none());
        assert_eq!(step_timeout(limit, t.loop_elapsed), Duration::MAX);

        assert!(limit_halt(&lp(2, "0"), &t, limit).unwrap().contains("iteration limit 2"));
        let limit = lp(0, "100").time_limit_secs().unwrap();
        assert!(limit_halt(&lp(0, "100"), &t, limit).unwrap().contains("time limit reached"));
        assert_eq!(step_timeout(100, 40), Duration::from_secs(60));
    }

    #[test]
    fn abort_during_agent_step_records_no_elapsed_or_cost() {
        use crate::agent::tests::script;
        let _lock = agent_lock();
        let dir = temp("abort-step");
        // Prints a cost line, then waits. Abort kills it before the step ends.
        let bin = script(
            "abort-step",
            r#"echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"x"}],"usage":{"cost":{"total":0.5}}}}'; sleep 30"#,
        );
        fs::write(dir.join("user.toml"), format!("[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\n")).unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "research");
        db.insert(&mut t).unwrap();
        db.claim(&t.id, &["new"]).unwrap();
        let (p2, id) = (p.clone(), t.id.clone());
        let h = thread::spawn(move || run(&p2, &id));
        thread::sleep(Duration::from_millis(300));
        abort(&p, &t.id).unwrap();
        h.join().unwrap().unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!((got.status.as_str(), got.loop_elapsed, got.cost, got.run_secs), ("aborted", 0, 0.0, 0));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn describe_passes_pi_args_to_agent() {
        use crate::agent::tests::script;
        let _lock = agent_lock();
        let dir = temp("describe-args");
        let argv = dir.join("argv.txt");
        let bin = script(
            "describe-args",
            &format!(r#"printf '%s\n' "$@" > {}; echo '{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"Short"}}]}}}}'"#, argv.display()),
        );
        fs::write(dir.join("user.toml"), format!("[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\npi_args = [\"--trust\"]\n")).unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "research");
        db.insert(&mut t).unwrap();
        describe(&p, &t.id);
        assert!(fs::read_to_string(&argv).unwrap().lines().any(|l| l == "--trust"));
        assert_eq!(db.get(&t.id).unwrap().short_description.as_deref(), Some("Short"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn completed_agent_step_adds_run_secs() {
        use crate::agent::tests::script;
        let _lock = agent_lock();
        let dir = temp("run-secs");
        let bin = script(
            "run-secs",
            r#"echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"x"}],"usage":{"cost":{"total":0.5}}}}'"#,
        );
        fs::write(dir.join("user.toml"), format!("[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\n")).unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "research");
        db.insert(&mut t).unwrap();
        db.claim(&t.id, &["new"]).unwrap();
        run(&p, &t.id).unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!(got.cost, 0.5);
        assert!(got.loop_elapsed > 0);
        assert_eq!(got.run_secs, got.loop_elapsed);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Polls until the task leaves `running`. Returns the task, or panics after 10 seconds.
    fn wait_not_running(db: &Db, id: &str) -> Task {
        for _ in 0..200 {
            let t = db.get(id).unwrap();
            if t.status != "running" {
                return t;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("task {id} still running");
    }

    #[test]
    fn approve_last_step_of_last_loop_marks_done() {
        let dir = temp("approve-last");
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "qa-review");
        t.loop_name = "execution".into();
        t.status = "waiting_review".into();
        db.insert(&mut t).unwrap();
        approve(&p, &t.id).unwrap();
        let got = wait_not_running(&db, &t.id);
        assert_eq!((got.status.as_str(), got.message), ("done", None));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn blocking_review_and_reject_restart_loop_at_first_step() {
        use crate::agent::tests::script;
        let _lock = agent_lock();
        let dir = temp("restart");
        // The review writes one blocker finding. The loop allows one iteration, so the next pass halts.
        let bin = script(
            "restart",
            r#"mkdir -p .features/REV-1 && echo '{"findings":[{"id":"1","severity":"blocker","problem":"p","fix":"f","status":"open"}]}' > .features/REV-1/REV-1-rev.json; echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"x"}]}}'"#,
        );
        let toml = format!(
            "[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\n[steps.dev]\nskill = \"s\"\n[steps.rev]\nkind = \"review\"\nskill = \"s\"\noutput = \"<KEY>-rev.json\"\n[steps.hold]\nkind = \"user-review\"\n[[loops]]\nname = \"build\"\nsteps = [\"dev\", \"rev\"]\niterations = 1\n"
        );
        fs::write(dir.join("user.toml"), toml).unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();

        // Blocking review: restarts at `dev` and counts the iteration, then the iteration limit halts.
        let mut t = Task::new(&dir.to_string_lossy(), Some("REV-1"), None, "r", "rev");
        t.loop_name = "build".into();
        t.iteration = 1;
        db.insert(&mut t).unwrap();
        db.claim(&t.id, &["new"]).unwrap();
        run(&p, &t.id).unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!((got.status.as_str(), got.step.as_str(), got.iteration), ("stopped", "dev", 2));

        // Reject: restarts at `hold` (the loop's first step) and counts the iteration.
        let toml = format!(
            "[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\n[steps.dev]\nskill = \"s\"\n[steps.rev]\nkind = \"review\"\nskill = \"s\"\noutput = \"<KEY>-rev.json\"\n[steps.hold]\nkind = \"user-review\"\n[[loops]]\nname = \"build\"\nsteps = [\"hold\", \"rev\"]\niterations = 1\n"
        );
        fs::write(dir.join("user.toml"), toml).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "rev");
        t.loop_name = "build".into();
        t.status = "waiting_review".into();
        t.iteration = 1;
        db.insert(&mut t).unwrap();
        reject(&p, &t.id, "").unwrap();
        let got = wait_not_running(&db, &t.id);
        assert_eq!((got.status.as_str(), got.step.as_str(), got.iteration), ("waiting_review", "hold", 2));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_stops_at_user_review_in_custom_loop() {
        use crate::agent::tests::script;
        let _lock = agent_lock();
        let dir = temp("user-review-mid");
        let calls = dir.join("calls.txt");
        // Records each agent call. The user review must add none.
        let bin = script(
            "user-review-mid",
            &format!(r#"echo x >> {}; echo '{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"x"}}]}}}}'"#, calls.display()),
        );
        let toml = format!(
            "[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\n[steps.dev]\nskill = \"s\"\n[steps.hold]\nkind = \"user-review\"\n[[loops]]\nname = \"build\"\nsteps = [\"dev\", \"hold\"]\n"
        );
        fs::write(dir.join("user.toml"), toml).unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "dev");
        t.loop_name = "build".into();
        db.insert(&mut t).unwrap();
        db.claim(&t.id, &["new"]).unwrap();
        run(&p, &t.id).unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!((got.status.as_str(), got.step.as_str()), ("waiting_review", "hold"));
        assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);
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

    #[test]
    fn json_dir_routes_json_outputs_and_skips_markdown_auto_commit() {
        use crate::agent::tests::script;
        let _lock = agent_lock();
        let dir = temp("json-dir");
        let p = Paths { home: dir.join("home"), user_config: dir.join("user.toml") };
        fs::create_dir_all(&p.home).unwrap();

        let mut cfg = defaults();
        let t = Task::new(&dir.to_string_lossy(), Some("PROJ-1"), None, "r", "code-review");

        // FR-002 / NFR-001: empty json.dir keeps .json under [markdown].
        assert_eq!(
            output_file(&p, &cfg, &t, "<KEY>-code-review.json"),
            dir.join(".features/PROJ-1/PROJ-1-code-review.json")
        );

        // FR-005: expand_home_with expands ~/ and leaves other paths untouched.
        assert_eq!(expand_home_with("~/.local/share/dev-loop/reviews", Some("/home/u")), PathBuf::from("/home/u/.local/share/dev-loop/reviews"));
        assert_eq!(expand_home_with("reviews", Some("/home/u")), PathBuf::from("reviews"));

        // FR-006 / E-003: relative json.dir resolves against Paths::home.
        cfg.json.dir = "reviews".into();
        assert_eq!(
            output_file(&p, &cfg, &t, "<KEY>-code-review.json"),
            p.home.join("reviews/PROJ-1/PROJ-1-code-review.json")
        );

        // FR-003 / FR-005 / E-002 / E-005: ~/ expands via HOME for .json and markdown.repo, .md stays under [markdown].
        if let Ok(home) = env::var("HOME") {
            cfg.json.dir = "~/.local/share/dev-loop/reviews".into();
            cfg.markdown.repo = "~/uca-knowledge-base".into();
            assert_eq!(
                output_file(&p, &cfg, &t, "<KEY>-code-review.json"),
                PathBuf::from(&home).join(".local/share/dev-loop/reviews/PROJ-1/PROJ-1-code-review.json")
            );
            assert_eq!(
                output_file(&p, &cfg, &t, "<KEY>-frd.md"),
                PathBuf::from(&home).join("uca-knowledge-base/.features/PROJ-1/PROJ-1-frd.md")
            );
        }

        // FR-004 / FR-007 / E-004 / E-006: `output` matches `run` path and `auto_commit` skips `ix::commit` for .json.
        let json_out = p.home.join("reviews/PROJ-1/PROJ-1-custom.json");
        let bin = script(
            "json-dir",
            &format!(
                r#"mkdir -p "$(dirname "{0}")" && echo '{{"ok":true}}' > "{0}"; echo '{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"x"}}]}}}}'"#,
                json_out.display()
            ),
        );
        let toml = format!(
            "[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\n[markdown]\nauto_commit = true\n[json]\ndir = \"reviews\"\n[steps.custom]\nskill = \"s\"\noutput = \"<KEY>-custom.json\"\n[steps.hold]\nkind = \"user-review\"\n[[loops]]\nname = \"build\"\nsteps = [\"custom\", \"hold\"]\n"
        );
        fs::write(&p.user_config, toml).unwrap();
        let git = |args: &[&str]| process::Command::new("git").current_dir(&dir).args(args).output().unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        fs::write(dir.join("README"), "init\n").unwrap();
        git(&["add", "README"]);
        git(&["commit", "-q", "-m", "init"]);

        let db = db::open(&p.db()).unwrap();
        let mut task = Task::new(&dir.to_string_lossy(), Some("PROJ-1"), None, "r", "custom");
        task.loop_name = "build".into();
        db.insert(&mut task).unwrap();
        assert_eq!(output(&p, &task.id, "custom").unwrap(), json!({ "path": json_out }));

        db.claim(&task.id, &["new"]).unwrap();
        run(&p, &task.id).unwrap();
        let got = db.get(&task.id).unwrap();
        assert_eq!(got.status, "waiting_review");
        assert!(json_out.is_file());
        let log = git(&["log", "--format=%s"]);
        assert_eq!(String::from_utf8_lossy(&log.stdout), "init\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn step_model_overrides_agent_model_without_leaking_and_describe_uses_agent_model() {
        use crate::agent::tests::script;
        let _lock = agent_lock();
        let dir = temp("model-resolution");
        let calls = dir.join("calls.txt");
        let bin = script(
            "model-resolution",
            &format!(
                r#"printf '%s' "$*" | tr '\n' ' ' >> {0}; echo >> {0}; echo '{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"Short"}}]}}}}'"#,
                calls.display()
            ),
        );
        let toml = format!(
            "[agent]\nkind = \"pi\"\npi_path = \"{bin}\"\nmodel = \"sonnet\"\n\
             [steps.s1]\nskill = \"k1\"\nmodel = \"opus\"\n\
             [steps.s2]\nskill = \"k2\"\nmodel = \"   \"\n\
             [steps.hold]\nkind = \"user-review\"\nmodel = \"ignored\"\n\
             [[loops]]\nname = \"build\"\nsteps = [\"s1\", \"s2\", \"hold\"]\n"
        );
        fs::write(dir.join("user.toml"), toml).unwrap();
        let p = Paths { home: dir.clone(), user_config: dir.join("user.toml") };
        let db = db::open(&p.db()).unwrap();
        let mut t = Task::new(&dir.to_string_lossy(), None, None, "r", "s1");
        t.loop_name = "build".into();
        db.insert(&mut t).unwrap();

        describe(&p, &t.id);
        db.claim(&t.id, &["new"]).unwrap();
        run(&p, &t.id).unwrap();

        let got = db.get(&t.id).unwrap();
        assert_eq!((got.status.as_str(), got.step.as_str()), ("waiting_review", "hold"));
        let lines: Vec<String> = fs::read_to_string(&calls).unwrap().lines().map(str::to_string).collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("--model=sonnet "), "describe: {}", lines[0]);
        assert!(lines[1].contains("--model=opus "), "s1: {}", lines[1]);
        assert!(lines[2].contains("--model=sonnet "), "s2: {}", lines[2]);
        assert!(!lines[2].contains("--model=opus"), "s2 leaked: {}", lines[2]);
        fs::remove_dir_all(dir).unwrap();
    }
}
