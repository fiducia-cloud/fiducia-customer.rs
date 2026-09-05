from __future__ import annotations

import importlib.util
import stat
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[1]


def load_script(filename: str, module_name: str) -> ModuleType:
    path = ROOT / "scripts" / filename
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"unable to load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


checker = load_script("check-generated-contract.py", "fiducia_generated_contract_checker")
seeder = load_script("seed-contract-fixtures.py", "fiducia_generated_contract_seeder")


class GeneratedContractCheckerTests(unittest.TestCase):
    def test_read_policy_normalizes_marker_case_and_spacing(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            generated = Path(temp) / "generated"
            generated.mkdir()
            (generated / "README.md").write_text(
                "<!--   GENERATED-POLICY:   FrOzEn   -->\n",
                encoding="utf-8",
            )

            self.assertEqual(checker.read_policy(generated), checker.POLICY_FROZEN)

    def test_freeze_tree_protects_payloads_but_keeps_policy_readme_writable(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            generated = Path(temp) / "generated"
            nested = generated / "nested"
            nested.mkdir(parents=True)
            readme = generated / "README.md"
            payload = generated / "client.ts"
            nested_payload = nested / "model.rs"
            readme.write_text("<!-- generated-policy: frozen -->\n", encoding="utf-8")
            payload.write_text("export const value = 1;\n", encoding="utf-8")
            nested_payload.write_text("pub const VALUE: u8 = 1;\n", encoding="utf-8")

            self.assertEqual(checker.freeze_tree(generated), 2)
            self.assertFalse(checker.is_writable(payload))
            self.assertFalse(checker.is_writable(nested_payload))
            self.assertTrue(checker.is_writable(readme))

            checker.thaw_file(payload)
            self.assertTrue(payload.stat().st_mode & stat.S_IWUSR)

    def test_structural_validation_reports_type_required_and_extra_fields(self) -> None:
        schema = {
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
            "additionalProperties": False,
        }

        self.assertEqual(checker.structural_validate([], schema), ["instance is not an object"])
        self.assertEqual(
            checker.structural_validate({"extra": True}, schema),
            ["missing required property 'name'", "undeclared property 'extra'"],
        )
        self.assertEqual(checker.structural_validate({"name": "Fiducia"}, schema), [])

    def test_cli_flag_walk_recurses_without_treating_command_metadata_as_env(self) -> None:
        contract = {
            "flags": {
                "json": {"env": "FIDUCIA_JSON", "type": "boolean", "default": False}
            },
            "commands": {
                "quote": {
                    "flags": {
                        "region": {"env": "FIDUCIA_REGION", "default": "iad"}
                    },
                    "commands": {
                        "preview": {
                            "flags": {
                                "limit": {
                                    "env": "FIDUCIA_PREVIEW_LIMIT",
                                    "type": "integer",
                                    "default": "3",
                                }
                            }
                        }
                    },
                }
            },
        }

        self.assertEqual(
            set(checker.walk_cli_flags(contract)),
            {"FIDUCIA_JSON", "FIDUCIA_REGION", "FIDUCIA_PREVIEW_LIMIT"},
        )

    def test_default_coercion_covers_supported_scalar_and_json_types(self) -> None:
        cases = [
            ({"type": "boolean", "default": "yes"}, True),
            ({"type": "integer", "default": "42"}, 42),
            ({"type": "number", "default": "3.5"}, 3.5),
            ({"type": "json", "default": '["rust", "dart"]'}, ["rust", "dart"]),
            ({"type": "integer", "default": "not-an-int"}, "not-an-int"),
            ({"type": "string"}, None),
        ]
        for spec, expected in cases:
            with self.subTest(spec=spec):
                self.assertEqual(checker.coerce_default(spec), expected)

    def test_instance_from_cli_flags_only_materializes_declared_defaults(self) -> None:
        envs = {
            "FIDUCIA_JSON": {"type": "boolean", "default": "1"},
            "FIDUCIA_REQUIRED": {"type": "string"},
        }

        self.assertEqual(checker.instance_from_cli_flags(envs), {"FIDUCIA_JSON": True})


class GeneratedContractSeederTests(unittest.TestCase):
    def test_sample_precedence_is_const_then_enum_then_default_then_example(self) -> None:
        self.assertEqual(
            seeder.sample_for(
                {
                    "const": "const-value",
                    "enum": ["enum-value"],
                    "default": "default-value",
                    "examples": ["example-value"],
                }
            ),
            "const-value",
        )
        self.assertEqual(seeder.sample_for({"enum": ["enum-value"]}), "enum-value")
        self.assertEqual(seeder.sample_for({"default": "default-value"}), "default-value")
        self.assertEqual(seeder.sample_for({"examples": ["example-value"]}), "example-value")

    def test_sample_for_builds_required_nested_objects_and_formats(self) -> None:
        schema = {
            "type": "object",
            "required": ["request_id", "callback", "contact"],
            "properties": {
                "request_id": {"type": "string", "format": "uuid"},
                "callback": {"type": "string", "format": "uri"},
                "contact": {
                    "type": "object",
                    "required": ["name"],
                    "properties": {"name": {"type": "string", "minLength": 3}},
                },
            },
        }

        self.assertEqual(
            seeder.sample_for(schema),
            {
                "request_id": "00000000-0000-4000-8000-000000000000",
                "callback": "https://example.invalid/x",
                "contact": {"name": "xxx"},
            },
        )

    def test_composition_merges_required_properties_from_the_first_branch(self) -> None:
        schema = {
            "type": "object",
            "required": ["base"],
            "properties": {"base": {"const": "base"}},
            "allOf": [
                {
                    "required": ["tier"],
                    "properties": {"tier": {"enum": ["enterprise"]}},
                }
            ],
        }

        self.assertEqual(
            seeder.sample_for(schema),
            {"base": "base", "tier": "enterprise"},
        )

    def test_negative_fixtures_cover_required_type_and_unknown_field_drift(self) -> None:
        schema = {
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string", "minLength": 1},
                "seats": {"type": "integer"},
            },
            "additionalProperties": False,
        }
        valid = {"name": "Fiducia", "seats": 10}
        labels = [label for label, _instance in seeder.negatives(schema, valid)]

        self.assertEqual(
            labels,
            [
                "missing-required-name",
                "wrong-type-name",
                "empty-name",
                "wrong-type-seats",
                "unknown-field",
            ],
        )


if __name__ == "__main__":
    unittest.main()
