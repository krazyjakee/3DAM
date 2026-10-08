import tempfile
import unittest
from pathlib import Path

from generate_updater_manifest import TARGETS, manifest


class UpdaterManifestTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        for target, suffix in TARGETS.values():
            asset = self.directory / f"3dam-v1.2.3-{target}{suffix}"
            asset.write_bytes(b"installer")
            asset.with_name(asset.name + ".sig").write_text("signature\n")

    def test_exact_installer_urls_and_inline_signatures(self):
        result = manifest(self.directory, "krazyjakee/3DAM", "v1.2.3", "New features")
        self.assertEqual(result["version"], "1.2.3")
        self.assertEqual(result["notes"], "New features")
        self.assertEqual(set(result["platforms"]), set(TARGETS))
        for entry in result["platforms"].values():
            self.assertEqual(entry["signature"], "signature")
            self.assertIn("/releases/download/v1.2.3/3dam-v1.2.3-", entry["url"])
        self.assertTrue(result["platforms"]["windows-x86_64-msi"]["url"].endswith(".msi"))
        self.assertTrue(result["platforms"]["windows-x86_64-nsis"]["url"].endswith("-setup.exe"))

    def test_incomplete_release_cannot_publish_a_manifest(self):
        asset = self.directory / "3dam-v1.2.3-x86_64-unknown-linux-gnu.AppImage"
        asset.unlink()
        with self.assertRaises(ValueError):
            manifest(self.directory, "krazyjakee/3DAM", "v1.2.3")

    def test_empty_signature_is_rejected(self):
        signature = next(self.directory.glob("*.sig"))
        signature.write_text("\n")
        with self.assertRaises(ValueError):
            manifest(self.directory, "krazyjakee/3DAM", "v1.2.3")

    def test_manual_build_tag_is_rejected(self):
        with self.assertRaises(ValueError):
            manifest(self.directory, "krazyjakee/3DAM", "dev-abcdef")


if __name__ == "__main__":
    unittest.main()
