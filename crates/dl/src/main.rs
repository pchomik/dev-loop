use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, io, process, thread};
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
    /// Reject the user review step and restart the current loop
    Reject {
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Abort a task in any status. Status becomes aborted. Start resumes it.
    Abort { id: String },
    /// Show tasks, or one task by ID
    List {
        id: Option<String>,
        /// Turn color on even when stdout is not a terminal
        #[arg(short, long)]
        color: bool,
    },
    /// Alias for list
    Ls {
        id: Option<String>,
        /// Turn color on even when stdout is not a terminal
        #[arg(short, long)]
        color: bool,
    },
    /// Print the output file of a step
    Cat {
        id: String,
        /// Step name, for example `review`
        step: String,
    },
    /// Show the agent log of a task
    Logs {
        id: String,
        /// Keep printing new lines until interrupted
        #[arg(short, long)]
        follow: bool,
    },
}

/// Lines shown before follow starts, or without `--follow`.
const TAIL_LINES: usize = 100;

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
        Cmd::List { id, color } | Cmd::Ls { id, color } => {
            let tasks = send(json!({ "cmd": "list", "id": id.unwrap_or_default() }))?;
            if let Some(arr) = tasks.as_array() {
                print_tasks(arr, color);
            }
        }
        Cmd::Logs { id, follow } => logs(&id, follow)?,
        Cmd::Cat { id, step } => cat(&id, &step)?,
    }
    Ok(())
}

/// Prints the output file of a step. dl-serve returns the path, so the path rule stays in one place.
fn cat(id: &str, step: &str) -> Result<(), String> {
    let path = send(json!({ "cmd": "output", "id": id, "step": step }))?["path"].as_str().unwrap_or_default().to_string();
    write_output(&path, &mut io::stdout().lock())
}

/// Copies the file at `path` to `out`. A read error names the path.
fn write_output(path: &str, out: &mut impl Write) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}. The step has not run, or its worktree is gone."))?;
    out.write_all(&bytes).map_err(|e| e.to_string())
}

fn print_tasks(tasks: &[Value], force_color: bool) {
    let is_tty = unsafe { libc::isatty(1) } == 1;
    let color = use_color(force_color, is_tty, env::var_os("NO_COLOR").is_some());
    for line in format_tasks(tasks, color) {
        println!("{line}");
    }
}

/// `--color` forces color on. Otherwise color is on only for a terminal and when `NO_COLOR` is unset.
fn use_color(force_color: bool, is_tty: bool, no_color: bool) -> bool {
    force_color || (is_tty && !no_color)
}

/// Waiting for review: the user runs `approve` or `reject`.
fn waiting_review(t: &Value) -> bool {
    t["status"].as_str() == Some("waiting_review")
}

/// Stopped by the iteration limit. `dl-serve` sends no reason field, so this matches the message text.
fn iteration_limit(t: &Value) -> bool {
    t["status"].as_str() == Some("stopped")
        && t["message"].as_str().is_some_and(|m| m.starts_with("iteration limit ") && m.ends_with(" reached"))
}

/// ANSI color code for a status. `new` and unknown statuses stay plain.
fn status_color(status: &str) -> Option<&'static str> {
    match status {
        "running" => Some("32"),
        "waiting_review" => Some("33"),
        "done" => Some("34"),
        "error" | "failed" | "aborted" => Some("31"),
        "stopped" => Some("35"),
        _ => None,
    }
}

/// Marker after the status or loop text. Red when color is on.
const MARK: &str = " [!]";

/// Wrap `s` in an ANSI color code. Without a code, return `s` as is.
fn paint(s: &str, code: Option<&str>) -> String {
    match code {
        Some(c) => format!("\x1b[{c}m{s}\x1b[0m"),
        None => s.to_string(),
    }
}

