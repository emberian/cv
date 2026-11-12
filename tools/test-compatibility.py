#!/usr/bin/env python3
"""Narrow mutation tests: these must fail for actual breaking contracts, not cosmetic edits."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("compat", Path(__file__).with_name("check-compatibility.py"))
compat = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compat)


class Compatibility(unittest.TestCase):
    def setUp(self):
        self.old = {"commands": [{"name": "show", "args": [{"name": "id", "required": True, "kind": "positional", "value_type": "string"},
            {"name": "last", "required": False, "kind": "option", "value_type": "integer", "default": "50"}]}],
            "tools": [{"name": "show", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}, "follow": {"type": "boolean"}}, "required": ["id"]}}],
            "schema": {"message": {"fields": ["role", "content"]}}, "rows": {"claude": {"id": "str", "message_count": "int"}, "codex": {"id": "str", "message_count": "int"}},
            "session": {"messages": [{"role": "=user", "content": []}]},
            "routes": {"GET /api/sessions": {"status": 200, "pointer": ""}, "GET /api/task/{}": {"status": 404, "pointer": ""}}}

    def test_additions_help_and_a_baseline_404_are_allowed(self):
        new = copy.deepcopy(self.old)
        new["commands"][0]["about"] = "new prose"
        new["commands"][0]["args"].append({"name": "optional", "required": False})
        new["tools"][0]["inputSchema"]["properties"]["optional"] = {"type": "string"}
        new["session"]["messages"][0]["new_field"] = "str"
        new["schema"]["message"]["fields"].append("new_field")
        new["routes"]["GET /api/task/{}"]["status"] = 200
        new["routes"]["GET /api/new"] = {"status": 200, "pointer": ""}
        compat.compare(self.old, new)

    def test_removal_type_default_requiredness_and_routes_fail(self):
        mutations = [lambda n: n["commands"][0]["args"].pop(),
            lambda n: n["commands"][0]["args"][1].pop("default"),
            lambda n: n["commands"][0]["args"][1].update(default="60"),
            lambda n: n["commands"][0]["args"][1].update(value_type="string"),
            lambda n: n["commands"][0]["args"][1].update(required=True),
            lambda n: n["commands"][0]["args"].append({"name": "new", "required": True}),
            lambda n: n["tools"][0]["inputSchema"]["properties"]["id"].update(type="integer"),
            lambda n: n["tools"][0]["inputSchema"]["properties"].pop("follow"),
            lambda n: n["tools"][0]["inputSchema"].update(required=["id", "new"]),
            lambda n: n["tools"].clear(), lambda n: n["session"]["messages"][0].pop("role"),
            lambda n: n["session"]["messages"][0].update(role="=assistant"),
            lambda n: n["schema"]["message"]["fields"].remove("content"),
            lambda n: n["rows"]["claude"].pop("id"), lambda n: n["rows"]["claude"].update(id="int"),
            lambda n: n["rows"].pop("codex"), lambda n: n["rows"]["codex"].update(message_count="null"),
            lambda n: n["routes"].clear(),
            lambda n: n["routes"]["GET /api/sessions"].update(status=404)]
        for mutation in mutations:
            new = copy.deepcopy(self.old)
            mutation(new)
            with self.subTest(mutation=mutation), self.assertRaises(compat.Incompatible):
                compat.compare(self.old, new)

    def test_an_accepted_removal_is_exact(self):
        new = copy.deepcopy(self.old)
        new["tools"][0]["inputSchema"]["properties"].pop("follow")
        compat.compare(self.old, new, {"tools.show.inputSchema.properties.follow"})
        with self.assertRaises(compat.Incompatible):
            compat.compare(self.old, new, {"tools.show.inputSchema.properties.id"})
        new["tools"][0]["inputSchema"]["properties"].pop("id")
        with self.assertRaises(compat.Incompatible):
            compat.compare(self.old, new, {"tools.show.inputSchema.properties.follow"})

    def test_an_accepted_value_change_is_exact(self):
        new = copy.deepcopy(self.old)
        new["tools"][0]["inputSchema"]["properties"]["id"]["type"] = "integer"
        path = "tools.show.inputSchema.properties.id.type"
        compat.compare(self.old, new, {path})
        with self.assertRaises(compat.Incompatible):
            compat.compare(self.old, new, {"tools.show.inputSchema.properties.follow.type"})

    def test_an_accepted_uncounted_row_is_exact(self):
        # The reviewed 0.18 change: a deferred-count harness's row count becomes null.
        new = copy.deepcopy(self.old)
        new["rows"]["codex"]["message_count"] = "null"
        compat.compare(self.old, new, {"rows.codex.message_count"})
        new["rows"]["claude"]["message_count"] = "null"
        with self.assertRaises(compat.Incompatible):
            compat.compare(self.old, new, {"rows.codex.message_count"})

    def test_accepted_command_tools_and_route_removals_are_exact(self):
        new = copy.deepcopy(self.old)
        new["commands"].clear()
        new["tools"].clear()
        new["routes"].pop("GET /api/sessions")
        with self.assertRaises(compat.Incompatible):
            compat.compare(self.old, new, {"commands.show", "tools"})
        compat.compare(self.old, new, {"commands.show", "tools", "routes.GET /api/sessions"})


if __name__ == "__main__": unittest.main()
