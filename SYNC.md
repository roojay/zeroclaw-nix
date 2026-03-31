# Upstream Sync Workflow

This fork tracks [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) via the `upstream` remote. When upstream releases a new version, follow these steps to sync.

## Branch Structure

- **`upstream/master`** — Read-only mirror of zeroclaw-labs. Zero fork-specific commits.
- **`main`** — Default branch. Release tag + nix packaging + feature patches. This is what downstream consumers point at.

## Sync Steps

### 1. Fetch upstream

```bash
git fetch upstream
git fetch upstream --tags
```

If upstream is unreachable (account suspended), this will fail — the fork continues to work without it.

### 2. Update upstream/master

```bash
git checkout upstream/master
git merge --ff-only upstream/master
git push origin upstream/master
```

### 3. Merge new tag into main

```bash
git checkout main
git merge v0.6.6
```

Nix files (`flake.nix`, `nix/*.nix`) won't conflict — upstream doesn't have them. Feature patches may conflict on files they touch. Resolve inline and note adjustments in the merge commit.

### 4. Check for absorbed patches

If upstream absorbed a feature we carry (e.g., email reply-threading), the merge will conflict on that code. Resolve in favor of upstream's implementation and note in the commit:

```
Merged v0.6.6. Absorbed upstream: email reply-threading (removed fork commit ecd17446).
```

### 5. Update Nix hashes

The Cargo.lock likely changed. Update the cargo hash:

```bash
# Build will fail with hash mismatch — copy the expected hash from the error
nix build .#zeroclaw 2>&1 | grep 'got:'
# Update cargoHash in nix/package.nix
```

If npm dependencies changed:

```bash
nix build .#zeroclaw-web 2>&1 | grep 'got:'
# Update npmDepsHash in nix/web.nix
```

### 6. Build and test

```bash
nix build .#zeroclaw
nix build .#zeroclaw-web
nix build .#zeroclaw-desktop
```

### 7. Push

```bash
git push origin main --tags
```

### 8. Update downstream (sid repo)

```bash
cd ~/Projects/sid
nix flake lock --update-input zeroclaw-nix
# Build and deploy
```

## Inspecting fork-specific commits

```bash
git log upstream/master..main --oneline
```

This shows exactly what the fork adds on top of upstream.
