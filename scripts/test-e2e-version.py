"""Pin module version selection with reordered workspace metadata."""
import json
from pathlib import Path
import re
import subprocess
import unittest

class VersionTests(unittest.TestCase):
    def test_module_version_is_selected_by_package_name(self):
        """A contract or helper package never supplies the module archive version."""
        script = Path(__file__).with_name("test-e2e.sh").read_text()
        query = re.search(r"jq -er '([^']+)'", script)[1]
        result = subprocess.run(["jq", "-er", query], input=json.dumps({"packages": [
            {"name": "unrelated-contract", "version": "9.9.9"},
            {"name": "tinybox-module", "version": "0.1.2-beta.1"}]}),
            text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "0.1.2-beta.1")

if __name__ == "__main__": unittest.main()