/// `MM:SS` under one hour, `HH:MM` from one hour. Hours are not capped.
fn format_run_time(secs: i64) -> String {
    if secs < 3600 {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60)
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
        ("run_secs", "time"),
        ("short_description", "desc"),
    ];

    let s = |t: &Value, k: &str| t[k].as_str().unwrap_or("-").to_string();
    let cell = |t: &Value, i: usize| -> String {
        let (k, _) = COLS[i];
        let status = t["status"].as_str().unwrap_or("");
        let text = match (k, status) {
            ("loop_name", "new") | ("step", "new" | "done") => "-".to_string(),
            ("cost", "new") => "$0".to_string(),
            ("loop_name", _) => format!("{} ({})", s(t, "loop_name"), t["iteration"].as_i64().unwrap_or(0)),
            ("cost", _) => format!("${:.4}", t["cost"].as_f64().unwrap_or(0.0)),
            ("run_secs", _) => format_run_time(t["run_secs"].as_i64().unwrap_or(0)),
            _ => s(t, k),
        };
        match (k, waiting_review(t), iteration_limit(t)) {
            ("status", true, _) | ("loop_name", _, true) => format!("{text}{MARK}"),
            _ => text,
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
            let text = cell(t, i);
            let padded = format!("{:<width$}", text, width = *width);
            let code = if COLS[i].0 == "status" { status_color(status) } else { None };
            // Color wraps the padded cell, so the escape bytes do not change the width.
            parts.push(match text.strip_suffix(MARK) {
                Some(body) if color => {
                    let pad = &padded[text.len()..];
                    format!("{} {}{pad}", paint(body, code), paint("[!]", Some("31")))
                }
                _ if color => paint(&padded, code),
                _ => padded,
            });
        }
        lines.push(parts.join("  "));
        if let Some(m) = t["message"].as_str().filter(|_| !waiting_review(t) && !iteration_limit(t)) {
            lines.push(format!("    ! {m}"));
        }
    }
    lines
}

