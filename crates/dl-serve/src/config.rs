use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{env, fs, io};
use toml::{Table, Value};

const DEFAULTS: &str = include_str!("default-config.toml");

#[derive(Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub user_config: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let base = PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()));
        Self {
            home: base.join(".local/share/dev-loop"),
            user_config: base.join(".config/dev-loop/config.toml"),
        }
    }
    pub fn db(&self) -> PathBuf {
        self.home.join("state.db")
    }
    pub fn pipe(&self) -> PathBuf {
        self.home.join("ctl.fifo")
    }
    pub fn worktrees(&self) -> PathBuf {
        self.home.join("worktrees")
    }
    pub fn log(&self, id: &str) -> PathBuf {
        self.home.join("logs").join(format!("{id}.log"))
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Pi,
    Cursor,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    #[default]
    Agent,
    Review,
    UserReview,
}

#[derive(Deserialize, Clone)]
pub struct Agent {
    pub kind: AgentKind,
    pub pi_path: String,
    pub cursor_path: String,
    pub pi_args: Vec<String>,
    pub cursor_args: Vec<String>,
}

#[derive(Deserialize, Clone)]
pub struct Step {
    #[serde(default)]
    pub kind: StepKind,
    pub skill: Option<String>,
    pub output: Option<String>,
    pub label: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct Loop {
    pub steps: Vec<String>,
    pub iterations: i64,
    pub time_limit: String,
}

impl Loop {
    /// Seconds. `0` means no time limit.
    pub fn time_limit_secs(&self) -> Result<i64, String> {
        let s = self.time_limit.as_str();
        let (num, mult) = if let Some(n) = s.strip_suffix('h') {
            (n, 3600)
        } else if let Some(n) = s.strip_suffix('m') {
            (n, 60)
        } else if let Some(n) = s.strip_suffix('s') {
            (n, 1)
        } else {
            (s, 1)
        };
        num.parse::<i64>()
            .map(|n| n * mult)
            .map_err(|_| format!("invalid time limit `{s}`"))
    }
}

#[derive(Deserialize, Clone)]
pub struct Loops {
    pub definition: Loop,
    pub execution: Loop,
}

#[derive(Deserialize, Clone)]
pub struct Github {
    pub enabled: bool,
    pub gh_path: String,
    pub auto_create_pr: bool,
}

#[derive(Deserialize, Clone)]
pub struct Gitlab {
    pub enabled: bool,
    pub glab_path: String,
    pub auto_create_mr: bool,
}

#[derive(Deserialize, Clone)]
pub struct Telegram {
    pub enabled: bool,
    pub token: String,
    pub chat_id: String,
}

#[derive(Deserialize, Clone)]
pub struct Markdown {
    pub dir: String,
    pub repo: String,
    pub auto_commit: bool,
    pub auto_push: bool,
}

#[derive(Deserialize, Clone)]
pub struct Task {
    pub prefix: String,
}

#[derive(Deserialize, Clone)]
pub struct Config {
    pub agent: Agent,
    pub steps: BTreeMap<String, Step>,
    pub loops: Loops,
    pub github: Github,
    pub gitlab: Gitlab,
    pub telegram: Telegram,
    pub markdown: Markdown,
    pub task: Task,
}

impl Config {
    /// Loop name is `definition` or `execution`.
    pub fn loop_named(&self, name: &str) -> &Loop {
        match name {
            "definition" => &self.loops.definition,
            _ => &self.loops.execution,
        }
    }
}

/// Loads defaults, then user config, then project config. Later values win.
pub fn load(paths: &Paths, project: Option<&Path>) -> Result<Config, String> {
    let mut t: Table = DEFAULTS.parse().map_err(|e| format!("defaults: {e}"))?;
    merge(&mut t, read_table(&paths.user_config)?);
    if let Some(dir) = project {
        merge(&mut t, read_table(&dir.join("dev-loop.toml"))?);
    }
    let cfg: Config = Value::Table(t).try_into().map_err(|e| format!("config: {e}"))?;
    for lp in [&cfg.loops.definition, &cfg.loops.execution] {
        if lp.steps.is_empty() {
            return Err("each loop needs steps".into());
        }
        if lp.iterations < 0 {
            return Err("iterations must be 0 or more".into());
        }
        for s in &lp.steps {
            if !cfg.steps.contains_key(s) {
                return Err(format!("loop step `{s}` is not defined under [steps]"));
            }
        }
        if lp.time_limit_secs()? < 0 {
            return Err(format!("time_limit `{}` must be 0 or more", lp.time_limit));
        }
    }
    Ok(cfg)
}

fn read_table(path: &Path) -> Result<Table, String> {
    match fs::read_to_string(path) {
        Ok(s) => s.parse().map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Table::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn merge(base: &mut Table, over: Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(Value::Table(b)), Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_overrides_user_and_time_limit_parses() {
        let dir = env::temp_dir().join(format!("dl-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.toml");
        fs::write(&user, "[markdown]\nauto_commit = true\n[loops.definition]\ntime_limit = \"30m\"\n").unwrap();
        let proj = dir.join("proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("dev-loop.toml"), "[markdown]\nauto_commit = false\n").unwrap();
        let paths = Paths { home: dir.clone(), user_config: user };
        let cfg = load(&paths, Some(&proj)).unwrap();
        assert!(!cfg.markdown.auto_commit);
        assert_eq!(cfg.loops.definition.time_limit_secs().unwrap(), 1800);
        assert_eq!(cfg.loops.execution.time_limit_secs().unwrap(), 3600);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn load_user(name: &str, toml: &str) -> Result<Config, String> {
        let dir = env::temp_dir().join(format!("dl-cfg-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.toml");
        fs::write(&user, toml).unwrap();
        let paths = Paths { home: dir.clone(), user_config: user };
        let r = load(&paths, None);
        fs::remove_dir_all(&dir).unwrap();
        r
    }

    #[test]
    fn time_limit_units() {
        let lp = |t: &str| Loop { steps: vec![], iterations: 1, time_limit: t.into() };
        assert_eq!(lp("1h").time_limit_secs().unwrap(), 3600);
        assert_eq!(lp("30m").time_limit_secs().unwrap(), 1800);
        assert_eq!(lp("45s").time_limit_secs().unwrap(), 45);
        assert_eq!(lp("90").time_limit_secs().unwrap(), 90);
        assert!(lp("1x").time_limit_secs().is_err());
    }

    #[test]
    fn load_rejects_loop_step_not_defined() {
        let err = load_user("step", "[loops.definition]\nsteps = [\"nope\"]\n").err().unwrap();
        assert!(err.contains("`nope` is not defined"), "{err}");
    }

    #[test]
    fn zero_limits_mean_no_limit_and_negative_is_rejected() {
        let cfg = load_user("zero", "[loops.execution]\niterations = 0\ntime_limit = \"0\"\n").unwrap();
        assert_eq!(cfg.loops.execution.iterations, 0);
        assert_eq!(cfg.loops.execution.time_limit_secs().unwrap(), 0);
        let err = load_user("iter", "[loops.execution]\niterations = -1\n").err().unwrap();
        assert!(err.contains("iterations must be 0 or more"), "{err}");
        let err = load_user("time", "[loops.execution]\ntime_limit = \"-5\"\n").err().unwrap();
        assert!(err.contains("must be 0 or more"), "{err}");
    }

    #[test]
    fn agent_args_default_to_empty_when_unset() {
        let cfg = load_user("args", "[agent]\nkind = \"pi\"\npi_path = \"pi\"\n").unwrap();
        assert!(cfg.agent.pi_args.is_empty());
        assert!(cfg.agent.cursor_args.is_empty());
    }

    #[test]
    fn loop_named_maps_execution_by_default() {
        let cfg = load_user("named", "").unwrap();
        assert_eq!(cfg.loop_named("definition").steps[0], "research");
        assert_eq!(cfg.loop_named("execution").steps[0], "development");
        assert_eq!(cfg.loop_named("other").steps[0], "development");
    }

    #[test]
    fn task_prefix_defaults_to_task_and_can_be_set() {
        assert_eq!(load_user("prefix-default", "").unwrap().task.prefix, "TASK-");
        let cfg = load_user("prefix-set", "[task]\nprefix = \"PROJ-\"\n").unwrap();
        assert_eq!(cfg.task.prefix, "PROJ-");
    }
}
