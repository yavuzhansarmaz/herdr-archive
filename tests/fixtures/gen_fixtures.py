"""Generate cross-compat fixtures with the REAL Python shelf code.

Run from the herdr-shelf checkout:
    python3 /path/to/gen_fixtures.py <out-dir>

Produces:
    python-claude-tab.json      record.json from shelf.archive.capture
    python-claude-tab.tree.json shelf.restore.build_tree output for it
    python-manual-muse.json     capture with session_overrides (manual muse)
    python-manual-muse.tree.json  its build_tree output

The Rust cross-compat test loads each record.json and asserts its own
build_tree output equals the .tree.json file (semantic JSON equality).
"""

import json
import sys
from datetime import datetime, timezone

sys.path.insert(0, "/home/yohansin/Desktop/PP/herdr-shelf")

from shelf import agents, archive, restore  # noqa: E402

NOW = datetime(2026, 9, 29, 17, 0, tzinfo=timezone.utc)


class StubClient:
    def __init__(self, procs):
        self.procs = procs

    def call(self, method, params=None):
        if method == "layout.export":
            return {"layout": {"root": LAYOUT, "focused_pane_id": "p1", "zoomed": False}}
        if method == "workspace.list":
            return {"workspaces": [{"workspace_id": "w1", "label": "backend"}]}
        if method == "pane.process_info":
            return {"process_info": {"foreground_processes": self.procs.get(params["pane_id"], [])}}
        raise AssertionError(method)


LAYOUT = {
    "type": "split",
    "direction": "horizontal",
    "ratio": 0.5,
    "first": {"type": "pane", "pane_id": "p1", "cwd": "/srv/a", "label": "agent"},
    "second": {
        "type": "split",
        "direction": "vertical",
        "ratio": 0.5,
        "first": {"type": "pane", "pane_id": "p2"},
        "second": {"type": "pane", "pane_id": "p3", "cwd": "/srv/other"},
    },
}

TAB = {"tab_id": "t1", "workspace_id": "w1", "label": "api-work", "focused": False}
PANES = [
    {"pane_id": "p1", "tab_id": "t1", "cwd": "/srv/a", "agent": "claude",
     "agent_status": "idle", "terminal_id": "term-1",
     "agent_session": {"agent": "claude", "kind": "id", "value": "SESS-AAAA", "source": "herdr:claude"}},
    {"pane_id": "p2", "tab_id": "t1", "cwd": "/srv/a", "agent": "muse",
     "agent_status": "idle", "terminal_id": "term-2"},  # no reported session
    {"pane_id": "p3", "tab_id": "t1", "cwd": "/srv/other", "terminal_id": "term-3"},  # plain shell
]
PROCS = {
    "p1": [{"name": "claude", "argv": ["/usr/bin/claude", "--model", "opus"]}],
    "p2": [{"name": "muse", "argv": ["muse"]}],
}


def activity_of(agent, value, terminal_id=None):
    return datetime(2026, 9, 20, 12, 0, tzinfo=timezone.utc)


def main(out):
    import os

    os.makedirs(out, exist_ok=True)
    table = agents.table()

    # 1. plain capture: claude agent pane, muse-without-session shell pane, shell pane
    record, files = archive.capture(
        StubClient(PROCS), TAB, PANES, table, activity_of, False, NOW, "default")
    assert files == []
    with open(f"{out}/python-claude-tab.json", "w") as f:
        json.dump(record, f, indent=2, sort_keys=True)
        f.write("\n")
    tree = restore.build_tree(record["layout"]["root"], record["panes"], table)
    with open(f"{out}/python-claude-tab.tree.json", "w") as f:
        json.dump(tree, f, indent=2, sort_keys=True)
        f.write("\n")

    # 2. manual muse override
    record2, _ = archive.capture(
        StubClient(PROCS), TAB, PANES, table, activity_of, False, NOW, "default",
        session_overrides={"p2": "MUSE-UUID-1"})
    with open(f"{out}/python-manual-muse.json", "w") as f:
        json.dump(record2, f, indent=2, sort_keys=True)
        f.write("\n")
    tree2 = restore.build_tree(record2["layout"]["root"], record2["panes"], table)
    with open(f"{out}/python-manual-muse.tree.json", "w") as f:
        json.dump(tree2, f, indent=2, sort_keys=True)
        f.write("\n")
    print("wrote fixtures to", out)


if __name__ == "__main__":
    main(sys.argv[1])
