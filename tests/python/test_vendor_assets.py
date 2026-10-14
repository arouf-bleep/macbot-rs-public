"""The delivery gate must notice modified or unreviewed browser assets."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from check_project_files import check_vendor_assets


class VendorAssetTests(unittest.TestCase):
    def test_changed_missing_and_added_assets_are_blocking(self):
        with tempfile.TemporaryDirectory() as directory:
            vendor = Path(directory)
            asset = vendor / "library.js"
            data = b"reviewed bytes"
            asset.write_bytes(data)
            (vendor / "asset-manifest.json").write_text(json.dumps({"files": [
                {"path": "library.js", "bytes": len(data),
                 "sha256": hashlib.sha256(data).hexdigest()}
            ]}))
            errors = []
            check_vendor_assets(vendor, errors)
            self.assertEqual(errors, [])
            asset.write_bytes(b"modified bytes")
            check_vendor_assets(vendor, errors)
            self.assertTrue(any("differs" in error for error in errors))
            asset.unlink()
            errors = []
            check_vendor_assets(vendor, errors)
            self.assertTrue(any("missing" in error for error in errors))
            asset.write_bytes(data)
            (vendor / "unreviewed.js").write_bytes(b"extra")
            errors = []
            check_vendor_assets(vendor, errors)
            self.assertTrue(any("does not cover" in error for error in errors))

    def test_path_escape_and_duplicate_records_are_blocking(self):
        with tempfile.TemporaryDirectory() as directory:
            vendor = Path(directory)
            for paths in [["../outside.js"], ["same.js", "same.js"]]:
                with self.subTest(paths=paths):
                    (vendor / "asset-manifest.json").write_text(json.dumps({"files": [
                        {"path": path, "bytes": 0, "sha256": "invalid"} for path in paths
                    ]}))
                    errors = []
                    check_vendor_assets(vendor, errors)
                    self.assertTrue(any("invalid or duplicate" in error for error in errors))

    def test_nested_inventory_and_symbolic_links_are_blocking(self):
        with tempfile.TemporaryDirectory() as directory:
            vendor = Path(directory) / "vendor"
            vendor.mkdir()
            manifest = vendor / "asset-manifest.json"
            manifest.write_text('{"files": []}')
            nested = vendor / "nested"
            nested.mkdir()
            extra = nested / "asset-manifest.json"
            extra.write_text("unreviewed")
            errors = []
            check_vendor_assets(vendor, errors)
            self.assertTrue(errors)
            extra.unlink()
            nested.rmdir()
            for link, destination in [(nested, directory), (manifest, Path(directory) / "inventory")]:
                with self.subTest(link=link.name):
                    if link == manifest:
                        manifest.rename(destination)
                    link.symlink_to(destination)
                    errors = []
                    check_vendor_assets(vendor, errors)
                    self.assertTrue(any("symlink" in error for error in errors))
                    link.unlink()
                    if link == manifest:
                        destination.rename(manifest)
            alias = Path(directory) / "vendor-alias"
            alias.symlink_to(vendor, target_is_directory=True)
            errors = []
            check_vendor_assets(alias, errors)
            self.assertTrue(any("symlink" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
