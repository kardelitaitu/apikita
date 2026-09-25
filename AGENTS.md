# Agent Guidelines & Workflow Rules

Rules that all agents working on this codebase must follow strictly without exception:

### 1. Temporary Scripts
- Any temporary script, scratch runner, or throwaway test file must be placed in the `.agents/` directory.
- Never place loose scratch scripts in the root directory or inside application source directories (`server/`, `website/`, `telegram/`).

### 2. Git Push Restriction
- **Never push to remote Git (`git push`) unless explicitly requested by the user.**
- All commits remain local on the active branch until the user specifically instructs to push.

### 3. Atomic One-Liner Git Commits (Strict Rule for Multi-Agent Work)
- **Always commit in a single, direct one-liner command.**
- **Never leave staged files sitting in the git index (`git add` without immediate commit).** Leaving uncommitted staged files causes index conflicts across concurrent agents.
- **Commands to use:**
  - For modified tracked files:
    ```bash
    git commit -a -m "<type>(<scope>): <concise message>"
    ```
  - When new files are created (untracked), add and commit atomically in one line:
    ```bash
    git add <path/to/files> && git commit -m "<type>(<scope>): <concise message>"
    ```
    *(In PowerShell: `git add <path/to/files>; git commit -m "<type>(<scope>): <concise message>"`)
- **Commit Message Standards:**
  - Always write a proper, meaningful commit message following Conventional Commits format:
    - `feat:` for new capabilities or endpoints
    - `fix:` for bug fixes
    - `perf:` for performance tuning
    - `test:` for benchmark or test suite additions
    - `docs:` for documentation and roadmap updates
    - `chore:` for maintenance, dependencies, or configuration
