#!/usr/bin/env python3
"""Assert that a built Unlatch.app is wired together consistently (review D20, §2(f)9).

The File Provider integration only works when the app (which is also the launchd engine
agent), the extension, the LaunchAgent plist and the Info.plists all agree on one team-prefixed
app group, and a mismatch shows up on a user's Mac only as "The application cannot be used right
now" (FP -2001). So CI checks it:

  * source entitlements (expanded with Xcode's build settings): app and appex name exactly the
    app group; the appex is sandboxed; the app/agent has no com.apple.application-identifier
    (MQ-066) and no keychain-access-groups (MQ-065);
  * built Info.plists: UnlatchAppGroup / UnlatchTeamID / UnlatchBundlePrefix,
    NSExtensionFileProviderDocumentGroup == the group, pipeline depths 16/4,
    NSLocalNetworkUsageDescription, bundle ids nested under the app's;
  * the LaunchAgent plist: MachServices == {<group>.engine}, KeepAlive, --agent,
    BundleProgram pointing at the app executable;
  * unlatch-askpass and (with --require-unlatchd) unlatchd + matching .sha256 in the bundle;
  * with --signed: the entitlements actually embedded by codesign say the same, and the group is
    prefixed with the signing team when the signature has one.

Usage:
  check-bundle.py --settings settings.json --app path/to/Unlatch.app [--require-unlatchd] [--signed]
  check-bundle.py --settings settings.json --expand-entitlements OUTDIR
where settings.json is `xcodebuild -showBuildSettings -json` for the Unlatch scheme.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import re
import subprocess
import sys
from pathlib import Path

APP_TARGET = "Unlatch"
APPEX_TARGET = "UnlatchFileProvider"
ASKPASS_TARGET = "unlatch-askpass"
FORBIDDEN_AGENT_KEYS = (
    "com.apple.application-identifier",
    "com.apple.developer.team-identifier",
    "keychain-access-groups",
)
VAR = re.compile(r"\$\(([A-Za-z0-9_]+)\)|\$\{([A-Za-z0-9_]+)\}")


class Failures:
    def __init__(self) -> None:
        self.items: list[str] = []

    def check(self, ok: bool, message: str) -> bool:
        if not ok:
            self.items.append(message)
        return ok


def load_settings(path: Path) -> dict[str, dict[str, str]]:
    data = json.loads(path.read_text())
    out: dict[str, dict[str, str]] = {}
    for entry in data:
        target = entry.get("target")
        if target:
            out[target] = entry.get("buildSettings", {})
    for t in (APP_TARGET, APPEX_TARGET):
        if t not in out:
            raise SystemExit(f"check-bundle: no build settings for target {t} in {path}")
    return out


def expand(value, settings: dict[str, str]):
    """Expand $(VAR) / ${VAR} recursively in strings inside a plist value."""
    if isinstance(value, str):
        for _ in range(10):
            new = VAR.sub(lambda m: settings.get(m.group(1) or m.group(2), ""), value)
            if new == value:
                break
            value = new
        return value
    if isinstance(value, list):
        return [expand(v, settings) for v in value]
    if isinstance(value, dict):
        return {expand(k, settings): expand(v, settings) for k, v in value.items()}
    return value


def source_entitlements(settings: dict[str, str]) -> dict:
    rel = settings.get("CODE_SIGN_ENTITLEMENTS")
    if not rel:
        raise SystemExit("check-bundle: CODE_SIGN_ENTITLEMENTS not set")
    path = Path(rel)
    if not path.is_absolute():
        path = Path(settings.get("PROJECT_DIR", ".")) / rel
    with path.open("rb") as f:
        return expand(plistlib.load(f), settings)


def read_plist(path: Path, f: Failures):
    if not f.check(path.is_file(), f"missing {path}"):
        return None
    with path.open("rb") as fh:
        return plistlib.load(fh)


def check_entitlements(kind: str, ent: dict, group: str, f: Failures, team: str | None = None, *, appex: bool) -> None:
    """`kind` only labels messages; `appex` picks the rules (extension vs app/agent)."""
    groups = ent.get("com.apple.security.application-groups")
    f.check(groups == [group], f"{kind}: application-groups is {groups!r}, expected [{group!r}]")
    if team:
        f.check(group.startswith(team + "."), f"{kind}: app group {group!r} is not prefixed with the signing team {team!r}")
    if appex:
        f.check(ent.get("com.apple.security.app-sandbox") is True, f"{kind}: must be sandboxed (com.apple.security.app-sandbox)")
    else:
        for key in FORBIDDEN_AGENT_KEYS:
            f.check(key not in ent, f"{kind}: must not carry {key} (MQ-066/MQ-065: launchd/AMFI refuse the agent)")
        f.check(not ent.get("com.apple.security.app-sandbox", False), f"{kind}: must not be sandboxed (it runs ssh and reads ~/.ssh)")


def check_sources(settings: dict[str, dict[str, str]], f: Failures) -> str:
    app = settings[APP_TARGET]
    appex = settings[APPEX_TARGET]
    group = app.get("UNLATCH_APP_GROUP", "")
    team = app.get("UNLATCH_TEAM_ID", "")
    prefix = app.get("UNLATCH_BUNDLE_PREFIX", "")
    f.check(bool(group) and "$(" not in group, f"UNLATCH_APP_GROUP not resolved: {group!r}")
    f.check(group == f"{team}.{prefix}.unlatch", f"UNLATCH_APP_GROUP {group!r} != $(UNLATCH_TEAM_ID).$(UNLATCH_BUNDLE_PREFIX).unlatch")
    f.check(appex.get("UNLATCH_APP_GROUP") == group, "app and appex resolve UNLATCH_APP_GROUP differently")
    f.check(app.get("PRODUCT_BUNDLE_IDENTIFIER") == f"{prefix}.unlatch", "app bundle id must be $(UNLATCH_BUNDLE_PREFIX).unlatch")
    f.check(appex.get("PRODUCT_BUNDLE_IDENTIFIER") == f"{prefix}.unlatch.fileprovider", "appex bundle id must be $(UNLATCH_BUNDLE_PREFIX).unlatch.fileprovider")
    check_entitlements("app/agent", source_entitlements(app), group, f, appex=False)
    check_entitlements("appex", source_entitlements(appex), group, f, appex=True)
    return group


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def check_bundle(app_path: Path, settings: dict[str, dict[str, str]], group: str, require_unlatchd: bool, f: Failures) -> None:
    app = settings[APP_TARGET]
    prefix = app.get("UNLATCH_BUNDLE_PREFIX", "")
    team = app.get("UNLATCH_TEAM_ID", "")
    exe = app.get("EXECUTABLE_NAME", "Unlatch")
    contents = app_path / "Contents"

    info = read_plist(contents / "Info.plist", f)
    agent_plist_name = f"{prefix}.unlatch.agent.plist"
    if info is not None:
        f.check(info.get("CFBundleIdentifier") == f"{prefix}.unlatch", f"app CFBundleIdentifier {info.get('CFBundleIdentifier')!r}")
        f.check(info.get("UnlatchAppGroup") == group, f"app UnlatchAppGroup {info.get('UnlatchAppGroup')!r} != {group!r}")
        f.check(info.get("UnlatchTeamID") == team, "app UnlatchTeamID mismatch")
        f.check(info.get("UnlatchBundlePrefix") == prefix, "app UnlatchBundlePrefix mismatch")
        f.check(info.get("UnlatchAgentPlist") == agent_plist_name, f"app UnlatchAgentPlist {info.get('UnlatchAgentPlist')!r} != {agent_plist_name!r}")
        f.check(bool(info.get("NSLocalNetworkUsageDescription")), "app: NSLocalNetworkUsageDescription missing (MQ-069)")
        f.check(info.get("LSUIElement") is True, "app: LSUIElement must be true (menu-bar app)")
        f.check((contents / "MacOS" / exe).is_file(), f"app executable Contents/MacOS/{exe} missing")

    appex_dir = contents / "PlugIns" / "UnlatchFileProvider.appex"
    ainfo = read_plist(appex_dir / "Contents" / "Info.plist", f)
    if ainfo is not None:
        ext = ainfo.get("NSExtension", {})
        f.check(ainfo.get("CFBundleIdentifier") == f"{prefix}.unlatch.fileprovider", "appex CFBundleIdentifier mismatch")
        f.check(ainfo.get("UnlatchAppGroup") == group, f"appex UnlatchAppGroup {ainfo.get('UnlatchAppGroup')!r} != {group!r}")
        f.check(ainfo.get("UnlatchTeamID") == team and ainfo.get("UnlatchBundlePrefix") == prefix, "appex Unlatch* identity keys mismatch")
        f.check(ext.get("NSExtensionPointIdentifier") == "com.apple.fileprovider-nonui", "appex: wrong NSExtensionPointIdentifier")
        f.check(str(ext.get("NSExtensionPrincipalClass", "")).endswith(".FileProviderExtension"), "appex: NSExtensionPrincipalClass")
        f.check(ext.get("NSExtensionFileProviderDocumentGroup") == group,
                f"appex: NSExtensionFileProviderDocumentGroup {ext.get('NSExtensionFileProviderDocumentGroup')!r} != {group!r}")
        f.check(ext.get("NSExtensionFileProviderDownloadPipelineDepth") == 16, "appex: DownloadPipelineDepth must be 16")
        f.check(ext.get("NSExtensionFileProviderUploadPipelineDepth") == 4, "appex: UploadPipelineDepth must be 4")
        f.check(ext.get("NSExtensionFileProviderSupportsEnumeration") is True, "appex: NSExtensionFileProviderSupportsEnumeration")

    agent = read_plist(contents / "Library" / "LaunchAgents" / agent_plist_name, f)
    if agent is not None:
        f.check(agent.get("Label") == f"{prefix}.unlatch.agent", f"LaunchAgent Label {agent.get('Label')!r}")
        f.check(agent.get("MachServices") == {f"{group}.engine": True},
                f"LaunchAgent MachServices {agent.get('MachServices')!r} != {{{group}.engine: true}}")
        f.check(agent.get("KeepAlive") is True, "LaunchAgent: KeepAlive must be true")
        prog = agent.get("BundleProgram", "")
        f.check(prog == f"Contents/MacOS/{exe}", f"LaunchAgent BundleProgram {prog!r}")
        f.check((app_path / prog).is_file() if prog else False, "LaunchAgent BundleProgram does not exist in the bundle")
        f.check("--agent" in agent.get("ProgramArguments", []), "LaunchAgent ProgramArguments lacks --agent")
        f.check(f"{prefix}.unlatch" in agent.get("AssociatedBundleIdentifiers", []), "LaunchAgent AssociatedBundleIdentifiers")

    askpass = contents / "MacOS" / "unlatch-askpass"
    f.check(askpass.is_file() and os.access(askpass, os.X_OK), "Contents/MacOS/unlatch-askpass missing or not executable")

    unlatchd_dir = contents / "Resources" / "unlatchd"
    if require_unlatchd:
        for arch in ("x86_64", "aarch64"):
            binary = unlatchd_dir / f"unlatchd-{arch}"
            sumfile = unlatchd_dir / f"unlatchd-{arch}.sha256"
            if f.check(binary.is_file() and sumfile.is_file(), f"Resources/unlatchd/unlatchd-{arch}(.sha256) missing"):
                want = sumfile.read_text().split()[0] if sumfile.read_text().split() else ""
                f.check(want == sha256(binary), f"unlatchd-{arch}: .sha256 does not match the binary")
                f.check(binary.read_bytes()[:4] == b"\x7fELF", f"unlatchd-{arch} is not an ELF binary")


def signed_entitlements(path: Path) -> dict:
    out = subprocess.run(["codesign", "-d", "--entitlements", "-", "--xml", str(path)], capture_output=True, check=True)
    return plistlib.loads(out.stdout) if out.stdout.strip() else {}


def signing_team(path: Path) -> str | None:
    out = subprocess.run(["codesign", "-dv", str(path)], capture_output=True, text=True, check=True)
    for line in out.stderr.splitlines():
        if line.startswith("TeamIdentifier="):
            team = line.split("=", 1)[1].strip()
            return None if team in ("", "not set") else team
    return None


def check_signed(app_path: Path, group: str, f: Failures) -> None:
    appex = app_path / "Contents" / "PlugIns" / "UnlatchFileProvider.appex"
    team = signing_team(app_path)
    check_entitlements("signed app/agent", signed_entitlements(app_path), group, f, team, appex=False)
    check_entitlements("signed appex", signed_entitlements(appex), group, f, team, appex=True)
    f.check(signing_team(appex) == team, "app and appex are signed by different teams")


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--settings", type=Path, required=True)
    ap.add_argument("--app", type=Path)
    ap.add_argument("--require-unlatchd", action="store_true")
    ap.add_argument("--signed", action="store_true")
    ap.add_argument("--expand-entitlements", type=Path, metavar="OUTDIR",
                    help="write the expanded app.entitlements / appex.entitlements for manual codesigning")
    args = ap.parse_args(argv)

    settings = load_settings(args.settings)
    f = Failures()
    group = check_sources(settings, f)

    if args.expand_entitlements:
        args.expand_entitlements.mkdir(parents=True, exist_ok=True)
        for name, target in (("app", APP_TARGET), ("appex", APPEX_TARGET)):
            with (args.expand_entitlements / f"{name}.entitlements").open("wb") as fh:
                plistlib.dump(source_entitlements(settings[target]), fh)

    if args.app:
        check_bundle(args.app, settings, group, args.require_unlatchd, f)
        if args.signed:
            check_signed(args.app, group, f)

    if f.items:
        for item in f.items:
            print(f"::error::check-bundle: {item}")
        return 1
    print(f"check-bundle: OK (app group {group})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
