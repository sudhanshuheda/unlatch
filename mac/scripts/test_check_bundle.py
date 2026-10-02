#!/usr/bin/env python3
"""Tests for check-bundle.py and gen-launchagent.sh, runnable on any OS (no Xcode):
builds a fake Unlatch.app from the real Info.plists/entitlements the way Xcode would (variable
expansion + the post-build scripts), checks it passes, then breaks it in each way CI must catch.

    python3 mac/scripts/test_check_bundle.py
"""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import os
import plistlib
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
MAC = SCRIPTS.parent

spec = importlib.util.spec_from_file_location("check_bundle", SCRIPTS / "check-bundle.py")
check_bundle = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(check_bundle)

TEAM = "ABCDE12345"
PREFIX = "com.example"
GROUP = f"{TEAM}.{PREFIX}.unlatch"


def settings_for(target: str, **extra: str) -> dict[str, str]:
    common = {
        "PROJECT_DIR": str(MAC),
        "UNLATCH_TEAM_ID": TEAM,
        "UNLATCH_BUNDLE_PREFIX": PREFIX,
        "UNLATCH_APP_GROUP": GROUP,
        "MARKETING_VERSION": "0.1.0",
        "CURRENT_PROJECT_VERSION": "1",
        "MACOSX_DEPLOYMENT_TARGET": "13.0",
        "DEVELOPMENT_TEAM": TEAM,
    }
    per = {
        "Unlatch": {
            "PRODUCT_NAME": "Unlatch", "EXECUTABLE_NAME": "Unlatch", "PRODUCT_MODULE_NAME": "Unlatch",
            "PRODUCT_BUNDLE_IDENTIFIER": f"{PREFIX}.unlatch", "PRODUCT_BUNDLE_PACKAGE_TYPE": "APPL",
            "INFOPLIST_FILE": "Unlatch/Info.plist", "CODE_SIGN_ENTITLEMENTS": "Unlatch/Unlatch.entitlements",
        },
        "UnlatchFileProvider": {
            "PRODUCT_NAME": "UnlatchFileProvider", "EXECUTABLE_NAME": "UnlatchFileProvider",
            "PRODUCT_MODULE_NAME": "UnlatchFileProvider",
            "PRODUCT_BUNDLE_IDENTIFIER": f"{PREFIX}.unlatch.fileprovider", "PRODUCT_BUNDLE_PACKAGE_TYPE": "XPC!",
            "INFOPLIST_FILE": "UnlatchFileProvider/Info.plist",
            "CODE_SIGN_ENTITLEMENTS": "UnlatchFileProvider/UnlatchFileProvider.entitlements",
        },
    }[target]
    return {**common, **per, **extra}


def expand_file(src: Path, dst: Path, settings: dict[str, str]) -> None:
    with src.open("rb") as f:
        data = check_bundle.expand(plistlib.load(f), settings)
    dst.parent.mkdir(parents=True, exist_ok=True)
    with dst.open("wb") as f:
        plistlib.dump(data, f)


class BundleTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="unlatch-bundle-"))
        self.addCleanup(shutil.rmtree, self.tmp)
        self.settings_json = self.tmp / "settings.json"
        self.settings = [
            {"target": "Unlatch", "buildSettings": settings_for("Unlatch")},
            {"target": "UnlatchFileProvider", "buildSettings": settings_for("UnlatchFileProvider")},
            {"target": "RustLib", "buildSettings": {}},
        ]
        self.write_settings()
        self.app = self.tmp / "Build" / "Unlatch.app"
        contents = self.app / "Contents"
        expand_file(MAC / "Unlatch/Info.plist", contents / "Info.plist", settings_for("Unlatch"))
        expand_file(MAC / "UnlatchFileProvider/Info.plist",
                    contents / "PlugIns/UnlatchFileProvider.appex/Contents/Info.plist", settings_for("UnlatchFileProvider"))
        for exe in (contents / "MacOS/Unlatch", contents / "MacOS/unlatch-askpass"):
            exe.parent.mkdir(parents=True, exist_ok=True)
            exe.write_bytes(b"\xcf\xfa\xed\xfe")
            exe.chmod(0o755)
        # The real post-build script, with the environment Xcode gives it.
        env = dict(os.environ, TARGET_BUILD_DIR=str(self.app.parent), CONTENTS_FOLDER_PATH="Unlatch.app/Contents",
                   EXECUTABLE_NAME="Unlatch", PRODUCT_BUNDLE_IDENTIFIER=f"{PREFIX}.unlatch",
                   UNLATCH_BUNDLE_PREFIX=PREFIX, UNLATCH_APP_GROUP=GROUP)
        subprocess.run(["bash", str(SCRIPTS / "gen-launchagent.sh")], env=env, check=True, capture_output=True)
        # unlatchd staged by build-rust.sh --stage-bundle from a prebuilt dir.
        staged = self.tmp / "unlatchd-src"
        staged.mkdir()
        for arch in ("x86_64", "aarch64"):
            (staged / f"unlatchd-{arch}").write_bytes(b"\x7fELF" + arch.encode() * 100)
        unlatchd_out = MAC / "build" / "unlatchd"
        self.backup = None
        if unlatchd_out.exists():
            self.backup = self.tmp / "unlatchd-backup"
            shutil.copytree(unlatchd_out, self.backup)
            self.addCleanup(self.restore_unlatchd, unlatchd_out)
        else:
            self.addCleanup(shutil.rmtree, unlatchd_out, True)
        benv = dict(os.environ, UNLATCHD_PREBUILT_DIR=str(staged))
        subprocess.run(["bash", str(SCRIPTS / "build-rust.sh"), "--unlatchd-only"], env=benv, check=True, capture_output=True)
        subprocess.run(["bash", str(SCRIPTS / "build-rust.sh"), "--stage-bundle", str(self.app)], env=benv, check=True,
                       capture_output=True)

    def restore_unlatchd(self, unlatchd_out: Path) -> None:
        shutil.rmtree(unlatchd_out, ignore_errors=True)
        if self.backup:
            shutil.copytree(self.backup, unlatchd_out)

    def write_settings(self) -> None:
        self.settings_json.write_text(json.dumps(self.settings))

    def run_check(self, *extra: str) -> tuple[int, str]:
        proc = subprocess.run(
            ["python3", str(SCRIPTS / "check-bundle.py"), "--settings", str(self.settings_json), "--app", str(self.app),
             "--require-unlatchd", *extra],
            capture_output=True, text=True)
        return proc.returncode, proc.stdout + proc.stderr

    def edit_plist(self, path: Path, fn) -> None:
        with path.open("rb") as f:
            data = plistlib.load(f)
        fn(data)
        with path.open("wb") as f:
            plistlib.dump(data, f)

    def assert_fails(self, needle: str) -> None:
        code, out = self.run_check()
        self.assertEqual(code, 1, out)
        self.assertIn(needle, out)

    # --- tests -------------------------------------------------------------------------------

    def test_consistent_bundle_passes(self) -> None:
        code, out = self.run_check()
        self.assertEqual(code, 0, out)
        self.assertIn(GROUP, out)

    def test_launchagent_contents(self) -> None:
        with (self.app / f"Contents/Library/LaunchAgents/{PREFIX}.unlatch.agent.plist").open("rb") as f:
            agent = plistlib.load(f)
        self.assertEqual(agent["MachServices"], {f"{GROUP}.engine": True})
        self.assertEqual(agent["ProgramArguments"], ["Contents/MacOS/Unlatch", "--agent"])
        self.assertTrue(agent["KeepAlive"])

    def test_launchagent_rejects_bad_identifier(self) -> None:
        env = dict(os.environ, TARGET_BUILD_DIR=str(self.tmp), CONTENTS_FOLDER_PATH="X.app/Contents", EXECUTABLE_NAME="Unlatch",
                   PRODUCT_BUNDLE_IDENTIFIER="a</string>", UNLATCH_BUNDLE_PREFIX=PREFIX, UNLATCH_APP_GROUP=GROUP)
        proc = subprocess.run(["bash", str(SCRIPTS / "gen-launchagent.sh")], env=env, capture_output=True, text=True)
        self.assertNotEqual(proc.returncode, 0)

    def test_document_group_mismatch(self) -> None:
        self.edit_plist(self.app / "Contents/PlugIns/UnlatchFileProvider.appex/Contents/Info.plist",
                        lambda d: d["NSExtension"].__setitem__("NSExtensionFileProviderDocumentGroup", "group.other"))
        self.assert_fails("NSExtensionFileProviderDocumentGroup")

    def test_pipeline_depth(self) -> None:
        self.edit_plist(self.app / "Contents/PlugIns/UnlatchFileProvider.appex/Contents/Info.plist",
                        lambda d: d["NSExtension"].__setitem__("NSExtensionFileProviderDownloadPipelineDepth", 6))
        self.assert_fails("DownloadPipelineDepth")

    def test_app_group_key_mismatch(self) -> None:
        self.edit_plist(self.app / "Contents/Info.plist", lambda d: d.__setitem__("UnlatchAppGroup", "X.other.unlatch"))
        self.assert_fails("app UnlatchAppGroup")

    def test_agent_without_agent_flag(self) -> None:
        self.edit_plist(self.app / f"Contents/Library/LaunchAgents/{PREFIX}.unlatch.agent.plist",
                        lambda d: d.__setitem__("ProgramArguments", ["Contents/MacOS/Unlatch"]))
        self.assert_fails("--agent")

    def test_wrong_mach_service(self) -> None:
        self.edit_plist(self.app / f"Contents/Library/LaunchAgents/{PREFIX}.unlatch.agent.plist",
                        lambda d: d.__setitem__("MachServices", {"com.example.engine": True}))
        self.assert_fails("MachServices")

    def test_tampered_unlatchd(self) -> None:
        (self.app / "Contents/Resources/unlatchd/unlatchd-aarch64").write_bytes(b"\x7fELFother")
        self.assert_fails("does not match")

    def test_missing_askpass(self) -> None:
        (self.app / "Contents/MacOS/unlatch-askpass").unlink()
        self.assert_fails("unlatch-askpass")

    def test_application_identifier_in_agent_entitlements(self) -> None:
        ent = self.tmp / "bad.entitlements"
        with (MAC / "Unlatch/Unlatch.entitlements").open("rb") as f:
            data = plistlib.load(f)
        data["com.apple.application-identifier"] = f"{TEAM}.{PREFIX}.unlatch"
        with ent.open("wb") as f:
            plistlib.dump(data, f)
        self.settings[0]["buildSettings"]["CODE_SIGN_ENTITLEMENTS"] = str(ent)
        self.write_settings()
        self.assert_fails("com.apple.application-identifier")

    def test_group_mismatch_between_targets(self) -> None:
        self.settings[1]["buildSettings"]["UNLATCH_APP_GROUP"] = "Z.other.unlatch"
        self.write_settings()
        self.assert_fails("resolve UNLATCH_APP_GROUP differently")

    def test_expand_entitlements(self) -> None:
        out = self.tmp / "ent"
        proc = subprocess.run(["python3", str(SCRIPTS / "check-bundle.py"), "--settings", str(self.settings_json),
                               "--expand-entitlements", str(out)], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        with (out / "appex.entitlements").open("rb") as f:
            appex = plistlib.load(f)
        self.assertEqual(appex["com.apple.security.application-groups"], [GROUP])
        self.assertIs(appex["com.apple.security.app-sandbox"], True)

    def run_signed(self, app_ent: dict, appex_ent: dict, teams: tuple = (None, None)) -> list[str]:
        """check_signed with codesign replaced by the given embedded entitlements / teams."""
        appex_path = self.app / "Contents/PlugIns/UnlatchFileProvider.appex"
        saved = (check_bundle.signed_entitlements, check_bundle.signing_team)
        check_bundle.signed_entitlements = lambda p: appex_ent if Path(p) == appex_path else app_ent
        check_bundle.signing_team = lambda p: teams[1] if Path(p) == appex_path else teams[0]
        try:
            f = check_bundle.Failures()
            check_bundle.check_signed(self.app, GROUP, f)
            return f.items
        finally:
            check_bundle.signed_entitlements, check_bundle.signing_team = saved

    def expanded(self, name: str) -> dict:
        target = {"app": "Unlatch", "appex": "UnlatchFileProvider"}[name]
        settings = {e["target"]: e["buildSettings"] for e in self.settings}
        return check_bundle.source_entitlements(settings[target])

    def test_signed_ad_hoc_bundle_passes(self) -> None:
        # Ad hoc: no team. The sandboxed appex must pass the appex rules, not the agent's.
        self.assertEqual(self.run_signed(self.expanded("app"), self.expanded("appex")), [])

    def test_signed_with_team_passes_and_checks_prefix(self) -> None:
        self.assertEqual(self.run_signed(self.expanded("app"), self.expanded("appex"), (TEAM, TEAM)), [])
        items = self.run_signed(self.expanded("app"), self.expanded("appex"), ("OTHERTEAM1", "OTHERTEAM1"))
        self.assertTrue(any("not prefixed with the signing team" in i for i in items), items)

    def test_signed_unsandboxed_appex_or_sandboxed_agent_fail(self) -> None:
        appex = self.expanded("appex")
        appex.pop("com.apple.security.app-sandbox")
        items = self.run_signed(self.expanded("app"), appex)
        self.assertTrue(any("signed appex: must be sandboxed" in i for i in items), items)
        app = self.expanded("app")
        app["com.apple.security.app-sandbox"] = True
        items = self.run_signed(app, self.expanded("appex"))
        self.assertTrue(any("signed app/agent: must not be sandboxed" in i for i in items), items)

    def test_signed_by_different_teams(self) -> None:
        items = self.run_signed(self.expanded("app"), self.expanded("appex"), (TEAM, "OTHERTEAM1"))
        self.assertTrue(any("different teams" in i for i in items), items)

    def test_unlatchd_checksums_written_by_build_script(self) -> None:
        d = self.app / "Contents/Resources/unlatchd"
        for arch in ("x86_64", "aarch64"):
            want = (d / f"unlatchd-{arch}.sha256").read_text().split()[0]
            self.assertEqual(want, hashlib.sha256((d / f"unlatchd-{arch}").read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main(verbosity=2)
