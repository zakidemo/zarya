#!/usr/bin/env python3
"""Soundness check for `zarya --label-json`: run tricky programs on marked data and
verify that every marker that reaches the output is covered by the label."""
import json, subprocess, sys, tempfile, os
from pathlib import Path
Z = sys.argv[1]

# Resource data. Every leaf carries a marker naming its resource and top-level key.
ROWS = [{"id": 1001 + i, "name": f"M~rows~name~{i}", "price": 5001 + i, "secret": f"M~rows~secret~{i}",
         "nest": {"x": f"M~rows~nest~{i}", "y": 7001 + i}} for i in range(3)]
CONF = {"idx": 1, "key": "name", "token": "M~conf~token", "limit": 6000, "res": "rows",
        "deep": {"a": "M~conf~deep", "b": {"c": "M~conf~deep~2"}}}
INT_MARKS = {}
for i in range(3):
    INT_MARKS[str(1001 + i)] = ("rows", "id"); INT_MARKS[str(5001 + i)] = ("rows", "price"); INT_MARKS[str(7001 + i)] = ("rows", "nest")
INT_MARKS["6000"] = ("conf", "limit")
RES = {"rows": json.dumps(ROWS), "conf": json.dumps(CONF)}
HEAD = 'permission text rows; permission text conf;\nschema Pub { id: Int; name: Text; }\nschema Wide { id: Int; name: Text; price: Int; secret: Text; nest: Json; }\n'
ROWSV = 'let rows: List<Json> = json_items(json_parse(read_text("rows")));\nlet conf: Json = json_parse(read_text("conf"));\n'

PROGRAMS = {
 "whole": 'print(read_text("rows"));',
 "field": 'for r in rows { print(json_get(r, "name")); }',
 "nested": 'for r in rows { print(json_get(json_get(r, "nest"), "x")); }',
 "map_filter": 'print(json_list(map(filter(rows, r, json_as_int(json_get(r, "price")) <= json_as_int(json_get(conf, "limit"))), r => json_get(r, "id"))));',
 "index_from_conf": 'print(json_get(at(rows, json_as_int(json_get(conf, "idx"))), "name"));',
 "dynamic_key": 'print(json_get(at(rows, 0), json_as_text(json_get(conf, "key"))));',
 "rewrap": 'print(json_get(json_object("id", at(rows, 0)), "id"));',
 "rewrap_array": 'print(json_at(json_array(at(rows, 0), json_get(conf, "token")), 0));',
 "concat_parse": 'print(json_get(json_parse("{\\"id\\":" + read_text("conf") + "}"), "id"));',
 "schema_pub": 'print(json_list(json_records(json_parse(read_text("rows")), "Pub")));',
 "schema_wide_then_get": 'let all: List<Json> = json_records(json_parse(read_text("rows")), "Wide"); print(json_get(at(all, 2), "secret"));',
 "schema_twice": 'let all: List<Json> = json_records(json_parse(read_text("rows")), "Wide"); print(json_validate(at(all, 0), "Pub"));',
 "result_value": 'let w: Json = json_records_result(json_parse(read_text("rows")), "Wide"); print(json_get(w, "value"));',
 "result_whole": 'let w: Json = json_records_result(json_parse(read_text("rows")), "Pub"); print(w);',
 "result_ok": 'let w: Json = json_records_result(json_parse(read_text("rows")), "Pub"); print(json_get(w, "ok")); print(json_get(w, "error"));',
 "fn_chain": 'fn pick(r: Json): Json = json_get(r, "name"); fn twice(r: Json): Json = json_array(pick(r), json_get(r, "id")); print(twice(at(rows, 1)));',
 "fn_reads": 'fn all(): Text = read_text("conf"); print(json_get(json_parse(all()), "token"));',
 "append_mix": 'let xs: List<Json> = append(map(rows, r => json_get(r, "name")), json_get(conf, "token")); print(json_list(xs));',
 "list_literal": 'let xs: List<Json> = [json_get(at(rows, 0), "secret"), json_get(conf, "deep")]; print(json_get(at(xs, 1), "a"));',
 "count": 'print(len(rows)); print(json_len(json_parse(read_text("rows"))));',
 "sum": 'print(sum(map(rows, r => json_as_int(json_get(r, "price")))));',
 "text_ops": 'for r in rows { print(json_as_text(json_get(r, "name")) + ":" + json_as_text(json_get(conf, "token"))); }',
 "cond_only": 'if text_contains(read_text("conf"), "token") { print("yes"); } else { print("no"); }',
 "dynamic_read": 'print(json_get(at(json_items(json_parse(read_text(json_as_text(json_get(conf, "res"))))), 0), "id"));',
 "map_identity_get": 'print(json_get(at(map(rows, r => r), 0), "name"));',
 "map_nest_get": 'print(json_get(at(map(rows, r => json_get(r, "nest")), 0), "x"));',
 "filter_keeps_shape": 'print(json_get(at(filter(rows, r, json_as_int(json_get(r, "id")) > 0), 0), "price"));',
 "json_items_deep": 'print(json_get(json_get(json_get(conf, "deep"), "b"), "c"));',
 "compare": 'print(json_as_int(json_get(at(rows, 0), "price")) == json_as_int(json_get(conf, "limit")) - 1);',
}

def covered(label, resource, field):
    for use in label["output"]:
        if use["resource"] == resource and (use["all"] or field in use["fields"]):
            return True
    return False

def markers(text):
    found = set()
    for part in text.replace('"', ' ').replace(',', ' ').replace(':', ' ').replace('[', ' ').replace(']', ' ').replace('{', ' ').replace('}', ' ').split():
        if part.startswith("M~"):
            _, resource, field = part.split("~")[:3]; found.add((resource, field))
        elif part in INT_MARKS: found.add(INT_MARKS[part])
    return found

bad = 0; ran = 0; precise = 0
with tempfile.TemporaryDirectory() as tmp:
    grants = []
    for name, text in RES.items():
        Path(tmp, name + ".json").write_text(text, encoding="utf-8"); grants += ["--grant-text", f"{name}={Path(tmp, name + '.json')}"]
    for name, body in PROGRAMS.items():
        source = Path(tmp, name + ".zr"); source.write_text(HEAD + ROWSV + body + "\n", encoding="utf-8")
        lab = subprocess.run([Z, "--label-json", source], capture_output=True, text=True, encoding="utf-8")
        run = subprocess.run([Z, source, *grants], capture_output=True, text=True, encoding="utf-8")
        if not lab.stdout.strip() or run.returncode != 0:
            print(f"SETUP PROBLEM {name}: label={lab.stdout.strip()[:120]} {lab.stderr.strip()[:200]} run={run.stderr.strip()[:200]}"); bad += 1; continue
        label = json.loads(lab.stdout); ran += 1
        seen = markers(run.stdout)
        missing = sorted(m for m in seen if not covered(label, *m))
        summary = "; ".join(f"{u['resource']}:" + ("ALL" if u["all"] else ",".join(u["fields"]) + ("+count" if u["count"] else "") + ("+valid" if u["validity"] else "")) for u in label["output"]) or "-"
        if not any(u["all"] for u in label["output"]): precise += 1
        status = "UNSOUND" if missing else "ok"
        if missing: bad += 1
        print(f"{status:8} {name:22} seen={sorted(seen)}  label={summary}" + (f"  MISSING={missing}" if missing else ""))
print(f"\n{ran} programs ran, {bad} problems, {precise} labels field-precise")
sys.exit(1 if bad else 0)
