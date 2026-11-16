use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::{env, io, process};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "dl", about = "Control the dl-serve app")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a task from a request
    Create {
        /// Task directory. Default: current directory
        #[arg(long)]
        path: Option<PathBuf>,
        /// Run the task in its own git worktree
        #[arg(long)]
        create_worktree: bool,
        /// jira, github, gitlab or file
        #[arg(long, value_parser = ["jira", "github", "gitlab", "file"])]
        source: Option<String>,
        /// Issue reference, for example PROJ-123 or 42
        #[arg(long)]
        issue: Option<String>,
        #[arg(long, conflicts_with = "file")]
        request: Option<String>,
        /// Read the request from a local file
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Start a new or failed task in its worktree
    Start { id: String },
    /// Approve the user review step
    Approve { id: String },
    /// Reject the user review step and restart the definition loop
    Reject {
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Stop a running task. Status becomes stopped. Start resumes it.
    Abort { id: String },
    /// Show tasks, or one task by ID
    List { id: Option<String> },
    /// Alias for list
    Ls { id: Option<String> },
}

fn main() {
    if let Err(e) = run(Cli::parse().cmd) {
        eprintln!("dl: {e}");
        process::exit(1);
    }
}

fn run(cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Create { path, create_worktree, source, issue, request, file } => {
            let request = match (request, file) {
                (Some(r), None) => r,
                (None, Some(f)) => fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?,
                _ => return Err("give --request or --file".into()),
            };
            let path = match path {
                Some(p) => p,
                None => env::current_dir().map_err(|e| e.to_string())?,
            };
            let path = fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let resp = send(json!({
                "cmd": "create", "path": path, "create_worktree": create_worktree,
                "source": source, "issue": issue, "request": request
            }))?;
            println!("{}", resp["id"].as_str().unwrap_or_default());
        }
        Cmd::Start { id } => {
            send(json!({ "cmd": "start", "id": id }))?;
        }
        Cmd::Approve { id } => {
            send(json!({ "cmd": "approve", "id": id }))?;
        }
        Cmd::Reject { id, note } => {
            send(json!({ "cmd": "reject", "id": id, "note": note.unwrap_or_default() }))?;
        }
        Cmd::Abort { id } => {
            send(json!({ "cmd": "abort", "id": id }))?;
        }
        Cmd::List { id } | Cmd::Ls { id } => {
            let tasks = send(json!({ "cmd": "list", "id": id.unwrap_or_default() }))?;
            if let Some(arr) = tasks.as_array() {
                print_tasks(arr);
            }
        }
    }
    Ok(())
}

fn print_tasks(tasks: &[Value]) {
    let is_tty = unsafe { libc::isatty(1) } == 1;
    let color = use_color(is_tty, env::var_os("NO_COLOR").is_some());
    for line in format_tasks(tasks, color) {
        println!("{line}");
    }
}

/// Color is on only for a terminal and when `NO_COLOR` is unset.
fn use_color(is_tty: bool, no_color: bool) -> bool {
    is_tty && !no_color
}

/// ANSI color code for a status. `new` and unknown statuses stay plain.
fn status_color(status: &str) -> Option<&'static str> {
    match status {
        "running" => Some("32"),
        "waiting_review" => Some("33"),
        "done" => Some("34"),
        "error" | "failed" => Some("31"),
        "stopped" => Some("35"),
        _ => None,
    }
}

