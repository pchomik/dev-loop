use crate::agent;
use crate::config::{self, Config, Paths, StepKind};
use crate::db::{self, Db, Task};
use crate::integrations as ix;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Review step prints this line to request another execution iteration.
const REWORK: &str = "DEV-LOOP-VERDICT: REWORK";
const SOURCES: [&str; 4] = ["jira", "github", "gitlab", "file"];

fn field<'a>(req: &'a Value, key: &str) -> Result<&'a str, String> {
    req[key].as_str().ok_or_else(|| format!("missing `{key}`"))
}

fn project_dir(p: &Paths, name: &str) -> Result<PathBuf, String> {
    config::load(p, None)?
        .projects
        .get(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("unknown project `{name}`; add it under [projects] in {}", p.user_config.display()))
}

pub fn create(p: &Paths, req: &Value) -> Result<Value, String> {
    let project = field(req, "project")?;
    let dir = project_dir(p, project)?;
    let request = field(req, "request")?;
    let source = req["source"].as_str();
    if source.is_some_and(|s| !SOURCES.contains(&s)) {
        return Err(format!("source must be one of {SOURCES:?}"));
    }
    let cfg = config::load(p, Some(&dir))?;
    let task = Task::new(project, req["issue"].as_str(), source, request, &cfg.loops.definition.steps[0]);
    db::open(&p.db())?.insert(&task)?;
    let (p2, id) = (p.clone(), task.id.clone());
    thread::spawn(move || describe(&p2, &id));
    Ok(json!({ "id": task.id }))
}

/// Hidden agent step: short description for a new task.
fn describe(p: &Paths, id: &str) {
    let res = (|| -> Result<(), String> {
        let db = db::open(&p.db())?;
        let t = db.get(id)?;
        let dir = project_dir(p, &t.project)?;
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
    let t = db::open(&p.db())?.claim(id, &["new", "failed", "stopped"])?;
    spawn_run(p, t.id.clone());
    Ok(json!({ "id": t.id }))
}

pub fn approve(p: &Paths, id: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    let mut t = db.claim(id, &["waiting_review"])?;
    let cfg = config::load(p, Some(&project_dir(p, &t.project)?))?;
    advance(&cfg, &mut t)?;
    db.save(&t)?;
    spawn_run(p, t.id.clone());
    Ok(json!({ "id": t.id }))
}

pub fn reject(p: &Paths, id: &str, note: &str) -> Result<Value, String> {
    let db = db::open(&p.db())?;
    let mut t = db.claim(id, &["waiting_review"])?;
    let cfg = config::load(p, Some(&project_dir(p, &t.project)?))?;
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
        let dir = project_dir(p, &t.project)?;
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

        if t.worktree.is_none() {
            let wt = p.worktrees().join(id);
            ix::worktree_add(&dir, &wt, &branch(id))?;
            t.worktree = Some(wt.to_string_lossy().into_owned());
            db.save(&t)?;
        }
        let wt = PathBuf::from(t.worktree.clone().unwrap_or_default());

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
        let prompt = build_prompt(&t, &key, output_str.as_deref(), step.kind == StepKind::Review);
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
        if step.kind == StepKind::Review && out.text.contains(REWORK) {
            t.iteration += 1;
            t.step = lp.steps[0].clone();
            t.review_note = Some(out.text);
            db.save(&t)?;
            continue;
        }
        if advance(&cfg, &mut t)? {
            let msg = match ix::create_pr(&cfg, &wt, &branch(id)) {
                Ok(()) => None,
                Err(e) => Some(format!("PR/MR creation failed: {e}")),
            };
            db.set_status(id, "done", msg.as_deref())?;
            ix::notify(&cfg, &format!("Done: {}", t.label()));
            return Ok(());
        }
        db.save(&t)?;
    }
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

/// `<KEY>` is the issue ID, or the task UUID when no issue is set.
fn key(t: &Task) -> String {
    t.issue
        .as_deref()
        .unwrap_or(&t.id)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

fn build_prompt(t: &Task, key: &str, output: Option<&str>, review: bool) -> String {
    let mut s = format!("Task key: {key}\nProject: {}\n", t.project);
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
    if review {
        s += &format!("\nEnd your reply with the line `{REWORK}` if rework is needed. Otherwise end with `DEV-LOOP-VERDICT: PASS`.\n");
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
    fn key_is_issue_or_uuid_with_unsafe_chars_replaced() {
        let with_issue = Task::new("p", Some("PROJ-12"), None, "r", "research");
        assert_eq!(key(&with_issue), "PROJ-12");
        assert_eq!(key(&Task::new("p", Some("a/b c"), None, "r", "research")), "a-b-c");
        let no_issue = Task::new("p", None, None, "r", "research");
        assert_eq!(key(&no_issue), no_issue.id);
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

        for next in ["code-review", "rules-review", "simplification-review"] {
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
    fn build_prompt_includes_request_feedback_output_and_verdict() {
        let mut t = Task::new("proj", Some("PROJ-1"), None, "add login", "research");
        t.review_note = Some("fix tests".into());
        let p = build_prompt(&t, "PROJ-1", Some("/docs/x.md"), true);
        assert!(p.contains("Task key: PROJ-1\nProject: proj\n"));
        assert!(p.contains("Issue reference: PROJ-1"));
        assert!(p.contains("Original request:\nadd login"));
        assert!(p.contains("Feedback to address:\nfix tests"));
        assert!(p.contains("Write the output document to: /docs/x.md"));
        assert!(p.contains(REWORK));

        let plain = build_prompt(&Task::new("proj", None, None, "r", "research"), "k", None, false);
        assert!(!plain.contains("Issue reference"));
        assert!(!plain.contains(REWORK));
        assert!(!plain.contains("Write the output document"));
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
        let t = Task::new("p", None, None, "r", "research");
        db.insert(&t).unwrap();
        halt(&db, &defaults(), &t, "stopped", "iteration limit 3 reached").unwrap();
        let got = db.get(&t.id).unwrap();
        assert_eq!(got.status, "stopped");
        assert_eq!(got.message.as_deref(), Some("iteration limit 3 reached"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_rejects_unknown_project_and_bad_source() {
        let dir = temp("create");
        let user = dir.join("user.toml");
        fs::write(&user, "[projects]\nproj = \"/tmp\"\n").unwrap();
        let p = Paths { home: dir.clone(), user_config: user };

        let err = create(&p, &json!({ "project": "other", "request": "r" })).unwrap_err();
        assert!(err.contains("unknown project `other`"), "{err}");

        let err = create(&p, &json!({ "project": "proj", "request": "r", "source": "slack" })).unwrap_err();
        assert!(err.contains("source must be one of"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }
}
