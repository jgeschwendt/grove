<h1 align="center"><code>grove</code></h1>

<p align="center">Cultivate your worktrees. Grow branches side by side—no stashing, no switching.</p>

---

---

## Install

```bash
curl -fsSL https://jgeschwendt.github.io/grove/scripts/install.sh | bash
```

Installs to `~/.grove/` (symlinks to `~/.local/bin` if present).

## CLI

```bash
grove # open the text interface
```

```bash
grove open # the http interface
```

```bash
claude -p "how do i use grove?"
```

## MCP

Endpoint: `http://localhost:7777/mcp`

```bash
claude mcp add grove --transport http http://localhost:7777/mcp
```

```bash
claude -p "what tools are available in grove?"
```
