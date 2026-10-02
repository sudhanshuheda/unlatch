---
name: unlatch
description: Show a folder on this Linux VM in the user's macOS Finder (or on a Linux desktop) with Unlatch. Use when the user asks to browse, open, preview or drag this machine's files on their own computer, for example "set up Unlatch", "show this folder in Finder", "I want to see these files on my Mac". Do not use for copying single files into the chat.
---

# Unlatch: put this VM's folder in the user's Finder

Unlatch shows a folder from this Linux VM in the user's macOS Finder over the ssh connection they
already use. A small daemon on the VM pushes every change, so files you write show up on the Mac
almost at once. No ports are opened. Source: https://github.com/sudhanshuheda/unlatch (open source)

## Rules

- Never ask the user for a password, private key or token. Unlatch uses their own ssh setup
  (`~/.ssh/config`, ssh agent, ProxyJump, Tailscale). If their ssh needs a password or 2FA code,
  the Unlatch app on their Mac shows ssh's own prompt; that is between them and ssh.
- Never open ports, edit firewall rules or change sshd settings.
- Never run the Mac command yourself on the VM. It is for the user's Mac.

## Steps (you are on the VM)

1. Check Node: `node --version` and `npx --version`. If either is missing, tell the user Unlatch
   needs Node 18 or later for `npx`, and stop.
2. Pick the folder. Use the one the user named. If they did not name one, ask which folder to
   share, and suggest the current project directory.
3. In that folder, run: `npx -y unlatch share --json`
   (or `npx -y unlatch share --json <folder>`).
4. Read the JSON on stdout. First check `mac_app_published`:
   - If it is `false`, this is a pre-release without the Mac app, so `mac_command` does not
     work on a Mac yet. Do not give it as the thing to run. Tell the user, in one line, that
     the Mac app is not published yet. Give `linux_command` (for a Linux desktop) in a code
     block, and say a Mac needs the app built from source (the link is in `warnings[]`). Skip
     step 5.
   - Otherwise, go on as below.

   - `mac_command`: the one line the user runs in Terminal on their Mac. Give it to them
     verbatim, in a code block, and tell them to paste it into Terminal on their Mac.
   - `host_guess`: if `true`, the VM could not tell which address the Mac reaches it by (for
     example a cloud VM behind NAT). Relay `host_hint` with the command. If `mac_command`
     contains `<your-ssh-host>`, ask the user what they type after `ssh` to reach this VM (a
     `~/.ssh/config` alias or an address) and give them the command with that filled in.
   - `host_hint`: when present and `host_guess` is `false`, add it in one line.
   - `host_candidates[]`: if there is more than one, add that the other `mac_command` values
     work if the Mac cannot reach the first host name. Each entry has a `note`.
   - `linux_command`: the same for a Linux desktop (a FUSE mount instead of Finder).
   - `warnings[]`: pass them on in one line each. Each one contains its fix. (When
     `mac_app_published` is `false`, the Mac-app warning is already covered by step 4.)
5. Only when `mac_app_published` is `true`, tell the user what happens next: the first run on the Mac installs the Unlatch app and may
   ask them to allow it in System Settings → Login Items. Then the VM appears in Finder under
   Locations, and files open on demand.

Keep the report short: the command, where to paste it, what they will see.

## If something fails

Run `npx -y unlatch doctor --json` in the same folder and relay each failed check with its fix.
Common causes: the inotify watch limit is too low, the folder is on a network filesystem,
or the Mac cannot reach this VM's host name (use another `host_candidates` entry, or the
user's own `~/.ssh/config` alias).

## If you are on the user's Mac instead

Run `npx -y unlatch connect <user@host>:<folder> --json` with the VM the user named. When the
Mac app is published, it installs the app on first use and opens the folder in Finder. If it
exits with code 3, the user has to act (for example approve the app in System Settings → Login
Items, or answer an ssh dialog); tell them what the `error` field says. If the JSON `code` is
`not_published`, this release has no Mac app yet: tell the user that, and pass on the
build-from-source link from `error`.

## Other commands

`npx -y unlatch status`, `npx -y unlatch doctor [folder]`, `npx -y unlatch remove <name>`.
Add `--json` to any command to get machine-readable output.
