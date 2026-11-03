#!/usr/bin/env python3
"""Prints the slowest tests from nextest JUnit reports as a Markdown table (ADR-0032).

Usage: slowest-tests.py <title> <junit.xml>... Appends to $GITHUB_STEP_SUMMARY when it
is set, else prints to stdout. Never fails the job: timings are reported, not gated,
because a pass/fail check on wall-clock time would itself be flaky.
"""

import os
import sys
import xml.etree.ElementTree as ET

TOP = 15
# Fast-tier tests above this are listed as candidates for the slow tier or a fix.
FAST_TIER_TARGET_S = 1.0


def main() -> None:
    title, paths = sys.argv[1], sys.argv[2:]
    cases = []
    for path in paths:
        if not os.path.exists(path):
            continue
        for case in ET.parse(path).getroot().iter("testcase"):
            cases.append((float(case.get("time", "0")), case.get("classname", ""), case.get("name", "")))
    cases.sort(reverse=True)
    total = sum(t for t, _, _ in cases)
    lines = [
        f"### {title}",
        "",
        f"{len(cases)} tests, {total:.1f} s summed test time "
        f"({sum(1 for t, _, _ in cases if t > FAST_TIER_TARGET_S)} above {FAST_TIER_TARGET_S:.0f} s).",
        "",
        "| Time (s) | Binary | Test |",
        "| ---: | --- | --- |",
    ]
    lines += [f"| {t:.2f} | `{c}` | `{n}` |" for t, c, n in cases[:TOP]]
    text = "\n".join(lines) + "\n"
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(text)
    print(text)


if __name__ == "__main__":
    main()
