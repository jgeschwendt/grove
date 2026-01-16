<h1 align="center"><code>grove</code></h1>

<p align="center">Cultivate your worktrees. Grow branches side by side—no stashing, no switching.</p>

---

---

## Install

```bash
curl -fsSL https://jgeschwendt.github.io/grove/scripts/install.sh | bash
```

Installs to `~/.grove/` and adds to PATH. Restart your shell or `source` your profile.

## CLI

```bash
grove           # Terminal UI
grove open      # http://localhost:7777
grove --help    # Print usage
```

## MCP

Endpoint: `http://localhost:7777/mcp`

Tools: `list_repositories`, `clone_repository`, `delete_repository`, `list_worktrees`, `create_worktree`, `delete_worktree`, `refresh_worktrees`

## Cloning Pattern

```
~/code/{username}/{repo}/
  ├── .bare/            # Bare repository
  ├── .trunk/           # Primary worktree
  ├── feature--auth/    # feature/auth (/ → --)
  └── bugfix--login/    # bugfix/login
```

Shared files (`.env`, `.claude/`, etc.) propagate from `.trunk` to all branches.
