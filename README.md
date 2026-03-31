# zeroclaw-nix

> **This is NOT the official ZeroClaw repository.** This is a personal Nix packaging fork maintained by [@kcalvelli](https://github.com/kcalvelli). The upstream project is [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) (currently suspended on GitHub).

This repo exists for one reason: **build sovereignty**. When the upstream GitHub account was suspended, our NixOS deployments broke because `nix flake update` returned 404. This fork preserves the full upstream git history (4,141 commits through v0.6.5) and adds Nix packaging on top, so we can build and deploy without depending on the upstream account.

## What this fork adds

- **Nix flake** exporting `packages.{zeroclaw, zeroclaw-web, zeroclaw-desktop}`
- **NixOS module** (`nixosModules.default`) with freeform TOML settings, typed channel submodules, secret file injection via `*File` options, and systemd hardening
- **Feature patches as commits** — XMPP channel, OpenAI-compatible proxy, email improvements, and other customizations that previously lived as `.patch` files in a downstream repo

This fork does **not** modify ZeroClaw's core Rust source beyond the feature patches listed below. It is not a competing project and will sync with upstream if/when the account returns.

## Usage

### Flake input

```nix
{
  inputs.zeroclaw-nix = {
    url = "github:kcalvelli/zeroclaw-nix";
    inputs.nixpkgs.follows = "nixpkgs";
  };
}
```

### Build

```bash
nix build github:kcalvelli/zeroclaw-nix                    # server
nix build github:kcalvelli/zeroclaw-nix#zeroclaw-web       # web frontend
nix build github:kcalvelli/zeroclaw-nix#zeroclaw-desktop   # desktop app (Tauri)
```

### NixOS module

```nix
{
  imports = [ inputs.zeroclaw-nix.nixosModules.default ];

  services.zeroclaw = {
    enable = true;
    package = inputs.zeroclaw-nix.packages.x86_64-linux.zeroclaw;

    # Freeform settings — any ZeroClaw config key works, rendered to TOML
    settings = {
      default_provider = "anthropic";
      default_model = "claude-sonnet-4-6";
      gateway.port = 18789;
      memory.backend = "sqlite";
    };

    # Typed channel config with secret file injection
    channels.telegram = {
      enable = true;
      botTokenFile = "/run/secrets/telegram-bot-token";
    };

    channels.email = {
      enable = true;
      passwordFile = "/run/secrets/email-password";
      imap_host = "mail.example.com";
      smtp_host = "mail.example.com";
      username = "bot@example.com";
      from_address = "bot@example.com";
    };

    channels.xmpp = {
      enable = true;
      passwordFile = "/run/secrets/xmpp-password";
      jid = "bot@example.com";
    };

    # API keys via environment files (compatible with sops-nix, agenix, etc.)
    environmentFiles = [ "/run/secrets/zeroclaw-env" ];
  };
}
```

## Branch structure

| Branch | Purpose |
|--------|---------|
| `main` | Default branch. v0.6.5 tag + nix packaging + feature patches. Point your flake here. |
| `upstream/master` | Read-only mirror of zeroclaw-labs/zeroclaw. Zero fork-specific commits. |

To see exactly what the fork adds: `git log upstream/master..main --oneline`

## Feature patches

These are applied as commits on `main` (not `.patch` files):

| Commit | Description |
|--------|-------------|
| `feat: wire XMPP channel` | Full XMPP channel implementation (xmpp.rs + registry wiring) |
| `feat: OpenAI proxy` | v1/chat/completions endpoint + v1/models (openai_proxy.rs) |
| `feat: full agent loop for webhooks` | Use tool-enabled agent loop instead of simple chat for webhook requests |
| `fix: email reply loop` | Skip emails from own address |
| `feat: email subject threading` | Preserve subject line in reply threading |
| `feat: email sent folder` | Save sent emails to IMAP Sent folder |
| `fix: skip noreply/bounce` | Ignore noreply and bounce addresses |
| `fix: Claude Code permissions` | Skip permission checks in Claude Code CLI provider |
| `feat: SOP provider override` | Allow per-SOP provider/model selection |
| `feat: gateway URL env var` | Runtime ZEROCLAW_GATEWAY_URL for desktop app |
| `feat: broadened CSP` | Tauri CSP allows remote gateway connections |

## Upstream sync

See [SYNC.md](SYNC.md) for the step-by-step workflow to merge new upstream releases.

## License

MIT OR Apache-2.0 (inherited from upstream ZeroClaw).

## Upstream project

ZeroClaw is created by [zeroclaw-labs](https://github.com/zeroclaw-labs). All credit for the core platform goes to them. If you want to use ZeroClaw without Nix-specific packaging, use the upstream repo when it becomes available again.
