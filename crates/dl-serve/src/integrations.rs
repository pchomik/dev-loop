use crate::config::Config;
use std::path::Path;
use std::process::Command;

/// Runs an external tool in `cwd`. Returns stderr on failure.
fn run(tool: &str, cwd: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(tool)
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("{tool}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{tool} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

pub fn worktree_add(project: &Path, wt: &Path, branch: &str) -> Result<(), String> {
    let wt = wt.to_string_lossy();
    run("git", project, &["worktree", "add", "-b", branch, wt.as_ref(), "HEAD"])
}

/// Removes the worktree with its uncommitted files, then force-deletes its branch, even with unmerged commits.
pub fn remove_worktree(project: &Path, wt: &Path, branch: &str) -> Result<(), String> {
    let wt = wt.to_string_lossy();
    run("git", project, &["worktree", "remove", "--force", wt.as_ref()])?;
    run("git", project, &["branch", "-D", branch])
}

pub fn commit(root: &Path, file: &Path, msg: &str, push: bool) -> Result<(), String> {
    let f = file.to_string_lossy();
    run("git", root, &["add", "--", f.as_ref()])?;
    run("git", root, &["commit", "-m", msg])?;
    if push {
        run("git", root, &["push", "-u", "origin", "HEAD"])?;
    }
    Ok(())
}

/// Pushes the checked-out branch under the same remote name, then opens the PR or MR.
/// True when a PR or MR is created for each task. Parallel tasks need this.
pub fn auto_create_enabled(cfg: &Config) -> bool {
    (cfg.github.enabled && cfg.github.auto_create_pr) || (cfg.gitlab.enabled && cfg.gitlab.auto_create_mr)
}

pub fn create_pr(cfg: &Config, wt: &Path) -> Result<(), String> {
    // Check each tool exists before pushing, so a missing tool leaves no pushed branch.
    if cfg.github.enabled && cfg.github.auto_create_pr {
        run(&cfg.github.gh_path, wt, &["--version"])?;
        run("git", wt, &["push", "-u", "origin", "HEAD"])?;
        run(&cfg.github.gh_path, wt, &["pr", "create", "--fill"])?;
    }
    if cfg.gitlab.enabled && cfg.gitlab.auto_create_mr {
        run(&cfg.gitlab.glab_path, wt, &["--version"])?;
        run("git", wt, &["push", "-u", "origin", "HEAD"])?;
        run(&cfg.gitlab.glab_path, wt, &["mr", "create", "--fill", "--yes"])?;
    }
    Ok(())
}

/// Logs to stderr and sends a Telegram message when Telegram is enabled.
pub fn notify(cfg: &Config, text: &str) {
    eprintln!("dl-serve: {text}");
    let (token, chat) = (&cfg.telegram.token, &cfg.telegram.chat_id);
    if !cfg.telegram.enabled || token.is_empty() || chat.is_empty() {
        return;
    }
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let chat_arg = format!("chat_id={chat}");
    let text_arg = format!("text={text}");
    let res = Command::new("curl")
        .args(["-sS", "-X", "POST", url.as_str(), "--data-urlencode", chat_arg.as_str(), "--data-urlencode", text_arg.as_str()])
        .output();
    match res {
        Ok(o) if o.status.success() => {}
        _ => eprintln!("dl-serve: telegram notification failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Paths};
    use std::path::PathBuf;
    use std::{env, fs, process};

    fn temp(name: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("dl-ix-{}-{name}", process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Creates a git repo with one commit at `dir`.
    fn git_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        run("git", dir, &["init", "-q"]).unwrap();
        run("git", dir, &["config", "user.email", "t@example.com"]).unwrap();
        run("git", dir, &["config", "user.name", "t"]).unwrap();
        fs::write(dir.join("README"), "x").unwrap();
        run("git", dir, &["add", "."]).unwrap();
        run("git", dir, &["commit", "-qm", "init"]).unwrap();
    }

    fn defaults() -> Config {
        let paths = Paths { home: env::temp_dir(), user_config: PathBuf::from("/nonexistent/dl.toml") };
        config::load(&paths, None).unwrap()
    }

    #[test]
    fn run_reports_missing_tool_and_failed_command() {
        let dir = temp("run");
        assert!(run("no-such-tool-dl", &dir, &[]).unwrap_err().contains("no-such-tool-dl"));
        git_repo(&dir);
        let err = run("git", &dir, &["checkout", "no-such-branch-dl"]).unwrap_err();
        assert!(err.contains("checkout"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn worktree_add_checks_out_new_branch() {
        let dir = temp("wt");
        let repo = dir.join("repo");
        git_repo(&repo);
        let wt = dir.join("wt");
        worktree_add(&repo, &wt, "dev-loop/x").unwrap();
        assert!(wt.join("README").is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remove_worktree_removes_uncommitted_files_and_branch_with_commits() {
        let dir = temp("rm");
        let repo = dir.join("repo");
        git_repo(&repo);
        let wt = dir.join("wt");
        worktree_add(&repo, &wt, "dev-loop/y").unwrap();
        fs::write(wt.join("README"), "y").unwrap();
        run("git", &wt, &["commit", "-qam", "work"]).unwrap();
        fs::write(wt.join("untracked.md"), "doc").unwrap();
        remove_worktree(&repo, &wt, "dev-loop/y").unwrap();
        assert!(!wt.exists());
        let branches = process::Command::new("git").current_dir(&repo).args(["branch", "--list", "dev-loop/y"]).output().unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn commit_adds_only_given_file() {
        let dir = temp("commit");
        git_repo(&dir);
        fs::create_dir_all(dir.join("docs")).unwrap();
        fs::write(dir.join("docs/a.md"), "doc").unwrap();
        commit(&dir, &dir.join("docs/a.md"), "dev-loop: test", false).unwrap();

        let log = process::Command::new("git").current_dir(&dir).args(["log", "-1", "--format=%s"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "dev-loop: test");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn commit_with_push_fails_without_remote() {
        let dir = temp("push");
        git_repo(&dir);
        fs::write(dir.join("a.md"), "doc").unwrap();
        assert!(commit(&dir, &dir.join("a.md"), "msg", true).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_pr_does_nothing_when_disabled() {
        assert!(create_pr(&defaults(), Path::new("/nonexistent")).is_ok());
    }

    #[test]
    fn create_pr_stops_before_push_when_gh_missing() {
        let mut cfg = defaults();
        cfg.github.enabled = true;
        cfg.github.auto_create_pr = true;
        cfg.github.gh_path = "/nonexistent/gh".into();
        assert!(create_pr(&cfg, Path::new("/nonexistent")).is_err());
    }
}
