"""Focused examples for the cargo metadata dependency guard."""

import copy
import unittest

from zakura_dependencies import REQUIRED_DIRECT, REQUIRED_PACKAGES, violations


def metadata():
    return {
        "workspace_members": ["server"],
        "packages": [
            {"id": "server", "name": "ths-server", "version": "0.0.0",
             "dependencies": [{"name": name} for name in REQUIRED_DIRECT]},
            *[{"id": name, "name": name, "version": "4.0.0", "dependencies": []}
              for name in sorted(REQUIRED_PACKAGES)],
        ],
    }


class CommonDependencies(unittest.TestCase):
    def test_coherent_family_allows_independent_zcash_packages(self):
        graph = metadata()
        for name in ["zcash_script", "zcash_encoding", "zcash_spec", "zakura-client-backend"]:
            graph["packages"].append({"id": name, "name": name, "version": "0.1.0", "dependencies": []})
        self.assertEqual(violations(graph), [])

    def test_old_or_duplicate_family_version_is_rejected(self):
        for version in ["2.2.0", "3.0.0", "4.0.0", "4.1.0"]:
            graph = metadata()
            duplicate = copy.deepcopy(graph["packages"][1])
            duplicate["version"] = version
            graph["packages"].append(duplicate)
            self.assertTrue(violations(graph))

    def test_upstream_note_encryption_is_rejected(self):
        graph = metadata()
        graph["packages"].append({"id": "upstream", "name": "zcash_note_encryption", "version": "0.4.0", "dependencies": []})
        self.assertTrue(violations(graph))

    def test_transitive_note_encryption_is_allowed(self):
        graph = metadata()
        self.assertNotIn("zakura-note-encryption", REQUIRED_DIRECT)
        self.assertEqual(violations(graph), [])

    def test_missing_note_encryption_is_rejected(self):
        graph = metadata()
        graph["packages"] = [package for package in graph["packages"] if package["name"] != "zakura-note-encryption"]
        self.assertTrue(any("package is missing: zakura-note-encryption" in error for error in violations(graph)))

    def test_required_direct_dependency_cannot_be_only_transitive(self):
        graph = metadata()
        graph["packages"][0]["dependencies"] = [dependency for dependency in graph["packages"][0]["dependencies"] if dependency["name"] != "zakura-keys"]
        self.assertTrue(any("direct Common dependency is missing: zakura-keys" in error for error in violations(graph)))


if __name__ == "__main__":
    unittest.main()
