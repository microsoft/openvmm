# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
import tempfile
import unittest

import investigate_ci


class JunitArtifactTests(unittest.TestCase):
    def test_old_and_combined_unit_artifacts_are_selected_without_duplicates(self):
        names = [
            "x64-windows-unit-tests-junit-xml",
            "x64-windows-unit-tests-resources",
            "x64-linux-unit-tests-base-resources",
            "x64-linux-unit-tests-extra-junit-xml",
            "x64-linux-vmm-tests-resources",
            "x64-linux-vmm-tests-logs",
            "unrelated-resources",
        ]
        self.assertEqual(investigate_ci.list_junit_artifacts(names), [
            "x64-windows-unit-tests-junit-xml",
            "x64-linux-unit-tests-base-resources",
            "x64-linux-unit-tests-extra-junit-xml",
        ])

    def test_combined_artifact_keeps_failure_output_available(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            name = "x64-linux-unit-tests-base-resources"
            artifact = root / name
            artifact.mkdir()
            (artifact / "junit.xml").write_text(
                '<testsuites><testsuite><testcase classname="crate" name="test">'
                '<failure message="assertion failed"/>'
                '<system-out>panicked at the assertion</system-out>'
                '</testcase></testsuite></testsuites>'
            )
            (artifact / "metadata.json").write_text("{}")
            output = StringIO()
            with redirect_stdout(output):
                count = investigate_ci.show_junit_failures(
                    "unused-cached-run", investigate_ci.list_junit_artifacts([name]), root
                )
            self.assertEqual(count, 1)
            self.assertIn("FAIL: crate::test", output.getvalue())
            self.assertIn("panicked at the assertion", output.getvalue())


if __name__ == "__main__":
    unittest.main()
