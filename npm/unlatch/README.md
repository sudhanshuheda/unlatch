# unlatch

**Unlatch your VM.** Your cloud VM's files show up in macOS Finder and feel as fast as a
local folder. It is built for people who run coding agents (Claude Code, Codex) on remote VMs.

> **Pre-release (0.1.0-alpha.1): Linux only.** What works today from npm: the VM side
> (`npx unlatch share`: daemon install, VM checks, index server) and the Linux FUSE client
> (`npx unlatch connect <user@vm>:<folder> --mount <dir>`) on x86_64 and arm64. **The Mac app is
> not published yet**, so the Mac half of this README describes how it will work; on a Mac,
> `npx unlatch connect` says so and points to
> [building from source](https://github.com/sudhanshuheda/unlatch#building-from-source).

## One command on each side

On the VM, over ssh (or ask your agent to run it):

```sh
npx unlatch
```

This installs the daemon, checks the VM, and prints the one command to run on your Mac
(in this pre-release, it leads with the Linux-desktop command instead, since the Mac app is not
published yet):

```sh
npx unlatch connect you@your-vm:~/code
```

On a cloud VM behind NAT without Tailscale, `share` asks the cloud's metadata service (AWS,
Google Cloud, Azure) for the public address. If it cannot find one, it prints
`<your-ssh-host>` in place of the host and says so: put the address or `~/.ssh/config` alias
you ssh with there.

Once the Mac app ships, that command will install the app, add the VM, and open the folder in
Finder, and `npx unlatch` on the Mac on its own will ask which host from `~/.ssh/config` to use
and which folder to show. Until then, on a Mac these commands only print how to build the app
from source.

## For coding agents

```sh
npx -y unlatch share --json          # on the VM: prints {"mac_command": …, "host_candidates": […], …}
npx unlatch skill                    # teach Claude Code / Codex to do that when you ask
```

`skill` writes `~/.claude/skills/unlatch/SKILL.md` (Claude Code) and
`~/.agents/skills/unlatch/SKILL.md` (Codex). Add `--claude`, `--codex` or `--all` to choose
which. With the skill installed, "show me this folder in Finder" is enough: the agent runs
`share` on the VM and gives you the command for your Mac.

## How it works

- A small Rust daemon on the VM keeps an index of the folder and watches it with inotify. It
  pushes every change over **one ssh connection** as it happens. On a network filesystem
  (NFS, CIFS, …), or for folders beyond its share of the inotify watch limit, it falls back to
  polling every 1–30 s; `share` and `doctor` warn when that applies.
- The Mac keeps a local copy of the whole metadata tree, so Finder never waits on the network.
  A file's contents download the first time you open it. When you look at a folder, its small
  files are fetched ahead of time. Edits save straight back to the VM.
- A File Provider extension makes the VM a real Finder location, so every Mac app works with
  it: Quick Look, dragging into Slack, Preview, ⌘C/⌘V.
- No ports are opened. It uses your existing ssh setup: `~/.ssh/config`, your agent,
  ProxyJump and Tailscale all work. No passwords, passphrases or keys are stored. With key-based
  ssh nothing is asked; if your ssh needs a password, passphrase or 2FA code (or to trust a new
  host key), the Mac app shows ssh's own prompt in a dialog when you connect and passes the
  answer straight to ssh. Background reconnects never prompt. When both sides change a file,
  you get a conflict copy instead of a silent overwrite.
- On Linux desktops, `connect … --mount <dir>` gives you a FUSE mount instead of Finder.

## Commands

| | |
|---|---|
| `unlatch` | VM: share the current folder. Mac (once the app ships): guided setup. |
| `unlatch share [dir] [--json] [--no-serve]` | VM: install or refresh the daemon, run checks, pre-start the index server, print the Mac command |
| `unlatch connect <user@host>:<dir> [--port N] [--identity F] [--name N]` | Mac (once the app ships): install the app if needed, add the VM, wait until it is live, open it in Finder |
| `unlatch connect <user@host>:<dir> --mount <dir>` | Linux desktop: FUSE mount |
| `unlatch status` · `open [name]` · `remove <name>` · `remove --mount <dir>` | |
| `unlatch doctor [dir]` | VM: inotify limits against tree size, filesystem type (NFS gets a warning), disk space, sshd. Mac (once the app ships): app, extension, agent, ssh to each VM |
| `unlatch update` · `uninstall --yes` | Uninstall removes the VMs from Finder first, then the app. Your files on the VM are not touched. |
| `unlatch skill [--claude\|--codex\|--all\|--print]` | Install the agent skill |

Every command takes `--json`. Exit codes: 0 ok, 1 failed, 2 usage, 3 you need to act (for example,
approve the app under Login Items).

## Install notes

- `unlatch` has no runtime dependencies. The binaries come from one platform package that npm
  picks for your machine (`unlatch-linux-x64` or `unlatch-linux-arm64`; the Mac package,
  `unlatch-darwin-universal`, comes with the Mac app). If your npm config skips optional dependencies
  (`--omit=optional`), the installer tells you how to add the package back.
- On the VM, the daemon goes into the first usable directory from this list: `$UNLATCH_HOME`,
  `$XDG_DATA_HOME/unlatch`, `~/.unlatch`, `/var/tmp/unlatch-$UID`, `/tmp/unlatch-$UID`. The file is
  `unlatchd-<version>-<sha16>`. The Mac app uses the same rule, so after `share` it finds the
  daemon already in place and does not upload it again. If you never run `share`, the Mac
  uploads the daemon on first connect instead. That upload is sha256-verified. The Mac looks
  as a non-interactive ssh session does, so set `$UNLATCH_HOME` where such sessions see it.
- `connect` never replaces a newer installed app with an older one (for example from a stale
  npx cache); it keeps the newer app and tells you to run `npx unlatch@latest`.

More: [docs/INSTALL.md](https://github.com/sudhanshuheda/unlatch/blob/dev/docs/INSTALL.md).
The intended home page is unlatch.dev.

License: MIT OR Apache-2.0.