fn format_tasks(tasks: &[Value], color: bool) -> Vec<String> {
    if tasks.is_empty() {
        return Vec::new();
    }

    const COLS: &[(&str, &str)] = &[
        ("id", "id"),
        ("status", "status"),
        ("path", "path"),
        ("issue", "issue"),
        ("loop_name", "loop"),
        ("step", "step"),
        ("cost", "cost"),
        ("short_description", "desc"),
    ];

    let s = |t: &Value, k: &str| t[k].as_str().unwrap_or("-").to_string();
    let cell = |t: &Value, i: usize| -> String {
        let (k, _) = COLS[i];
        let status = t["status"].as_str().unwrap_or("");
        match (k, status) {
            ("loop_name", "new") | ("step", "new" | "done") => "-".to_string(),
            ("cost", "new") => "$0".to_string(),
            ("loop_name", _) => format!("{} ({})", s(t, "loop_name"), t["iteration"].as_i64().unwrap_or(0)),
            ("cost", _) => format!("${:.4}", t["cost"].as_f64().unwrap_or(0.0)),
            _ => s(t, k),
        }
    };

    let col_count = COLS.len();
    let mut widths = vec![0usize; col_count];
    for i in 0..col_count {
        widths[i] = COLS[i].1.len();
    }
    for t in tasks {
        for (i, width) in widths.iter_mut().enumerate() {
            *width = (*width).max(cell(t, i).len());
        }
    }

    let mut lines = Vec::new();
    let mut parts = Vec::with_capacity(col_count);
    for i in 0..col_count {
        parts.push(format!("{:<width$}", COLS[i].1, width = widths[i]));
    }
    lines.push(parts.join("  "));
    for t in tasks {
        parts.clear();
        let status = t["status"].as_str().unwrap_or("");
        for (i, width) in widths.iter().enumerate() {
            let padded = format!("{:<width$}", cell(t, i), width = *width);
            // Color wraps the padded cell, so the escape bytes do not change the width.
            parts.push(match status_color(status) {
                Some(code) if color && COLS[i].0 == "status" => format!("\x1b[{code}m{padded}\x1b[0m"),
                _ => padded,
            });
        }
        lines.push(parts.join("  "));
        if let Some(m) = t["message"].as_str() {
            lines.push(format!("    ! {m}"));
        }
    }
    lines
}

fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share/dev-loop")
}

/// Sends one request and waits for the reply on a per-request FIFO.
fn send(mut req: Value) -> Result<Value, String> {
    let pipe = home().join("ctl.fifo");
    let reply = env::temp_dir().join(format!("dl-ctl-{}.fifo", Uuid::new_v4()));
    mkfifo(&reply)?;
    let res = exchange(&pipe, &reply, &mut req);
    let _ = fs::remove_file(&reply);
    let resp = res?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(resp["data"].clone())
    } else {
        Err(resp["error"].as_str().unwrap_or("request failed").to_string())
    }
}

fn exchange(pipe: &Path, reply: &Path, req: &mut Value) -> Result<Value, String> {
    // Open the reply FIFO read-write so the daemon's open for writing never blocks.
    let reader = OpenOptions::new().read(true).write(true).open(reply).map_err(|e| e.to_string())?;
    req["reply"] = json!(reply.to_string_lossy());
    let mut w = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(pipe)
        .map_err(|e| format!("dl-serve is not running ({}): {e}", pipe.display()))?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("bad reply: {e}"))
}

