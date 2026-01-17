# Branch Management Guide

This document describes the Git workflow for maintaining a distributable downstream fork while staying synchronized with the upstream repository.

## Repository Setup

### Remotes

| Remote     | URL                                          | Purpose                |
| ---------- | -------------------------------------------- | ---------------------- |
| `origin`   | `git@github.com:xiaot623/vibe-kanban.git`    | Your fork (downstream) |
| `upstream` | `https://github.com/BloopAI/vibe-kanban.git` | Original repository    |

### Adding Upstream Remote

```bash
git remote add upstream https://github.com/BloopAI/vibe-kanban.git
git fetch upstream
```

## Branch Architecture

```
upstream/main ──────────────────────────────────────────►
       │                    │                    │
       ▼                    ▼                    ▼
    origin/main ─────────────────────────────────────────► (tracks upstream)
       │                    │                    │
       │         merge      │         merge      │
       ▼                    ▼                    ▼
    origin/release ──────────────────────────────────────► (distributable)
       │
       ├── local/feature-a  (personal features)
       ├── local/feature-b
       │
       └── feature/xxx ──────► PR to upstream (contributions)
```

## Branch Naming Conventions

| Prefix      | Purpose                                            | Example              |
| ----------- | -------------------------------------------------- | -------------------- |
| `main`      | Mirror of upstream, kept clean                     | -                    |
| `release`   | Your distributable version with all customizations | -                    |
| `local/*`   | Personal features, not intended for upstream       | `local/custom-theme` |
| `feature/*` | Features planned for upstream contribution         | `feature/dark-mode`  |
| `fix/*`     | Bug fixes (may contribute upstream)                | `fix/login-crash`    |

## Common Operations

### 1. Sync Upstream to Main

Keep your `main` branch in sync with upstream:

```bash
git fetch upstream
git checkout main
git merge upstream/main --ff-only
git push origin main
```

### 2. Update Release with Upstream Changes

Merge upstream updates into your distributable release:

```bash
git checkout release
git merge main
# Resolve conflicts if any
git push origin release
```

### 3. Develop Personal Features

Features only for your downstream distribution:

```bash
# Create feature branch from release
git checkout release
git checkout -b local/my-feature

# ... develop and commit ...

# Merge back to release
git checkout release
git merge local/my-feature
git push origin release

# Optionally delete the branch
git branch -d local/my-feature
```

### 4. Develop Contributable Features

Features intended for upstream contribution:

```bash
# Always branch from latest upstream main
git checkout main
git pull upstream main
git checkout -b feature/cool-feature

# ... develop and commit ...

# Push to your fork
git push origin feature/cool-feature

# Create PR at https://github.com/BloopAI/vibe-kanban
```

### 5. After Contribution is Accepted

When your PR is merged upstream:

```bash
# Sync upstream changes
git fetch upstream
git checkout main
git merge upstream/main --ff-only
git push origin main

# Update release (feature now comes from upstream)
git checkout release
git merge main
git push origin release

# Clean up feature branch
git branch -d feature/cool-feature
git push origin --delete feature/cool-feature
```

## Initial Setup

### Create the Release Branch

```bash
git checkout main
git checkout -b release
git push -u origin release
```

## Best Practices

1. **Keep `main` clean** - Never commit directly to `main`. It should only receive merges from `upstream/main`.

2. **Rebase before contributing** - Before creating a PR, rebase your feature branch on the latest `upstream/main`:
   ```bash
   git checkout feature/my-feature
   git fetch upstream
   git rebase upstream/main
   ```

3. **Atomic commits** - Keep commits small and focused. Use `git rebase -i` to clean up history before contributing.

4. **Resolve conflicts early** - Regularly merge `main` into `release` to catch conflicts early.

5. **Tag releases** - Tag your distributable versions:
   ```bash
   git checkout release
   git tag -a v1.0.0-downstream -m "Downstream release v1.0.0"
   git push origin v1.0.0-downstream
   ```

## Conflict Resolution Strategy

When merging `main` into `release` causes conflicts:

1. **Understand the conflict** - Determine if it's due to upstream changes overlapping with your customizations.

2. **Preserve your customizations** - Generally, keep your local modifications while incorporating upstream improvements.

3. **Document decisions** - If you override upstream changes, add a comment explaining why.

4. **Consider contributing** - If your customization is generally useful, consider contributing it upstream to avoid future conflicts.

## Quick Reference

| Task                      | Command                                                      |
| ------------------------- | ------------------------------------------------------------ |
| Sync upstream             | `git fetch upstream && git checkout main && git merge upstream/main --ff-only` |
| Update release            | `git checkout release && git merge main`                     |
| New personal feature      | `git checkout release && git checkout -b local/xxx`          |
| New contributable feature | `git checkout main && git checkout -b feature/xxx`           |
| List all branches         | `git branch -a`                                              |
| View upstream changes     | `git log main..upstream/main`                                |