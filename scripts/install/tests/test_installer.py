"""Offline bootstrap checks with real archives and isolated child environments."""

import hashlib
import json
import os
from pathlib import Path
import platform
import stat
import subprocess
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[3]


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.scratch = Path(self.temporary.name)
        self.home = self.scratch / "home"
        self.home.mkdir()
        archive = self.scratch / "ck.zip"
        with zipfile.ZipFile(archive, "w") as fixture:
            # Bootstrap must not execute this candidate.
            fixture.writestr("ck", "#!/bin/sh\nexit 99\n")
        target = {"Darwin": "darwin", "Linux": "linux"}[platform.system()]
        arch = "arm64" if platform.machine() in ("arm64", "aarch64") else "x64"
        index = self.scratch / "index.json"
        index.write_text(json.dumps({"components": {"core": {"assets": {
            f"{target}-{arch}": {"ck": {
                "url": archive.as_uri(),
                "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
            }}
        }}}}))
        self.env = dict(os.environ, HOME=str(self.home), SHELL="/bin/zsh",
                        XDG_DATA_HOME="", XDG_CONFIG_HOME=str(self.scratch / "config"),
                        XDG_RUNTIME_DIR=str(self.scratch / "runtime"),
                        CK_RELEASE_INDEX_URL=index.as_uri(), WSL_DISTRO_NAME="")
        self.env.pop("CK_PROFILE_PATH", None)

    def install(self):
        result = subprocess.run(["bash", str(ROOT / "scripts/install/install.sh")],
                                env=self.env, cwd=self.scratch,
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_profile_update_preserves_symlink_target_and_permissions(self):
        dotfiles = self.home / "dotfiles"
        dotfiles.mkdir()
        target = dotfiles / "zshrc"
        target.write_text("# custom dotfiles\n")
        target.chmod(0o640)
        profile = self.home / ".zshrc"
        # Two relative links exercise target resolution, not just absolute links.
        (dotfiles / "current").symlink_to("zshrc")
        profile.symlink_to("dotfiles/current")
        self.install()
        self.assertTrue(profile.is_symlink(), "installer replaced the profile symlink")
        self.assertEqual(os.readlink(profile), "dotfiles/current")
        self.assertIn("# cortexkit-managed PATH begin", target.read_text())
        self.assertIn("# custom dotfiles", target.read_text())
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o640)
        self.install()
        self.assertTrue(profile.is_symlink())
        self.assertEqual(target.read_text().count("# cortexkit-managed PATH begin"), 1)
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o640)

        profile.unlink()
        profile.write_text("# regular profile\n")
        profile.chmod(0o644)
        self.install()
        self.assertEqual(stat.S_IMODE(profile.stat().st_mode), 0o644)

    def test_bootstrap_uses_xdg_data_home_for_inventory_and_shell_path(self):
        data = self.scratch / 'data with $literal "quotes" `ticks`\\backslash'
        self.env["XDG_DATA_HOME"] = str(data)
        self.install()
        binary = data / "cortexkit/bin/ck"
        manifest = data / "cortexkit/installer-manifest.json"
        self.assertTrue(binary.is_file(), "binary was not placed in XDG_DATA_HOME")
        inventory = json.loads(manifest.read_text())
        self.assertEqual(inventory["mutations"][0]["path"], str(binary))
        self.assertEqual(inventory["mutations"][2]["path"], str(manifest))
        self.assertFalse((self.home / ".local/share/cortexkit").exists())
        result = subprocess.run(["bash", "-c", '. "$HOME/.zshrc"; printf "%s" "$PATH"'],
                                env=dict(self.env, PATH="/usr/bin:/bin"), cwd=self.scratch,
                                text=True, capture_output=True, check=True)
        self.assertEqual(result.stdout, str(binary.parent) + ":/usr/bin:/bin")
        # Reinstall must put its sidecar beside the XDG inventory as well.
        self.install()
        self.assertTrue((data / "cortexkit/installer-manifest.bootstrap.json").is_file())
        self.env["SHELL"] = "/usr/bin/fish"
        self.install()
        fish_profile = Path(self.env["XDG_CONFIG_HOME"]) / "fish/config.fish"
        result = subprocess.run(["fish", "--no-config", "-c",
                                 'source "$XDG_CONFIG_HOME/fish/config.fish"; printf "%s" $PATH[1]'],
                                env=self.env, cwd=self.scratch,
                                text=True, capture_output=True, check=True)
        self.assertTrue(fish_profile.is_file())
        self.assertEqual(result.stdout, str(binary.parent))


if __name__ == "__main__":
    unittest.main()