fn mkfifo(path: &Path) -> Result<(), String> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn temp(name: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("dl-ctl-{}-{name}", process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn create_needs_request_or_file() {
        let cmd = Cmd::Create { path: None, create_worktree: false, source: None, issue: None, request: None, file: None };
        assert!(run(cmd).unwrap_err().contains("--request or --file"));
    }

    #[test]
    fn create_reports_unreadable_file() {
        let cmd = Cmd::Create { path: None, create_worktree: false, source: None, issue: None, request: None, file: Some("/nonexistent/req.md".into()) };
        assert!(run(cmd).unwrap_err().contains("req.md"));
    }

    #[test]
    fn exchange_fails_when_daemon_pipe_missing() {
        let dir = temp("nopipe");
        let reply = dir.join("reply.fifo");
        mkfifo(&reply).unwrap();
        let err = exchange(&dir.join("ctl.fifo"), &reply, &mut json!({ "cmd": "list" })).unwrap_err();
        assert!(err.contains("dl-serve is not running"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exchange_sends_request_and_reads_reply() {
        let dir = temp("roundtrip");
        let pipe = dir.join("ctl.fifo");
        let reply = dir.join("reply.fifo");
        mkfifo(&pipe).unwrap();
        mkfifo(&reply).unwrap();

        // Keep a read end open so the client's non-blocking open succeeds.
        let daemon_in = OpenOptions::new().read(true).write(true).open(&pipe).unwrap();
        let daemon = thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(daemon_in).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let mut out = OpenOptions::new().write(true).open(req["reply"].as_str().unwrap()).unwrap();
            writeln!(out, "{}", json!({ "ok": true, "data": { "cmd": req["cmd"] } })).unwrap();
        });

        let resp = exchange(&pipe, &reply, &mut json!({ "cmd": "list" })).unwrap();
        daemon.join().unwrap();
        assert_eq!(resp, json!({ "ok": true, "data": { "cmd": "list" } }));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn format_tasks_aligns_columns() {
        let tasks = vec![
            json!({
                "id": "1", "status": "new", "path": "/a", "issue": null,
                "loop_name": "definition", "step": "research", "iteration": 1,
                "cost": 0.5, "short_description": "First"
            }),
            json!({
                "id": "12", "status": "running", "path": "/abc", "issue": "PROJ-1",
                "loop_name": "execution", "step": "development", "iteration": 2,
                "cost": 0.1234, "short_description": "Second task"
            }),
        ];
        let lines = format_tasks(&tasks, false);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "id  status   path  issue   loop           step         cost     desc       ");
        let header = &lines[0];
        let row1 = &lines[1];
        let row2 = &lines[2];
        assert!(header.starts_with("id"));
        assert!(row1.starts_with("1   new"));
        assert!(row2.starts_with("12  running"));
        assert!(!header.contains("iter"));
        assert!(row1.contains("new") && row1.contains("$0 "));
        assert!(!row1.contains("definition") && !row1.contains("research"));
        assert!(row2.contains("execution (2)"));
        for line in &lines {
            assert!(!line.starts_with("─"));
            assert!(!line.starts_with("┌"));
            assert!(!line.starts_with("┐"));
            assert!(!line.starts_with("├"));
            assert!(!line.starts_with("┤"));
            assert!(!line.starts_with("└"));
            assert!(!line.starts_with("┘"));
            assert!(!line.starts_with("+"));
            assert!(!line.starts_with("-"));
            assert!(!line.starts_with("="));
        }
    }

    #[test]
    fn format_tasks_shows_message() {
        let tasks = vec![
            json!({
                "id": "1", "status": "new", "path": "/a", "issue": null,
                "loop_name": "definition", "step": "research", "iteration": 1,
                "cost": 0.0, "short_description": "First",
                "message": "needs attention"
            }),
        ];
        let lines = format_tasks(&tasks, false);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "id  status  path  issue  loop  step  cost  desc ");
        assert_eq!(lines[2], "    ! needs attention");
    }

    #[test]
    fn format_tasks_shows_step_for_done_only_after_start() {
        let task = |status: &str| {
            json!({
                "id": "1", "status": status, "path": "/a", "issue": null,
                "loop_name": "execution", "step": "qa-review", "iteration": 1,
                "cost": 0.5, "short_description": "First"
            })
        };
        let done = format_tasks(&[task("done")], false);
        assert!(done[1].contains("execution (1)") && !done[1].contains("qa-review"));
        let running = format_tasks(&[task("running")], false);
        assert!(running[1].contains("qa-review") && running[1].contains("$0.5000"));
    }

    #[test]
    fn format_tasks_colors_status_cell_only() {
        let task = |status: &str| {
            json!({
                "id": "1", "status": status, "path": "/a", "issue": null,
                "loop_name": "definition", "step": "research", "iteration": 1,
                "cost": 0.0, "short_description": "First"
            })
        };
        let tasks = [task("running"), task("new")];
        let plain = format_tasks(&tasks, false);
        let colored = format_tasks(&tasks, true);
        // Removing the color codes gives the plain output, so widths match.
        let strip = |s: &String| s.replace("\x1b[32m", "").replace("\x1b[0m", "");
        assert_eq!(colored.iter().map(strip).collect::<Vec<_>>(), plain);
        // Escape codes wrap only the status cell of the running row.
        let row = &colored[1];
        let start = row.find("\x1b[32m").unwrap();
        let end = row.find("\x1b[0m").unwrap() + "\x1b[0m".len();
        assert!(row[start..end].contains("running"));
        assert!(!row[..start].contains('\x1b'));
        assert!(!row[end..].contains('\x1b'));
        assert!(!colored[2].contains('\x1b'));
    }

    #[test]
    fn use_color_only_for_tty_without_no_color() {
        assert!(!use_color(false, false));
        assert!(!use_color(true, true));
        assert!(!use_color(false, true));
        assert!(use_color(true, false));
    }
}
