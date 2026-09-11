#!/usr/bin/env python3
"""One-off Notion archive: dump every configured database to JSON + Markdown.

Run it where the Notion credentials live (the Pi):

    set -a; . /opt/optimimer/optimimer.env; set +a
    python3 notion_export.py --out /opt/optimimer/notion-archive

Reads NOTION_TOKEN and the *_DB_ID variables from the environment (or pass
--env-file). For each database it writes:

    <out>/<name>.json          every page, raw Notion payload (lossless)
    <out>/<name>/<slug>.md     one Markdown file per page: frontmatter with the
                               flattened properties, body from the page blocks

Standard library only, so it runs on a stock Raspberry Pi OS python3.
"""
import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

API = "https://api.notion.com/v1"
VERSION = "2022-06-28"
DBS = {
    "notes": "NOTES_DB_ID",
    "tasks": "LIFEOS_DB_ID",
    "finances": "FINANCES_DB_ID",
    "projects": "PROJECTS_DB_ID",
    "topics": "TOPICS_DB_ID",
    "areas": "AREAS_DB_ID",
}


def load_env_file(path):
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            os.environ.setdefault(k.strip(), v.strip().strip('"').strip("'"))


def call(token, method, path, body=None, retries=5):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{API}{path}", data=data, method=method)
    req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Notion-Version", VERSION)
    req.add_header("Content-Type", "application/json")
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code == 429 or e.code >= 500:
                wait = float(e.headers.get("Retry-After", 2 ** attempt))
                time.sleep(wait)
                continue
            raise SystemExit(f"Notion {e.code} on {path}: {e.read().decode()[:300]}")
    raise SystemExit(f"gave up on {path}")


def query_all(token, db_id):
    pages, cursor = [], None
    while True:
        body = {"page_size": 100}
        if cursor:
            body["start_cursor"] = cursor
        r = call(token, "POST", f"/databases/{db_id}/query", body)
        pages.extend(r.get("results", []))
        if not r.get("has_more"):
            return pages
        cursor = r.get("next_cursor")
        time.sleep(0.34)  # ~3 req/s budget


def rich(rt):
    return "".join(t.get("plain_text", "") for t in rt or [])


def prop_value(p):
    t = p.get("type")
    v = p.get(t)
    if t in ("title", "rich_text"):
        return rich(v)
    if t in ("number", "checkbox", "url", "email", "phone_number"):
        return v
    if t in ("select", "status"):
        return v["name"] if v else None
    if t == "multi_select":
        return [x["name"] for x in v]
    if t == "date":
        return None if not v else (v["start"] if not v.get("end") else f'{v["start"]} → {v["end"]}')
    if t == "relation":
        return [x["id"] for x in v]
    if t == "people":
        return [x.get("name") or x["id"] for x in v]
    if t == "files":
        return [x.get("name") for x in v]
    if t in ("created_time", "last_edited_time"):
        return v
    if t == "formula":
        return v.get(v.get("type"))
    if t == "rollup":
        return v.get(v.get("type"))
    return v


def blocks(token, block_id, depth=0):
    """Yield markdown lines for a block's children, recursively."""
    cursor, out = None, []
    while True:
        q = f"/blocks/{block_id}/children?page_size=100" + (f"&start_cursor={cursor}" if cursor else "")
        r = call(token, "GET", q)
        for b in r.get("results", []):
            out.extend(block_md(token, b, depth))
        if not r.get("has_more"):
            break
        cursor = r.get("next_cursor")
    time.sleep(0.34)
    return out


def block_md(token, b, depth):
    t = b["type"]
    d = b.get(t, {})
    ind = "  " * depth
    text = rich(d.get("rich_text"))
    line = None
    if t == "paragraph":
        line = text
    elif t.startswith("heading_"):
        line = "#" * int(t[-1]) + " " + text
    elif t == "bulleted_list_item":
        line = f"{ind}- {text}"
    elif t == "numbered_list_item":
        line = f"{ind}1. {text}"
    elif t == "to_do":
        line = f"{ind}- [{'x' if d.get('checked') else ' '}] {text}"
    elif t == "quote":
        line = "> " + text
    elif t == "callout":
        line = "> " + text
    elif t == "code":
        line = f"```{d.get('language', '')}\n{text}\n```"
    elif t == "divider":
        line = "---"
    elif t == "toggle":
        line = f"{ind}- {text}"
    elif t == "child_page":
        line = f"[[{d.get('title', '')}]]"
    elif t in ("image", "file", "pdf", "video", "bookmark", "embed"):
        src = d.get("external", {}).get("url") or d.get("file", {}).get("url") or d.get("url", "")
        line = f"[{t}]({src})"
    elif t == "table":
        line = None  # rows come as children
    elif t == "table_row":
        line = "| " + " | ".join(rich(c) for c in d.get("cells", [])) + " |"
    else:
        line = f"<!-- {t} -->" + (f" {text}" if text else "")
    lines = [line] if line is not None else []
    if b.get("has_children"):
        lines.extend(blocks(token, b["id"], depth + 1 if t in ("bulleted_list_item", "numbered_list_item", "to_do", "toggle") else depth))
    return lines


