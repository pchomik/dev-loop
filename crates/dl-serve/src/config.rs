use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
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

fn default_iterations() -> i64 {
    3
}

fn default_time_limit() -> String {
    "1h".into()
}

#[derive(Deserialize, Clone)]
pub struct Loop {
    pub name: String,
    pub steps: Vec<String>,
    #[serde(default = "default_iterations")]
    pub iterations: i64,
    #[serde(default = "default_time_limit")]
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
    /// Run order is the array order.
    pub loops: Vec<Loop>,
    pub github: Github,
    pub gitlab: Gitlab,
    pub telegram: Telegram,
    pub markdown: Markdown,
    pub task: Task,
}

impl Config {
    /// Errors when no loop has this name, e.g. a loop removed from config.
    pub fn loop_named(&self, name: &str) -> Result<&Loop, String> {
        self.loops.iter().find(|l| l.name == name).ok_or_else(|| format!("loop `{name}` is not defined"))
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
    if cfg.loops.is_empty() {
        return Err("config needs at least one loop".into());
    }
    let mut loop_names = HashSet::new();
    let mut step_names = HashSet::new();
    for lp in &cfg.loops {
        if lp.name.is_empty() {
            return Err("loop name must not be empty".into());
        }
        if !loop_names.insert(lp.name.as_str()) {
            return Err(format!("loop `{}` is defined twice", lp.name));
        }
        if lp.steps.is_empty() {
            return Err(format!("loop `{}` needs steps", lp.name));
        }
        if lp.iterations < 0 {
            return Err("iterations must be 0 or more".into());
        }
        for s in &lp.steps {
            if !cfg.steps.contains_key(s) {
                return Err(format!("loop step `{s}` is not defined under [steps]"));
            }
            if !step_names.insert(s.as_str()) {
                return Err(format!("step `{s}` is used more than once across loops"));
            }
        }
        if lp.time_limit_secs()? < 0 {
            return Err(format!("time_limit `{}` must be 0 or more", lp.time_limit));
        }
    }
    for (name, s) in &cfg.steps {
        if s.kind == StepKind::Review && s.output.is_none() {
            return Err(format!("review step `{name}` needs output"));
        }
        if s.kind != StepKind::UserReview && s.skill.is_none() {
            return Err(format!("step `{name}` needs skill"));
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
        fs::write(&user, "[markdown]\nauto_commit = true\n[[loops]]\nname = \"definition\"\nsteps = [\"research\"]\ntime_limit = \"30m\"\n").unwrap();
        let proj = dir.join("proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("dev-loop.toml"), "[markdown]\nauto_commit = false\n").unwrap();
        let paths = Paths { home: dir.clone(), user_config: user };
        let cfg = load(&paths, Some(&proj)).unwrap();
        assert!(!cfg.markdown.auto_commit);
        assert_eq!(cfg.loop_named("definition").unwrap().time_limit_secs().unwrap(), 1800);
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
        let lp = |t: &str| Loop { name: "x".into(), steps: vec![], iterations: 1, time_limit: t.into() };
        assert_eq!(lp("1h").time_limit_secs().unwrap(), 3600);
        assert_eq!(lp("30m").time_limit_secs().unwrap(), 1800);
        assert_eq!(lp("45s").time_limit_secs().unwrap(), 45);
        assert_eq!(lp("90").time_limit_secs().unwrap(), 90);
        assert!(lp("1x").time_limit_secs().is_err());
    }

    #[test]
    fn load_rejects_loop_step_not_defined() {
        let err = load_user("step", "[[loops]]\nname = \"definition\"\nsteps = [\"nope\"]\n").err().unwrap();
        assert!(err.contains("`nope` is not defined"), "{err}");
    }

    #[test]
    fn load_rejects_loop_without_steps() {
        let err = load_user("empty", "[[loops]]\nname = \"a\"\nsteps = []\n").err().unwrap();
        assert!(err.contains("loop `a` needs steps"), "{err}");
    }

    #[test]
    fn zero_limits_mean_no_limit_and_negative_is_rejected() {
        let cfg = load_user("zero", "[[loops]]\nname = \"execution\"\nsteps = [\"development\"]\niterations = 0\ntime_limit = \"0\"\n").unwrap();
        assert_eq!(cfg.loop_named("execution").unwrap().iterations, 0);
        assert_eq!(cfg.loop_named("execution").unwrap().time_limit_secs().unwrap(), 0);
        let err = load_user("iter", "[[loops]]\nname = \"execution\"\nsteps = [\"development\"]\niterations = -1\n").err().unwrap();
        assert!(err.contains("iterations must be 0 or more"), "{err}");
        let err = load_user("time", "[[loops]]\nname = \"execution\"\nsteps = [\"development\"]\ntime_limit = \"-5\"\n").err().unwrap();
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
        assert_eq!(cfg.loop_named("definition").unwrap().steps[0], "research");
        assert_eq!(cfg.loop_named("execution").unwrap().steps[0], "development");
        for name in ["definition", "execution"] {
            let lp = cfg.loop_named(name).unwrap();
            assert_eq!((lp.iterations, lp.time_limit_secs().unwrap()), (3, 3600), "{name}");
        }
        assert!(cfg.loop_named("other").err().unwrap().contains("loop `other` is not defined"));
    }

    #[test]
    fn custom_loops_keep_array_order_and_default_limits() {
        let cfg = load_user(
            "custom",
            "[steps.a]\nskill = \"s\"\n[steps.b]\nskill = \"s\"\n[[loops]]\nname = \"zeta\"\nsteps = [\"b\"]\n[[loops]]\nname = \"alpha\"\nsteps = [\"a\"]\niterations = 5\n",
        )
        .unwrap();
        let names: Vec<_> = cfg.loops.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha"]);
        assert_eq!((cfg.loops[0].iterations, cfg.loops[0].time_limit.as_str()), (3, "1h"));
        assert_eq!((cfg.loops[1].iterations, cfg.loops[1].time_limit_secs().unwrap()), (5, 3600));
    }

    #[test]
    fn load_accepts_twenty_steps_across_two_loops() {
        let names: Vec<String> = (0..20).map(|i| format!("\"s{i}\"")).collect();
        let mut toml: String = (0..20).map(|i| format!("[steps.s{i}]\nskill = \"s\"\n")).collect();
        toml += &format!("[[loops]]\nname = \"a\"\nsteps = [{}]\n", names[..10].join(", "));
        toml += &format!("[[loops]]\nname = \"b\"\nsteps = [{}]\n", names[10..].join(", "));
        let cfg = load_user("twenty", &toml).unwrap();
        assert_eq!(cfg.loops.len(), 2);
    }

    #[test]
    fn load_rejects_step_without_skill_and_review_without_output() {
        let err = load_user("no-skill", "[steps.bad]\nlabel = \"x\"\n[[loops]]\nname = \"a\"\nsteps = [\"bad\"]\n").err().unwrap();
        assert!(err.contains("step `bad` needs skill"), "{err}");
        let toml = "[steps.rv]\nkind = \"review\"\nskill = \"s\"\n[[loops]]\nname = \"a\"\nsteps = [\"rv\"]\n";
        let err = load_user("no-output", toml).err().unwrap();
        assert!(err.contains("review step `rv` needs output"), "{err}");
    }

    #[test]
    fn load_rejects_duplicate_loop_or_step_names_and_old_shape() {
        let dup = "[[loops]]\nname = \"build\"\nsteps = [\"research\"]\n[[loops]]\nname = \"build\"\nsteps = [\"frd\"]\n";
        assert!(load_user("dup-loop", dup).err().unwrap().contains("loop `build` is defined twice"));
        let dup_step = "[[loops]]\nname = \"a\"\nsteps = [\"research\"]\n[[loops]]\nname = \"b\"\nsteps = [\"research\"]\n";
        assert!(load_user("dup-step", dup_step).err().unwrap().contains("`research` is used more than once"));
        assert!(load_user("old", "[loops.definition]\nsteps = [\"research\"]\n").is_err());
    }

    #[test]
    fn task_prefix_defaults_to_task_and_can_be_set() {
        assert_eq!(load_user("prefix-default", "").unwrap().task.prefix, "TASK-");
        let cfg = load_user("prefix-set", "[task]\nprefix = \"PROJ-\"\n").unwrap();
        assert_eq!(cfg.task.prefix, "PROJ-");
    }
}
