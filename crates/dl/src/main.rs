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
    /// Show tasks, or one task by ID
    Status { id: Option<String> },
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
        Cmd::Status { id } => {
            let tasks = send(json!({ "cmd": "status", "id": id.unwrap_or_default() }))?;
            for t in tasks.as_array().into_iter().flatten() {
                print_task(t);
            }
        }
    }
    Ok(())
}

fn print_task(t: &Value) {
    let s = |k: &str| t[k].as_str().unwrap_or("-").to_string();
    println!(
        "{}  {:<16} {:<10} {:<12} {}/{} #{}  ${:.4}  {}",
        s("id"),
        s("status"),
        s("path"),
        s("issue"),
        s("loop_name"),
        s("step"),
        t["iteration"],
        t["cost"].as_f64().unwrap_or(0.0),
        s("short_description"),
    );
    if let Some(m) = t["message"].as_str() {
        println!("    ! {m}");
    }
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
        let err = exchange(&dir.join("ctl.fifo"), &reply, &mut json!({ "cmd": "status" })).unwrap_err();
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

        let resp = exchange(&pipe, &reply, &mut json!({ "cmd": "status" })).unwrap();
        daemon.join().unwrap();
        assert_eq!(resp, json!({ "ok": true, "data": { "cmd": "status" } }));
        fs::remove_dir_all(dir).unwrap();
    }
}
