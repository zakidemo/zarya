#!/usr/bin/env python3
"""Zarya demo (runtime dev.14): static labels, then four attacks against naive PHP and Zarya.

Run from this folder:
    python -I -X utf8 run_demo.py
Options:
    --zarya PATH   use an existing zarya executable instead of building one
    --php PATH     full path to the PHP CLI if it is not in PATH

Everything is local and synthetic. Nothing is sent anywhere.
The Zarya side runs the real runtime. The catalog snapshot is produced here by
one fixed SQL query on public columns, standing in for the Rust SQLite host.
"""
import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORK = HERE / "_work"
APP = HERE / "zarya_app"

SECRET = "DEMO_SECRET_db_password=not-a-real-password"
HASH = "demo-hash-not-real"
HIDDEN = "Unpublished draft workshop"


def run(cmd, cwd=None):
    p = subprocess.run([str(c) for c in cmd], cwd=cwd, capture_output=True,
                       text=True, encoding="utf-8", errors="replace", timeout=300)
    return p.returncode, (p.stdout or "").strip(), (p.stderr or "").strip()


def find_zarya(arg):
    if arg:
        path = Path(arg)
        if not path.is_file():
            sys.exit(f"STOP: --zarya file not found: {path}")
        return path
    cargo = shutil.which("cargo")
    if not cargo:
        sys.exit("STOP: cargo not found. Install Rust, or pass --zarya PATH to your zarya executable.")
    target = WORK / "target"
    print("Building the Zarya runtime from runtime/rust (offline, no dependencies)...")
    code, out, err = run([cargo, "build", "--release", "--offline", "--target-dir", target],
                         cwd=HERE / "runtime" / "rust")
    if code != 0:
        sys.exit("STOP: runtime build failed:\n" + err[-2000:])
    exe = target / "release" / ("zarya.exe" if os.name == "nt" else "zarya")
    if not exe.is_file():
        sys.exit(f"STOP: built executable not found at {exe}")
    return exe


def php_with_sqlite(php):
    """Return (command, note). The command is None when this PHP cannot load SQLite3."""
    probe = ["-r", "echo class_exists('SQLite3') ? 'sqlite-yes' : 'sqlite-no';"]
    if "sqlite-yes" in run([php, *probe])[1]:
        return [php], ""
    # A PHP without php.ini loads no extensions. Enable SQLite3 for this run only.
    ext = Path(php).resolve().parent / "ext"
    flags = ["-d", f"extension_dir={ext}", "-d", "extension=sqlite3"]
    if "sqlite-yes" in run([php, *flags, *probe])[1]:
        return [php, *flags], "SQLite3 enabled for this run only (this PHP has no php.ini)"
    return None, "this PHP cannot load the SQLite3 extension, so attacks 1 to 3 cannot run on the PHP side"


def make_data():
    WORK.mkdir(exist_ok=True)
    db = WORK / "demo.sqlite"
    if db.exists():
        db.unlink()
    con = sqlite3.connect(db)
    con.executescript("""
        CREATE TABLE courses (id INTEGER PRIMARY KEY, title TEXT, price_minor INTEGER,
                              cost_minor INTEGER, published INTEGER);
        CREATE TABLE staff (id INTEGER PRIMARY KEY, email TEXT, password_hash TEXT);
    """)
    con.executemany("INSERT INTO courses VALUES (?,?,?,?,?)", [
        (301, "ورشة الرؤية الحاسوبية", 3000, 900, 1),
        (302, "ورشة معالجة الإشارات", 4500, 1400, 1),
        (303, HIDDEN, 100, 50, 0),
    ])
    con.executemany("INSERT INTO staff VALUES (?,?,?)", [
        (1, "admin@example.test", HASH + "-1"),
        (2, "finance@example.test", HASH + "-2"),
    ])
    con.commit()
    # Stand-in for the Rust host: one fixed query, public columns only, no user input in SQL.
    rows = con.execute("SELECT id, title, price_minor FROM courses "
                       "WHERE published = 1 ORDER BY id LIMIT 20").fetchall()
    con.close()
    snapshot = WORK / "courses_public.json"
    snapshot.write_text(json.dumps(
        [{"id": i, "title": t, "price_minor": p} for i, t, p in rows], ensure_ascii=False),
        encoding="utf-8")
    secrets = WORK / "secrets.txt"
    secrets.write_text(SECRET, encoding="utf-8")
    return db, snapshot, secrets


