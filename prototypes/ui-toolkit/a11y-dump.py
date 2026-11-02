"""Dump the AT-SPI accessibility tree of running applications (screen reader view)."""
import sys
import pyatspi

def walk(node, depth, out, limit=400):
    if len(out) >= limit or depth > 25:
        return
    try:
        role = node.getRoleName()
        name = node.name or ""
    except Exception:
        return
    out.append("  " * depth + f"{role}: {name}")
    for i in range(node.childCount):
        try:
            walk(node.getChildAtIndex(i), depth + 1, out, limit)
        except Exception:
            pass

desktop = pyatspi.Registry.getDesktop(0)
for i in range(desktop.childCount):
    app = desktop.getChildAtIndex(i)
    if app is None:
        continue
    out = []
    walk(app, 0, out)
    print("\n".join(out))
    roles = [l.strip().split(":")[0] for l in out]
    print(f"-- {app.name}: {len(out)} nodes, buttons={roles.count('push button')}, page tabs={roles.count('page tab')}")
