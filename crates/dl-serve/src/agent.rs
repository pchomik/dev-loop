use crate::config::{AgentKind, Config};
use crate::log::Log;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// Process group ID of the running agent, by task ID. `kill` sends `SIGKILL` through it.
static RUNNING: Mutex<BTreeMap<String, i32>> = Mutex::new(BTreeMap::new());

pub struct Outcome {
    pub text: String,
    pub cost: f64,
    pub error: Option<String>,
    pub timed_out: bool,
    pub secs: i64,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.error.is_none() && !self.timed_out
    }
}

#[derive(Default)]
struct Parsed {
    text: String,
    cost: f64,
    error: Option<String>,
}

/// Runs one agent turn in `cwd` for task `id`. Process group is killed at `timeout` or by `kill`.
/// Prompt, agent text and failure stderr go to the task log at `log_path`.
/// `alive` is checked under the registry lock before spawn. Returns `None` without spawn when it is false.
#[allow(clippy::too_many_arguments)]
pub fn run(
    cfg: &Config,
    prompt: &str,
    skill: Option<&str>,
    cwd: &Path,
    id: &str,
    log_path: &Path,
    timeout: Duration,
    alive: impl Fn() -> Result<bool, String>,
) -> Result<Option<Outcome>, String> {
    let kind = cfg.agent.kind;
    let (bin, args): (&str, Vec<String>) = match kind {
        AgentKind::Pi => {
            let msg = match skill {
                Some(s) => format!("/skill:{s} {prompt}"),
                None => prompt.to_string(),
            };
            (&cfg.agent.pi_path, vec!["--mode".into(), "json".into(), "--no-session".into(), msg])
        }
        AgentKind::Cursor => {
            let msg = match skill {
                Some(s) => format!("Use the agent skill `{s}`. {prompt}"),
                None => prompt.to_string(),
            };
            (
                &cfg.agent.cursor_path,
                vec!["-p".into(), "--output-format".into(), "stream-json".into(), "--force".into(), msg],
            )
        }
    };

    let mut log = Log::open(log_path)?;
    let mut parser_log = Log::open(log_path)?;
    let start = Instant::now();
    // Check, spawn and register under one lock, so `kill` cannot miss a starting group.
    let mut child = {
        let mut running = RUNNING.lock().unwrap();
        if !alive()? {
            return Ok(None);
        }
        let child = Command::new(bin)
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("cannot start `{bin}`: {e}"))?;
        running.insert(id.into(), child.id() as i32);
        child
    };
    let pid = child.id() as i32;
    log.write(&format!("=== {} ===\n{prompt}\n", skill.unwrap_or("describe")));
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let out_thread = thread::spawn(move || parse(kind, stdout, &mut parser_log));
    let err_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            // Negative PID kills the whole process group started by process_group(0).
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            break child.wait().map_err(|e| e.to_string())?;
        }
        thread::sleep(Duration::from_millis(100));
    };
    {
        let mut running = RUNNING.lock().unwrap();
        // A describe step for the same task may have registered its own group. Keep that one.
        if running.get(id) == Some(&pid) {
            running.remove(id);
        }
    }

    let parsed = out_thread.join().unwrap_or_default();
    let stderr_text = err_thread.join().unwrap_or_default();
    if !status.success() && !timed_out && !stderr_text.is_empty() {
        log.write(&format!("{stderr_text}\n"));
    }
    let error = parsed.error.or_else(|| {
        (!status.success() && !timed_out).then(|| {
            if stderr_text.is_empty() { format!("exit {status}") } else { stderr_text }
        })
    });
    Ok(Some(Outcome {
        text: parsed.text,
        cost: parsed.cost,
        error,
        timed_out,
        secs: start.elapsed().as_secs_f64().ceil() as i64,
    }))
}

