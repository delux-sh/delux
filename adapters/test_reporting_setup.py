"""Consumer-visible setup regressions; all files/process metadata are isolated."""

import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import os
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("reporting_setup", Path(__file__).with_name("reporting_setup.py"))
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)
PAYLOAD = setup.MARKER.decode() + "\nexport default function reporter() {}\n"
PAYLOADS = {"pi": PAYLOAD, "opencode": PAYLOAD}


class ReportingSetupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.home = Path(self.directory.name).resolve()
        self.environment = {"HOME": str(self.home), "OMP_PROFILE": "work"}
        self.path = self.home / ".omp/profiles/work/agent/extensions/delux-agent-status.ts"
        self.version = "3.8"
        self.native_status = ""
        self.adapter_status = ""
        self.started = "100"
        self.agent = "omp"

    def command(self, args, env=None):
        if args[:2] == ["tmux", "list-panes"]:
            return "\t".join(["$0", "12", "%0", "10", "9", "11", self.version,
                              self.native_status, self.adapter_status]) + "\n"
        if args[:2] == ["tmux", "display-message"]:
            return "review\n"
        if args[:2] == ["ps", "-eo"]:
            return "10 1 sh\n20 10 " + self.agent + "\n"
        raise AssertionError(args)

    def process(self, pid):
        if pid == 10:
            return ["/bin/sh"], {"HOME": str(self.home)}, "99", str(self.home)
        return [self.agent], self.environment.copy(), self.started, str(self.home)

    def inspect(self):
        with patch.object(setup, "run", side_effect=self.command), patch.object(setup, "process_data", side_effect=self.process):
            return setup.inspect(PAYLOADS)

    def install(self, token):
        with patch.object(setup, "run", side_effect=self.command), patch.object(setup, "process_data", side_effect=self.process):
            return setup.install(PAYLOADS, token)

    def test_named_profile_install_is_read_only_until_explicit_action(self):
        report = self.inspect()
        target = report["targets"][0]
        self.assertFalse(self.path.exists())
        self.assertEqual((report["native"], target["profile"], target["state"]), (False, "work", "missing"))
        self.assertEqual(target["files"], [str(self.path)])
        self.assertEqual(target["action"], "install")
        receipt = self.install(target["token"])
        self.assertEqual(receipt["files"], [str(self.path)])
        self.assertEqual(self.path.read_text(), PAYLOAD)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        installed = self.inspect()["targets"][0]
        self.assertEqual(installed["state"], "installed")
        self.assertIsNone(installed["token"])
        self.assertIsNone(installed["action"])

    def test_only_observed_status_confirms_reporting(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_text(PAYLOAD)
        self.assertEqual(self.inspect()["targets"][0]["state"], "installed")
        self.adapter_status = "waiting 100"
        self.assertEqual(self.inspect()["targets"][0]["state"], "reporting")
        self.native_status = "idle"
        self.assertEqual(self.inspect()["targets"][0]["state"], "installed")
        self.native_status = "blocked"
        self.assertEqual(self.inspect()["targets"][0]["state"], "reporting")

    def test_native_capability_without_status_is_not_confirmation(self):
        self.version = "3.9"
        target = self.inspect()["targets"][0]
        self.assertEqual(target["state"], "native")
        self.assertIsNone(target["action"])
        self.assertFalse(self.path.exists())
        self.version = "3.8-custom"
        self.assertIsNone(self.inspect()["native"])

    def test_shell_without_status_is_not_told_to_install(self):
        self.agent = "sh"
        target = self.inspect()["targets"][0]
        self.assertEqual(target["state"], "unknown")
        self.assertIsNone(target["action"])
        self.assertEqual(target["files"], [])

    def test_process_restart_invalidates_confirmation(self):
        target = self.inspect()["targets"][0]
        self.started = "101"
        replacement = self.inspect()["targets"][0]
        self.assertNotEqual(target["id"], replacement["id"])
        with self.assertRaisesRegex(setup.SetupError, "changed since inspection"):
            self.install(target["token"])
        self.assertFalse(self.path.exists())

    def test_profile_change_invalidates_confirmation(self):
        token = self.inspect()["targets"][0]["token"]
        self.environment["OMP_PROFILE"] = "personal"
        with self.assertRaisesRegex(setup.SetupError, "changed since inspection"):
            self.install(token)
        self.assertFalse((self.home / ".omp").exists())

    def test_conflicting_file_after_inspection_is_not_overwritten(self):
        token = self.inspect()["targets"][0]["token"]
        self.path.parent.mkdir(parents=True)
        self.path.write_text("user-owned extension\n")
        with self.assertRaisesRegex(setup.SetupError, "changed since inspection"):
            self.install(token)
        self.assertEqual(self.path.read_text(), "user-owned extension\n")
        self.assertEqual(self.inspect()["targets"][0]["state"], "conflict")

    def test_changed_owned_reporter_is_not_overwritten(self):
        self.path.parent.mkdir(parents=True)
        old = setup.MARKER + b"\nold reporter\n"
        self.path.write_bytes(old)
        target = self.inspect()["targets"][0]
        self.assertEqual(target["action"], "update")
        self.path.write_bytes(old + b"concurrent edit\n")
        with self.assertRaises(setup.SetupError):
            self.install(target["token"])
        self.assertEqual(self.path.read_bytes(), old + b"concurrent edit\n")

    def test_owned_reporter_updates_without_leftover_temporary_files(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_bytes(setup.MARKER + b"\nold reporter\n")
        self.install(self.inspect()["targets"][0]["token"])
        self.assertEqual(self.path.read_text(), PAYLOAD)
        self.assertEqual(list(self.path.parent.iterdir()), [self.path])

    def test_symlink_parent_is_refused(self):
        outside = self.home / "outside"
        outside.mkdir()
        (self.home / ".omp").symlink_to(outside, target_is_directory=True)
        target = self.inspect()["targets"][0]
        self.assertEqual(target["state"], "conflict")
        self.assertIsNone(target["token"])
        self.assertEqual(list(outside.iterdir()), [])

    def test_hardlinked_reporter_is_refused(self):
        self.path.parent.mkdir(parents=True)
        original = self.home / "original"
        original.write_text(PAYLOAD)
        os.link(original, self.path)
        target = self.inspect()["targets"][0]
        self.assertEqual(target["state"], "conflict")
        self.assertIsNone(target["action"])
        self.assertEqual(original.read_text(), PAYLOAD)

    def test_invalid_token_cannot_choose_an_arbitrary_path(self):
        forged = base64.urlsafe_b64encode(json.dumps({"path": str(self.home / "victim")}).encode()).decode()
        with self.assertRaises(setup.SetupError):
            self.install(forged)
        self.assertFalse((self.home / "victim").exists())

    def test_profile_precedence_and_default_override(self):
        cases = [
            ({"OMP_PROFILE": "work", "PI_CODING_AGENT_DIR": "/ignored"}, ".omp/profiles/work/agent", "work"),
            ({"OMP_PROFILE": "", "PI_PROFILE": "ignored"}, ".omp/agent", None),
            ({"OMP_PROFILE": " default ", "PI_PROFILE": "ignored"}, ".omp/agent", None),
            ({"PI_PROFILE": "legacy"}, ".omp/profiles/legacy/agent", "legacy"),
            ({"OMP_PROFILE": "work", "PI_CONFIG_DIR": "/custom"}, "custom/profiles/work/agent", "work"),
            ({"OMP_PROFILE": "default", "PI_CODING_AGENT_DIR": str(self.home / "override")}, "override", None),
            ({"OMP_PROFILE": "", "PI_PROFILE": "old", "PI_CODING_AGENT_DIR": str(self.home / ".omp/profiles/old/agent")}, ".omp/agent", None),
        ]
        for values, directory, profile in cases:
            with self.subTest(values=values):
                env = {"HOME": str(self.home), **values}
                path, actual_profile, _ = setup.target_path("omp", ["omp"], env, str(self.home))
                self.assertEqual(path, str(self.home / directory / "extensions/delux-agent-status.ts"))
                self.assertEqual(actual_profile, profile)

    def test_cli_profile_overrides_environment(self):
        path, profile, _ = setup.target_path("omp", ["omp", "--profile", "cli"], self.environment, str(self.home))
        self.assertEqual(profile, "cli")
        self.assertIn("/profiles/cli/", path)

    def test_invalid_profile_and_disabled_extensions_are_not_installable(self):
        for value in ("../other", "Bad", "con", "work.", "a" * 65):
            with self.subTest(profile=value), self.assertRaises(setup.SetupError):
                setup.target_path("omp", ["omp"], {"HOME": str(self.home), "OMP_PROFILE": value}, str(self.home))
        with self.assertRaisesRegex(setup.SetupError, "discovery is disabled"):
            setup.target_path("omp", ["omp", "--no-extensions"], self.environment, str(self.home))

    def test_descriptor_write_rechecks_confirmation_digest(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_bytes(setup.MARKER + b"\nuser changed this\n")
        with self.assertRaisesRegex(setup.SetupError, "changed after inspection"):
            setup.install_adapter(str(self.path), PAYLOAD.encode(), hashlib.sha256(b"previous").hexdigest())
        self.assertIn(b"user changed this", self.path.read_bytes())


if __name__ == "__main__":
    unittest.main()
