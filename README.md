<p>
    <h1 align="center">dev-loop</h1>
    <p align="center">
        <img src="https://badgen.net/badge/rust/1.85+/orange">
        <img src="https://badgen.net/badge/edition/2024/orange">
    </p>
    <p align="center">
        <img src="https://badgen.net/badge/os/linux/blue">
        <img src="https://badgen.net/badge/os/macos/blue">
    </p>
    <p align="center">
        dev-loop orchestrates AI agent steps (pi-agent, Cursor) for feature development loops. <br>
        Each feature runs a definition loop and an execution loop in its task directory.
    </p>
</p>

## Available features

- Runs each feature through two loops: Definition (Research, FRD, user review) and Execution (Development, code review, rules review, simplification review).
- Runs each step as an agent skill set in configuration, on pi-agent or Cursor.
- User review step notifies you and waits for approval. No agent runs until you approve or reject.
- Each task runs in its `--path` directory. `--create-worktree` gives a task its own git worktree named by task ID.
- Enforces an iteration limit per loop (default `3`) and a time limit per loop (default `1h`).
- Checks that each step's output file exists when the step defines one.
- Creates tasks from Jira, GitHub issue, GitLab issue or a local file.
- Stores local state in SQLite.
- Optional GitHub (`gh`) and GitLab (`glab`) integration: pull request and merge request creation.
- Optional Telegram notifications.
- Command-line client (`dl`) to create, start, approve, reject and check tasks.

## Crates

| Crate            | Binary           | Role                                                        |
|------------------|------------------|-------------------------------------------------------------|
| `crates/dl-serve`     | `dl-serve`     | Main app. Runs loops, steps and agents. Listens on a named pipe. |
| `crates/dl` | `dl` | Client. Sends JSON commands to the main app over the named pipe. |

## Workflow

```
Task created
  -> Definition loop: research -> frd -> user-review (approve or reject)
  -> Execution loop:  development -> code-review -> rules-review -> simplification-review
```

A rejected user review restarts the Definition loop. Each review step writes a findings JSON file. Open `blocker` or `major` findings send the task back to development. Development answers each finding with `fixed` or `wontfix`. The review closes it or reopens it. `minor` and `nit` findings never block.

## Usage

Start the main app. It creates a named pipe in `~/.local/share/dev-loop/`:

```sh
    dl-serve
```

Then use the client:

```sh
    # Create a task from a request, prints the task ID
    dl create --path ~/src/my-app --request "Add CSV export to reports"

    # Create a task from a local file, with an issue reference
    dl create --path ~/src/my-app --source file --issue PROJ-123 --file feature.md

    # Create a task with its own worktree, default path is the current directory
    dl create --create-worktree --request "Add CSV export to reports"

    # Start a new or failed task
    dl start <ID>

    # Approve or reject the user review step
    dl approve <ID>
    dl reject <ID> --note "Missing error cases"

    # Show all tasks, or one task by ID
    dl status
    dl status <ID>
```

`--request` and `--file` are mutually exclusive. Use one of them.

## Configuration

dev-loop reads two files in the same format. Project values override user values.

| File                                  | Scope                          |
|---------------------------------------|--------------------------------|
| `~/.config/dev-loop/config.toml`      | User. |
| `dev-loop.toml`                       | Project. Overrides user values. |

Built-in defaults live in [`crates/dl-serve/src/default-config.toml`](crates/dl-serve/src/default-config.toml).

<details>

<summary>agent</summary>

### agent

Choose the agent and set the binary paths.

```toml
    [agent]
    kind = "pi"              # pi | cursor
    pi_path = "pi"
    cursor_path = "cursor-agent"
```

</details>

<details>

<summary>steps</summary>

### steps

Each step sets a `label`, and optionally a `skill` and an `output` file. `<KEY>` in `output` is the issue ID, or the task ID when no issue is set.

```toml
    [steps.research]
    skill = "create-research"
    output = "<KEY>-research.md"
    label = "dl:research"

    [steps.user-review]
    kind = "user-review"     # agent | review | user-review
    label = "dl:user-review"
```

</details>

<details>

<summary>loops</summary>

### loops

Set the steps of each loop, the iteration limit and the time limit.

```toml
    [loops.definition]
    steps = ["research", "frd", "user-review"]
    iterations = 3
    time_limit = "1h"
```

</details>

<details>

<summary>github and gitlab</summary>

### github and gitlab

Enable the tool and set its path. Authenticate `gh` or `glab` outside dev-loop. Tokens are not stored in configuration.

```toml
    [github]
    enabled = false
    gh_path = "gh"
    auto_create_pr = false

    [gitlab]
    enabled = false
    glab_path = "glab"
    auto_create_mr = false
```

</details>

<details>

<summary>telegram</summary>

### telegram

Send notifications to a Telegram chat. Used for notifications only.

```toml
    [telegram]
    enabled = true
    token = "123456:ABC"
    chat_id = "987654321"
```

</details>

<details>

<summary>markdown</summary>

### markdown

Set where produced documents go and whether to commit and push them.

```toml
    [markdown]
    dir = ".features"        # folder for <KEY>/ documents
    repo = ""                # empty: task worktree
    auto_commit = false
    auto_push = false
```

</details>

Jira access goes through an MCP server configured in pi-agent or Cursor, not in dev-loop.

## Continuous Integration

[![CI](https://github.com/pchomik/dev-loop/workflows/CI/badge.svg)](https://github.com/pchomik/dev-loop/actions)

CI runs Clippy and tests on Linux and macOS.

## Install

### From source

Requires a Rust toolchain ([rustup](https://rustup.rs/)) and git. Install the agent tools you use (`pi` or `cursor-agent`), plus `gh` or `glab` when enabled.

```sh
    git clone https://github.com/pchomik/dev-loop
    cd dev-loop
    cargo build --release --workspace --locked
```

Binaries are in `target/release/`: `dl-serve` and `dl`.

```sh
    cargo install --path crates/dl-serve
    cargo install --path crates/dl
```

### From source for testing

```sh
    cargo test --workspace --locked
    cargo clippy --workspace --all-targets --locked -- -D warnings
```

## Contribution

Please feel free to present your idea by code example (pull request) or reported issues.

- Issues: [github.com/pchomik/dev-loop/issues](https://github.com/pchomik/dev-loop/issues)
- Repository: [github.com/pchomik/dev-loop](https://github.com/pchomik/dev-loop)

## License

MIT. See [LICENSE](LICENSE).