/// Kills the agent process group of task `id`. Does nothing when no agent runs for it.
pub fn kill(id: &str) {
    if let Some(&pgid) = RUNNING.lock().unwrap().get(id) {
        // Negative PID kills the whole process group started by process_group(0).
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

/// pi: `message_end` events. Cursor: final `result` event. Missing cost counts as 0 (D-004).
/// pi `text_delta` events stream to `log` as they arrive.
fn parse(kind: AgentKind, r: impl Read, log: &mut Log) -> Parsed {
    let mut p = Parsed::default();
    for line in BufReader::new(r).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match kind {
            AgentKind::Pi => {
                if v["type"] == "message_update" && v["assistantMessageEvent"]["type"] == "text_delta" {
                    log.delta(v["assistantMessageEvent"]["delta"].as_str().unwrap_or_default());
                    continue;
                }
                if v["type"] != "message_end" || v["message"]["role"] != "assistant" {
                    continue;
                }
                let m = &v["message"];
                p.cost += m["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
                let text: String = m["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "text")
                            .filter_map(|b| b["text"].as_str())
                            .collect()
                    })
                    .unwrap_or_default();
                log.finish(&text);
                if !text.is_empty() {
                    p.text = text;
                }
                p.error = (m["stopReason"] == "error")
                    .then(|| m["errorMessage"].as_str().unwrap_or("agent error").to_string());
            }
            AgentKind::Cursor => {
                if v["type"] != "result" {
                    continue;
                }
                p.text = v["result"].as_str().unwrap_or_default().to_string();
                log.finish(&p.text);
                p.error = v["is_error"].as_bool().unwrap_or(false).then(|| p.text.clone());
            }
        }
    }
    p
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{self, Paths};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::{env, fs, process};

    fn log_file() -> PathBuf {
        env::temp_dir().join(format!("dl-agent-{}.log", process::id()))
    }

    fn parse_str(kind: AgentKind, input: &str) -> Parsed {
        parse(kind, input.as_bytes(), &mut Log::open(&log_file()).unwrap())
    }

    fn cfg(kind: AgentKind, bin: &str) -> Config {
        let paths = Paths { home: env::temp_dir(), user_config: PathBuf::from("/nonexistent/dl.toml") };
        let mut c = config::load(&paths, None).unwrap();
        c.agent.kind = kind;
        c.agent.pi_path = bin.into();
        c.agent.cursor_path = bin.into();
        c
    }

    /// Writes an executable `sh` script and returns its path.
    pub(crate) fn script(name: &str, body: &str) -> String {
        let dir = env::temp_dir().join(format!("dl-agent-{}-{name}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent");
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn parse_pi_sums_cost_and_keeps_last_text() {
        let input = concat!(
            r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"skip"}]}}"#, "\n",
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"a"}],"usage":{"cost":{"total":0.5}}}}"#, "\n",
            "not json\n",
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"b"}],"usage":{"cost":{"total":0.25}}}}"#, "\n",
        );
        let p = parse_str(AgentKind::Pi, input);
        assert_eq!(p.text, "b");
        assert_eq!(p.cost, 0.75);
        assert!(p.error.is_none());
    }

    #[test]
    fn parse_pi_missing_cost_counts_zero() {
        let input = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"x"}]}}"#;
        assert_eq!(parse_str(AgentKind::Pi, input).cost, 0.0);
    }

    #[test]
    fn parse_pi_error_stop_reason_sets_error() {
        let input = r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"boom"}}"#;
        assert_eq!(parse_str(AgentKind::Pi, input).error.as_deref(), Some("boom"));
    }

    #[test]
    fn parse_cursor_reads_result_event() {
        let ok = r#"{"type":"result","result":"done","is_error":false}"#;
        let p = parse_str(AgentKind::Cursor, ok);
        assert_eq!(p.text, "done");
        assert!(p.error.is_none());

        let bad = r#"{"type":"result","result":"bad","is_error":true}"#;
        assert_eq!(parse_str(AgentKind::Cursor, bad).error.as_deref(), Some("bad"));
    }

    #[test]
    fn run_returns_text_and_cost_from_agent_output() {
        let bin = script(
            "ok",
            r#"echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"cost":{"total":0.5}}}}'"#,
        );
        let out = run(&cfg(AgentKind::Pi, &bin), "p", None, &env::temp_dir(), "t", &log_file(), Duration::from_secs(10), || Ok(true)).unwrap().unwrap();
        assert!(out.ok());
        assert_eq!(out.text, "hi");
        assert_eq!(out.cost, 0.5);
    }

    #[test]
    fn run_logs_prompt_streamed_text_and_final_text() {
        let bin = script(
            "log",
            r#"echo '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Hel"}}'; echo '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"lo"}}'; echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Hello!"}]}}'"#,
        );
        let path = env::temp_dir().join(format!("dl-agent-log-{}.log", process::id()));
        let _ = fs::remove_file(&path);
        run(&cfg(AgentKind::Pi, &bin), "PROMPT", Some("skill-x"), &env::temp_dir(), "t", &path, Duration::from_secs(10), || Ok(true)).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "=== skill-x ===\nPROMPT\nHello!\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn run_kills_agent_at_timeout() {
        let bin = script("slow", "sleep 5");
        let out = run(&cfg(AgentKind::Pi, &bin), "p", None, &env::temp_dir(), "t", &log_file(), Duration::from_secs(1), || Ok(true)).unwrap().unwrap();
        assert!(out.timed_out);
        assert!(!out.ok());
        assert!(out.secs < 5);
    }

    #[test]
    fn kill_stops_running_agent_before_timeout() {
        let bin = script("kill", "sleep 30");
        let c = cfg(AgentKind::Pi, &bin);
        let h = thread::spawn(move || run(&c, "p", None, &env::temp_dir(), "k", &log_file(), Duration::from_secs(60), || Ok(true)).unwrap().unwrap());
        thread::sleep(Duration::from_millis(300));
        kill("k");
        let out = h.join().unwrap();
        assert!(!out.timed_out);
        assert!(out.secs < 10);
    }

    #[test]
    fn run_does_not_spawn_when_not_alive() {
        let marker = env::temp_dir().join(format!("dl-agent-{}-marker", process::id()));
        let _ = fs::remove_file(&marker);
        let bin = script("refuse", &format!("touch {}", marker.display()));
        let out = run(&cfg(AgentKind::Pi, &bin), "p", None, &env::temp_dir(), "t", &log_file(), Duration::from_secs(10), || Ok(false)).unwrap();
        assert!(out.is_none());
        assert!(!marker.exists());
    }

    #[test]
    fn run_reports_stderr_on_failed_exit() {
        let bin = script("fail", "echo oops >&2; exit 3");
        let out = run(&cfg(AgentKind::Pi, &bin), "p", None, &env::temp_dir(), "t", &log_file(), Duration::from_secs(10), || Ok(true)).unwrap().unwrap();
        assert_eq!(out.error.as_deref(), Some("oops"));
    }

    #[test]
    fn run_errors_when_binary_missing() {
        let r = run(&cfg(AgentKind::Pi, "/nonexistent/pi"), "p", None, &env::temp_dir(), "t", &log_file(), Duration::from_secs(1), || Ok(true));
        assert!(r.is_err());
    }
}
