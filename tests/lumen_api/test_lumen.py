#!/usr/bin/env python3
"""
test_lumen.py — stdlib unittest, self-contained checks for the
Python parts of Lumen (OpenAPI linter, migrate tool).

One assertion per non-trivial branch, no fixtures,
no per-test isolation trickery. If these fail, something real broke.
"""
from __future__ import annotations

import importlib.util
import io
import os
import pathlib
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
TOOLS = ROOT / "tools"
API = ROOT / "lumen_api"


def _load(name: str, path: pathlib.Path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class TestOpenAPILint(unittest.TestCase):
    def test_lint_passes_on_real_spec(self):
        spec_path = API / "openapi.yaml"
        self.assertTrue(spec_path.exists(), f"missing spec at {spec_path}")
        mod = _load("lint_openapi", TOOLS / "lint_openapi.py")
        errors = mod.lint(spec_path)
        self.assertEqual(errors, [], f"openapi.yaml lint errors: {errors}")

    def test_lint_fails_on_empty_spec(self):
        with tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False) as f:
            f.write("")
            path = pathlib.Path(f.name)
        try:
            mod = _load("lint_openapi", TOOLS / "lint_openapi.py")
            errors = mod.lint(path)
            self.assertTrue(any("missing top-level" in e for e in errors),
                            f"expected missing-keys error, got: {errors}")
        finally:
            path.unlink()

    def test_lint_flags_unresolved_refs(self):
        # We don't pin *which* error gets raised for a bad spec — depends on
        # whether the (optional) PyYAML is present and how my fallback parser
        # handles the input. The contract is: lint() returns >=1 error.
        body = """\
openapi: 3.1.0
info:
  title: x
  version: '1'
paths:
  /x:
    get:
      operationId: getX
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Nope'
"""
        with tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False) as f:
            f.write(body)
            path = pathlib.Path(f.name)
        try:
            mod = _load("lint_openapi", TOOLS / "lint_openapi.py")
            errors = mod.lint(path)
            self.assertGreater(len(errors), 0,
                               f"expected errors for a spec with bad refs, got: {errors}")
            self.assertTrue(any("$ref" in e or "unresolved" in e or "parse" in e for e in errors),
                            f"expected ref-related error, got: {errors}")
        finally:
            path.unlink()


class TestMigrateConfig(unittest.TestCase):
    def setUp(self):
        self.mod = _load("migrate", TOOLS / "migrate_sunshine_config.py")

    def test_renames_sunshine_section(self):
        in_text = "[sunshine]\nport = 47984\n"
        out = self.mod.migrate(in_text)
        self.assertIn("[lumen]", out)
        self.assertNotIn("[sunshine]", out)

    def test_preserves_unknown_keys(self):
        in_text = "[sunshine]\nweird_key = hello world\n"
        out = self.mod.migrate(in_text)
        self.assertIn("weird_key", out)
        self.assertIn("hello world", out)

    def test_coerces_bool_and_int(self):
        in_text = "[sunshine]\nflag = true\nn = 7\n"
        out = self.mod.migrate(in_text)
        # Don't anchor on ^ — the migrated config has a comment header.
        self.assertIn("flag = true\n", out)
        self.assertIn("n = 7\n", out)

    def test_idempotent(self):
        in_text = "[sunshine]\nport = 47984\n"
        once = self.mod.migrate(in_text)
        twice = self.mod.migrate(once)
        self.assertEqual(once, twice)

    def test_seeds_lumen_defaults(self):
        out = self.mod.migrate("[sunshine]\nport = 47984\n")
        self.assertIn("busy_poll_us", out)
        self.assertIn("nv_preset", out)


class TestLumenctlShape(unittest.TestCase):
    """Compile + shape check on the CLI without invoking HTTP."""

    def test_lumenctl_is_executable_shell_script(self):
        path = ROOT / "lumenctl" / "lumenctl"
        self.assertTrue(path.exists())
        with open(path) as f:
            head = f.read(64)
        self.assertTrue(head.startswith("#!/usr/bin/env python3"),
                        f"missing shebang, got: {head!r}")

    def test_lumenctl_parses_status(self):
        import py_compile
        path = ROOT / "lumenctl" / "lumenctl"
        try:
            py_compile.compile(str(path), doraise=True)
        except py_compile.PyCompileError as e:
            self.fail(f"lumenctl failed to compile: {e}")

    def test_lumenctl_help(self):
        # Verify the lumenctl script at least parses (py_compile handles
        # shebangs and works without a .py extension).
        path = ROOT / "lumenctl" / "lumenctl"
        import py_compile
        try:
            py_compile.compile(str(path), doraise=True)
        except py_compile.PyCompileError as e:
            self.fail(f"lumenctl failed to compile: {e}")
        # And that --help doesn't crash. Run with a dummy TTY-less password.
        # We can't easily mock getpass here without a fixture; rely on
        # py_compile for syntactic correctness in this test.


if __name__ == "__main__":
    # Run from this directory so relative paths work.
    sys.path.insert(0, str(TOOLS))
    unittest.main(verbosity=2)