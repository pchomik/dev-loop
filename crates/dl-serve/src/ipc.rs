use crate::config::Paths;
use crate::{db, runner};
use serde_json::{Value, json};
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::{fs, io, thread};

/// Serves JSON requests from the named pipe. Each line is `{"cmd":..., "reply": <fifo path>, ...}`.
pub fn serve(p: &Paths) -> Result<(), String> {
    db::open(&p.db())?.fail_running()?;
    fs::create_dir_all(&p.home).map_err(|e| e.to_string())?;
    let pipe = p.pipe();
    if !pipe.exists() {
        mkfifo(&pipe)?;
    }
    // O_RDWR keeps the pipe open when no ctl client is connected.
    let f = OpenOptions::new().read(true).write(true).open(&pipe).map_err(|e| e.to_string())?;
    eprintln!("dl-serve: listening on {}", pipe.display());
    for line in BufReader::new(f).lines() {
        let line = line.map_err(|e| e.to_string())?;
        let p = p.clone();
        thread::spawn(move || handle(&p, &line));
    }
    Ok(())
}

fn mkfifo(path: &Path) -> Result<(), String> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn handle(p: &Paths, line: &str) {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return eprintln!("dl-serve: bad request: {e}"),
    };
    let resp = match dispatch(p, &req) {
        Ok(data) => json!({ "ok": true, "data": data }),
        Err(e) => json!({ "ok": false, "error": e }),
    };
    let Some(reply) = req["reply"].as_str() else { return };
    // O_NONBLOCK: fails instead of hanging if the client has gone.
    match OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(reply) {
        Ok(mut f) => {
            let _ = writeln!(f, "{resp}");
        }
        Err(e) => eprintln!("dl-serve: cannot reply on {reply}: {e}"),
    }
}

fn dispatch(p: &Paths, req: &Value) -> Result<Value, String> {
    let prefix = req["id"].as_str().unwrap_or("");
    match req["cmd"].as_str().unwrap_or("") {
        "create" => runner::create(p, req),
        "start" => runner::start(p, prefix),
        "approve" => runner::approve(p, prefix),
        "reject" => runner::reject(p, prefix, req["note"].as_str().unwrap_or("")),
        "status" => runner::status(p, prefix),
        cmd => Err(format!("unknown command `{cmd}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;
    use std::path::PathBuf;

    fn paths(name: &str) -> Paths {
        let home = std::env::temp_dir().join(format!("dl-ipc-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        Paths { home: home.clone(), user_config: home.join("none.toml") }
    }

    #[test]
    fn dispatch_status_lists_tasks() {
        let p = paths("status");
        assert_eq!(dispatch(&p, &json!({ "cmd": "status", "id": "" })).unwrap(), json!([]));
        fs::remove_dir_all(&p.home).unwrap();
    }

    #[test]
    fn dispatch_rejects_unknown_command() {
        let p = paths("unknown");
        let err = dispatch(&p, &json!({ "cmd": "nope" })).unwrap_err();
        assert!(err.contains("unknown command `nope`"), "{err}");
    }

    #[test]
    fn dispatch_start_reports_missing_task() {
        let p = paths("start");
        let err = dispatch(&p, &json!({ "cmd": "start", "id": "missing" })).unwrap_err();
        assert!(err.contains("no task `missing`"), "{err}");
        fs::remove_dir_all(&p.home).unwrap();
    }

    #[test]
    fn handle_writes_ok_reply_to_reply_path() {
        let p = paths("reply");
        let reply = p.home.join("reply.out");
        fs::create_dir_all(&p.home).unwrap();
        fs::write(&reply, "").unwrap();
        let line = json!({ "cmd": "status", "id": "", "reply": reply.to_string_lossy() }).to_string();
        handle(&p, &line);
        let resp: Value = serde_json::from_str(&fs::read_to_string(&reply).unwrap()).unwrap();
        assert_eq!(resp, json!({ "ok": true, "data": [] }));
        fs::remove_dir_all(&p.home).unwrap();
    }

    #[test]
    fn mkfifo_creates_pipe_and_fails_when_present() {
        let path: PathBuf = paths("fifo").home.join("ctl.fifo");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        mkfifo(&path).unwrap();
        assert!(fs::metadata(&path).unwrap().file_type().is_fifo());
        assert!(mkfifo(&path).is_err());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