def leaks(text):
    found = []
    if HASH in text:
        found.append("staff emails and password hashes")
    if HIDDEN in text:
        found.append("an unpublished course")
    if "cost_minor" in text:
        found.append("the private cost column")
    if "DEMO_SECRET" in text:
        found.append("a server secret file")
    return found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--zarya")
    ap.add_argument("--php")
    args = ap.parse_args()

    php = args.php or shutil.which("php")
    if php and not Path(php).is_file():
        sys.exit(f"STOP: PHP not found at {php}")
    php_cmd, php_note = php_with_sqlite(php) if php else (None, "")
    zarya = find_zarya(args.zarya)
    db, snapshot, secrets = make_data()
    version = run([zarya, "--version"])[1]

    def php_search(request):
        req = WORK / "request_php.json"
        req.write_text(json.dumps(request, ensure_ascii=False), encoding="utf-8")
        if php_cmd is None:
            return None
        code, out, err = run([*php_cmd, HERE / "php_naive" / "search.php", db, req])
        try:
            json.loads(out)
        except Exception:
            # A crash is not a result. Never report it as "no leak".
            return {"error": (out or err or f"exit code {code}").strip().splitlines()[0][:160]}
        return out

    def zarya_search(request, program="main.zr"):
        req = WORK / "request_zarya.json"
        req.write_text(json.dumps(request, ensure_ascii=False), encoding="utf-8")
        code, out, err = run([zarya, "--resource-policy-v1", APP / "resource_policy.json",
                              "--contract-policy-v1", APP / "output_contract.json",
                              APP / program, "--safe-output",
                              "--grant-text", f"request={req}",
                              "--grant-text", f"courses_public={snapshot}"])
        return code, out, err

    def zarya_plugin(program):
        req = WORK / "request_zarya.json"
        return run([zarya, "--resource-policy-v1", APP / "resource_policy.json", APP / program,
                    "--grant-text", f"request={req}", "--grant-text", f"secrets={secrets}"])

    normal = {"max_price_minor": 4500, "search_text": "ورشة"}
    results = []

    def record(name, php_out, z_code, z_out, z_err):
        php_error = php_out["error"] if isinstance(php_out, dict) else None
        php_leaks = leaks(php_out) if isinstance(php_out, str) else None
        z_leaks = leaks(z_out + z_err)
        results.append({
            "attack": name,
            "php": ("DID NOT RUN: " + php_error) if php_error else None if php_leaks is None else
                   ("LEAKED: " + ", ".join(php_leaks) if php_leaks else "no leak"),
            "zarya": "LEAKED: " + ", ".join(z_leaks) if z_leaks else
                     ("BLOCKED: " + (z_err.splitlines()[0] if z_err else "rejected") if z_code != 0
                      else "SAFE: " + z_out[:90]),
            "zarya_ok": not z_leaks,
            "php_leaked": bool(php_leaks),
        })

    # 0. Sanity: a normal search must still work in Zarya.
    code, out, err = zarya_search(normal)
    try:
        ids = [c["id"] for c in json.loads(out)["courses"]]
    except Exception:
        ids = None
    sanity = code == 0 and ids == [301, 302]

    # 1. SQL injection through the search text.
    attack = {"max_price_minor": 4500,
              "search_text": "x%' UNION SELECT id, email, 0, 0, password_hash FROM staff --"}
    record("1. SQL injection in the search box", php_search(attack) if php else None, *zarya_search(attack))

    # 2. Wrong type: text where a number is expected.
    attack = {"max_price_minor": "0 OR published = 0", "search_text": ""}
    record("2. Text sent where a number is expected", php_search(attack) if php else None, *zarya_search(attack))

    # 3. Private column exposure on an ordinary request.
    record("3. Private column returned by mistake", php_search(normal) if php else None,
           *zarya_search(normal, "leaky.zr"))

    # 4. Code reading a file it was never approved to read.
    php_out = run([php, HERE / "php_naive" / "plugin.php", secrets])[1] if php else None
    record("4a. Plugin reads a secret file (declared, not approved)", php_out,
           *zarya_plugin("plugin_reads_secret.zr"))
    record("4b. Plugin reads a secret file (not even declared)", php_out,
           *zarya_plugin("plugin_reads_undeclared.zr"))

    # Labels: what each program can touch, computed before anything runs.
    def label(program):
        code, out, err = run([zarya, "--label-json", APP / program])
        try:
            return code, json.loads(out)
        except Exception:
            return code, None

    def describe(data):
        if data is None:
            return "no label produced"
        uses = "; ".join(u["resource"] + ": " + ("any part" if u["all"] else "fields " + ", ".join(u["fields"]))
                         for u in data["output"]) or "nothing"
        notes = []
        if data["undeclared_reads"]:
            notes.append("NOT CLEAN, reads undeclared: " + ", ".join(data["undeclared_reads"]))
        if data["dynamic_read"]:
            notes.append("NOT CLEAN, computes a resource name")
        return f"reads [{', '.join(data['reads']) or 'nothing'}] -> output can contain [{uses}]" + \
               ("   " + "; ".join(notes) if notes else "")

    labels = {name: label(name) for name in ("main.zr", "plugin_reads_secret.zr", "plugin_reads_undeclared.zr")}
    main_label = labels["main.zr"][1]
    label_ok = (main_label is not None and main_label["clean"]
                and {u["resource"]: u["fields"] for u in main_label["output"]} ==
                    {"courses_public": ["id", "price_minor", "title"], "request": ["max_price_minor", "search_text"]}
                and labels["plugin_reads_secret.zr"][1] is not None
                and any(u["resource"] == "secrets" and u["all"] for u in labels["plugin_reads_secret.zr"][1]["output"])
                and labels["plugin_reads_undeclared.zr"][1] is not None
                and not labels["plugin_reads_undeclared.zr"][1]["clean"])

    print()
    print(f"Runtime: {version}   PHP: {php or 'NOT FOUND (PHP side skipped; pass --php PATH)'}")
    if php_note:
        print(f"PHP note: {php_note}")
    print("-" * 78)
    print("LABELS (static analysis, before any program runs)")
    for name, (code, data) in labels.items():
        print(f"   {name:28} {describe(data)}")
    print(f"Labels as expected: {'yes' if label_ok else 'NO'}")
    print("-" * 78)
    print(f"Normal Zarya search returns workshops 301 and 302: {'yes' if sanity else 'NO'}")
    print("-" * 78)
    for r in results:
        print(r["attack"])
        print("   naive PHP : " + (r["php"] or "skipped"))
        print("   Zarya     : " + r["zarya"])
    print("-" * 78)
    (WORK / "results.json").write_text(json.dumps(
        {"runtime": version, "sanity": sanity, "label_ok": label_ok,
         "labels": {name: data for name, (code, data) in labels.items()}, "results": results},
        ensure_ascii=False, indent=2),
        encoding="utf-8")
    zarya_all = sanity and label_ok and all(r["zarya_ok"] for r in results)
    php_all = bool(php) and all(r["php_leaked"] for r in results)
    if zarya_all and php_all:
        print("PASS: all attacks leaked data from naive PHP and none leaked from Zarya.")
    elif zarya_all:
        print("PARTIAL: Zarya blocked every attack; the PHP side did not run or did not leak every time.")
        if php and php_cmd is None:
            print("Reason: " + php_note)
    else:
        print("FAIL: check the lines above.")
    print(f"Details: {WORK / 'results.json'}")
    return 0 if zarya_all else 1


if __name__ == "__main__":
    sys.exit(main())