def slug(s, n=60):
    s = re.sub(r"[^\w\s-]", "", s, flags=re.U).strip().lower()
    s = re.sub(r"[\s_-]+", "-", s)
    return s[:n].strip("-") or "untitled"


def yaml_val(v):
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, list):
        return "[" + ", ".join(yaml_val(x) for x in v) + "]"
    return json.dumps(str(v), ensure_ascii=False)


def export_db(token, name, db_id, out, with_body):
    pages = query_all(token, db_id)
    with open(os.path.join(out, f"{name}.json"), "w") as f:
        json.dump(pages, f, ensure_ascii=False, indent=1)
    d = os.path.join(out, name)
    os.makedirs(d, exist_ok=True)
    seen = set()
    for i, p in enumerate(pages, 1):
        props = {k: prop_value(v) for k, v in p.get("properties", {}).items()}
        title = next((v for k, v in p["properties"].items() if v["type"] == "title"), None)
        title = rich(title["title"]) if title else ""
        created = (p.get("created_time") or "")[:10]
        base = f"{created}-{slug(title)}" if created else slug(title)
        fn = base
        k = 2
        while fn in seen:
            fn = f"{base}-{k}"
            k += 1
        seen.add(fn)
        fm = ["---", f"title: {yaml_val(title)}", f"notion_id: {yaml_val(p['id'])}", f"notion_url: {yaml_val(p.get('url'))}",
              f"created: {yaml_val(p.get('created_time'))}", f"edited: {yaml_val(p.get('last_edited_time'))}"]
        for key, val in props.items():
            if key.lower() in ("name", "title"):
                continue
            fm.append(f"{slug(key, 40).replace('-', '_')}: {yaml_val(val)}")
        fm.append("---")
        body = blocks(token, p["id"]) if with_body else []
        with open(os.path.join(d, fn + ".md"), "w") as f:
            f.write("\n".join(fm) + "\n\n" + (f"# {title}\n\n" if title else "") + "\n".join(body) + "\n")
        print(f"  [{name}] {i}/{len(pages)} {fn}", file=sys.stderr)
    return len(pages)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", required=True, help="archive directory (created)")
    ap.add_argument("--env-file", help="read NOTION_TOKEN / *_DB_ID from this file")
    ap.add_argument("--no-body", action="store_true", help="properties only, skip page blocks (much faster)")
    ap.add_argument("--only", help="comma-separated subset of: " + ",".join(DBS))
    a = ap.parse_args()
    if a.env_file:
        load_env_file(a.env_file)
    token = os.environ.get("NOTION_TOKEN", "")
    if not token:
        raise SystemExit("NOTION_TOKEN is not set")
    os.makedirs(a.out, exist_ok=True)
    only = set(a.only.split(",")) if a.only else set(DBS)
    total = {}
    for name, var in DBS.items():
        db_id = os.environ.get(var, "")
        if name not in only or not db_id:
            print(f"skip {name} ({var} unset)" if name in only else f"skip {name}", file=sys.stderr)
            continue
        print(f"exporting {name} …", file=sys.stderr)
        # Finances/relations tables are property-only rows; bodies are empty and cost a request each.
        with_body = not a.no_body and name in ("notes", "tasks", "projects")
        total[name] = export_db(token, name, db_id, a.out, with_body)
    with open(os.path.join(a.out, "README.md"), "w") as f:
        f.write("# Notion archive\n\nExported " + time.strftime("%Y-%m-%d %H:%M") + " by scripts/notion_export.py.\n\n"
                + "\n".join(f"- {k}: {v} pages" for k, v in total.items()) + "\n")
    print("done:", total, file=sys.stderr)


if __name__ == "__main__":
    main()