/// Prints the last lines of a task log. With `follow`, prints new lines until interrupted.
/// The log is read from the data home, not from dl-serve. Follow mode needs no IPC reply.
fn logs(id: &str, follow: bool) -> Result<(), String> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid task id `{id}`"));
    }
    let path = home().join("logs").join(format!("{id}.log"));
    let mut file = fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let mut out = io::stdout();
    out.write_all(last_lines(&String::from_utf8_lossy(&bytes), TAIL_LINES).as_bytes()).map_err(|e| e.to_string())?;
    if follow {
        loop {
            thread::sleep(Duration::from_millis(200));
            let new = read_new(&mut file, &mut bytes).map_err(|e| e.to_string())?;
            out.write_all(&new).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Returns the log bytes added since the last read and updates `seen`.
/// Compares the whole file with `seen`. A change inside `seen` means `Log::finish`
/// replaced streamed text. Then returns the lines from the first changed line on,
/// so the reader sees the final text. Re-reads the file each poll: a ceiling for huge logs.
fn read_new(file: &mut fs::File, seen: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut now = Vec::new();
    file.read_to_end(&mut now)?;
    let common = seen.iter().zip(&now).take_while(|(a, b)| a == b).count();
    // Pure append resumes at the old end. A change starts at its line boundary so the line prints whole.
    let from = if common == seen.len() {
        common
    } else {
        seen[..common].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)
    };
    let new = now[from..].to_vec();
    *seen = now;
    Ok(new)
}

/// The last `n` lines of `text`. A line without a newline at the end counts as a line.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    lines[lines.len().saturating_sub(n)..].concat()
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
    fn write_output_copies_file_bytes() {
        let dir = temp("cat-ok");
        let file = dir.join("out.md");
        fs::write(&file, "# Result\nbody\n").unwrap();
        let mut out = Vec::new();
        write_output(file.to_str().unwrap(), &mut out).unwrap();
        assert_eq!(out, b"# Result\nbody\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_output_reports_missing_file() {
        let mut out = Vec::new();
        let err = write_output("/nonexistent/out.md", &mut out).unwrap_err();
        assert!(err.contains("/nonexistent/out.md"), "{err}");
        assert!(err.contains("The step has not run"), "{err}");
        assert!(out.is_empty());
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
        assert_eq!(lines[0], "id  status   path  issue   loop           step         cost     time   desc       ");
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
        assert_eq!(lines[0], "id  status  path  issue  loop  step  cost  time   desc ");
        assert_eq!(lines[2], "    ! needs attention");
    }

    #[test]
    fn format_run_time_uses_minutes_then_hours() {
        assert_eq!(format_run_time(0), "00:00");
        assert_eq!(format_run_time(754), "12:34");
        assert_eq!(format_run_time(3599), "59:59");
        assert_eq!(format_run_time(3723), "01:02");
        assert_eq!(format_run_time(400 * 3600), "400:00");
    }

    #[test]
    fn format_tasks_shows_zero_time_for_new_task() {
        let tasks = vec![json!({
            "id": "1", "status": "new", "path": "/a", "issue": null,
            "loop_name": "definition", "step": "research", "iteration": 1,
            "cost": 0.0, "run_secs": 0, "short_description": "First"
        })];
        let lines = format_tasks(&tasks, false);
        let col = lines[0].find("time").unwrap();
        assert_eq!(&lines[1][col..col + 5], "00:00");
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
    fn status_color_marks_aborted_red() {
        assert_eq!(status_color("aborted"), Some("31"));
    }

    #[test]
    fn format_tasks_marks_waiting_review_and_iteration_limit() {
        let task = |status: &str, message: &str| {
            json!({
                "id": "1", "status": status, "path": "/a", "issue": null,
                "loop_name": "execution", "step": "research", "iteration": 4,
                "cost": 0.0, "short_description": "First", "message": message
            })
        };
        let tasks = [
            task("waiting_review", "Waiting for review: run `approve` or `reject`."),
            task("stopped", "iteration limit 3 reached"),
            task("stopped", "time limit reached in execution loop"),
        ];
        let plain = format_tasks(&tasks, false);
        assert!(plain[1].contains("waiting_review [!]"));
        assert!(plain[2].contains("execution (4) [!]"));
        // Message lines hide for the two marked cases only.
        assert_eq!(plain.len(), 5);
        assert_eq!(plain[4], "    ! time limit reached in execution loop");
        // Red [!] in both cells. Status text keeps its status color.
        let colored = format_tasks(&tasks, true);
        assert!(colored[1].contains("\x1b[33mwaiting_review\x1b[0m \x1b[31m[!]\x1b[0m"));
        assert!(colored[2].contains("execution (4) \x1b[31m[!]\x1b[0m"));
        let strip = |s: &String| s.replace("\x1b[33m", "").replace("\x1b[35m", "").replace("\x1b[31m", "").replace("\x1b[0m", "");
        assert_eq!(colored.iter().map(strip).collect::<Vec<_>>(), plain);
    }

    #[test]
    fn last_lines_keeps_last_n_lines_and_partial_line() {
        let text: String = (1..=250).map(|i| format!("line {i}\n")).collect();
        let tail = last_lines(&text, TAIL_LINES);
        assert!(tail.starts_with("line 151\n") && tail.ends_with("line 250\n"));
        assert_eq!(tail.lines().count(), TAIL_LINES);
        assert_eq!(last_lines("a\nb\npartial", 2), "b\npartial");
    }

    #[test]
    fn read_new_follows_appends_and_replace() {
        let path = temp("follow").join("task.log");
        fs::write(&path, "=== step ===\nHello, this is long\n").unwrap();
        let mut file = fs::File::open(&path).unwrap();
        let mut seen = Vec::new();
        file.read_to_end(&mut seen).unwrap();
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"next\n").unwrap();
        assert_eq!(read_new(&mut file, &mut seen).unwrap(), b"next\n");
        // Replace the streamed line with a shorter final text, as Log::finish does.
        fs::write(&path, "=== step ===\nHi\nnext\n").unwrap();
        assert_eq!(read_new(&mut file, &mut seen).unwrap(), b"Hi\nnext\n");
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"more\n").unwrap();
        assert_eq!(read_new(&mut file, &mut seen).unwrap(), b"more\n");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn read_new_follows_longer_and_same_length_replace() {
        let path = temp("follow-replace").join("task.log");
        fs::write(&path, "=== step ===\nHi\n").unwrap();
        let mut file = fs::File::open(&path).unwrap();
        let mut seen = Vec::new();
        file.read_to_end(&mut seen).unwrap();
        // Longer replace of the streamed line.
        fs::write(&path, "=== step ===\nHello there, longer final text\nmore\n").unwrap();
        assert_eq!(read_new(&mut file, &mut seen).unwrap(), b"Hello there, longer final text\nmore\n");
        // Same-length replace: only characters change.
        fs::write(&path, "=== step ===\nHEllo there, longer final text\nmore\n").unwrap();
        assert_eq!(read_new(&mut file, &mut seen).unwrap(), b"HEllo there, longer final text\nmore\n");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn use_color_forced_or_only_for_tty_without_no_color() {
        assert!(use_color(true, false, false));
        assert!(use_color(true, false, true));
        assert!(!use_color(false, false, false));
        assert!(!use_color(false, true, true));
        assert!(!use_color(false, false, true));
        assert!(use_color(false, true, false));
    }
}
