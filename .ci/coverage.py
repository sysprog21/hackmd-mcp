#!/usr/bin/env python3
"""Merge source-line coverage across all shipped platforms and enforce the floor."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys

PLATFORMS = ("Linux", "macOS", "Windows")
FLOOR = ".ci/coverage-floor"
# Mask Rust literals/comments before counting delimiters, preserving line numbers.
TOKENS = re.compile(
    r'r(#+)?".*?"(?(1)\1)|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\'|//[^\n]*|/\*.*?\*/',
    re.DOTALL,
)


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def excluded_lines(source):
    clean = TOKENS.sub(lambda match: "".join("\n" if c == "\n" else " " for c in match[0]), source)
    lines = clean.splitlines()
    excluded = set()
    i = 0
    while i < len(lines):
        if not lines[i].lstrip().startswith("#["):
            i += 1
            continue
        start = i
        depth = 0
        attribute = []
        while i < len(lines):
            attribute.append(lines[i])
            for offset, char in enumerate(lines[i]):
                depth += (char == "[") - (char == "]")
                if char == "]" and depth == 0:
                    if lines[i][offset + 1:].strip():
                        raise ValueError(f"attribute shares a line with source code at line {i + 1}")
                    break
            excluded.add(i + 1)
            i += 1
            if depth == 0:
                break
        if depth != 0:
            raise ValueError(f"unclosed attribute at line {start + 1}")
        if not re.search(r"cfg\s*\(\s*test\s*\)", "\n".join(attribute)):
            continue
        # A cfg(test) item can be a module, helper method, field or local block.
        braces = parens = brackets = angles = 0
        block = arm = False
        while i < len(lines):
            line = lines[i]
            excluded.add(i + 1)
            i += 1
            braces += line.count("{") - line.count("}")
            parens += line.count("(") - line.count(")")
            brackets += line.count("[") - line.count("]")
            types = line.replace("->", "").replace("=>", "")
            angles += types.count("<") - types.count(">")
            block |= "{" in line
            # A match arm ends at its comma; its guard may compare with < or >.
            arm |= "=>" in line
            if block and braces == 0:
                break
            if not block and parens == brackets == 0:
                if line.rstrip().endswith(";"):
                    break
                if line.rstrip().endswith(","):
                    # A comparison reads as a closing angle; guessing where the
                    # item ends could hide the production lines after it.
                    if arm or angles == 0:
                        break
                    if angles < 0:
                        raise ValueError(f"cannot find the end of the cfg(test) item at line {start + 1}")
    return excluded


def merge_reports(reports, sources):
    result = {}
    excluded = {name: excluded_lines(text) for name, text in sources.items()}
    lengths = {name: len(text.splitlines()) for name, text in sources.items()}
    for report in reports:
        name = None
        records = 0
        measured = 0
        for row in report.read_text().splitlines():
            if row.startswith("SF:"):
                path = row[3:].replace("\\", "/")
                name = path if path in sources else None
                records += 1
            elif row.startswith("DA:") and name:
                number, hits, *_ = row[3:].split(",")
                number, hits = int(number), int(hits)
                if number < 1 or number > lengths[name] or hits < 0:
                    raise ValueError(f"invalid line record in {report}: {row}")
                if number not in excluded[name]:
                    key = (name, number)
                    result[key] = result.get(key, False) or hits > 0
                    measured += 1
            elif row == "end_of_record":
                name = None
        if not records:
            raise ValueError(f"empty coverage report: {report}")
        if not measured:
            raise ValueError(f"report contains no production source lines: {report}")
    if not result:
        raise ValueError("reports contain no handwritten production source lines")
    return result


def changed_lines(diff):
    result = set()
    name = None
    for row in diff.splitlines():
        if row.startswith("+++ b/"):
            # git ends the name with a tab when it contains a space.
            name = row[6:].removesuffix("\t")
        elif row == "+++ /dev/null":
            name = None
        elif row.startswith("+++ "):
            raise ValueError(f"cannot read the changed file from {row!r}")
        elif name and (match := re.match(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", row)):
            start = int(match[1])
            count = int(match[2]) if match[2] is not None else 1
            result.update((name, line) for line in range(start, start + count))
    return result


def read_floor(text):
    floor = int(text.strip())
    if not 0 <= floor <= 100:
        raise ValueError("coverage floor must be an integer between 0 and 100")
    return floor


def check_floor(current, previous, covered, measured):
    if previous is not None and current < previous:
        raise ValueError(f"coverage floor cannot decrease: {previous}% -> {current}%")
    if measured and covered * 100 < current * measured:
        raise ValueError(f"changed-line coverage {covered}/{measured} is below the {current}% floor")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", type=Path)
    parser.add_argument("--base", help="verified comparison commit; omitted for report-only runs")
    args = parser.parse_args()
    reports = [args.reports / f"coverage-{platform}" / "lcov.info" for platform in PLATFORMS]
    if set(args.reports.rglob("lcov.info")) != set(reports):
        raise ValueError("coverage requires Linux, macOS and Windows lcov.info reports")
    sources = {
        name: Path(name).read_text()
        for name in git("ls-files", "-z", "src").split("\0")
        if name.endswith(".rs") and not name.startswith("src/fixture")
    }
    coverage = merge_reports(reports, sources)
    total = sum(coverage.values())
    summary = f"Source line coverage: {total}/{len(coverage)} ({total / len(coverage):.2%})\n"
    floor = read_floor(Path(FLOOR).read_text()) if Path(FLOOR).exists() else None
    failure = None
    if args.base:
        # Pass a full SHA, never an option or a revision supplied by a PR file.
        if not re.fullmatch(r"[0-9a-f]{40}", args.base):
            raise ValueError("base must be a full commit SHA")
        # A move counts only the lines it edits; prefixes and color are pinned
        # so local git config cannot make the parser miss every file.
        diff = git("-c", "core.quotePath=false", "diff", "--no-ext-diff", "--no-textconv", "--no-color",
                   "--find-renames", "--src-prefix=a/", "--dst-prefix=b/", "--unified=0",
                   args.base, "HEAD", "--", "src")
        measured = changed_lines(diff) & coverage.keys()
        covered = sum(coverage[line] for line in measured)
        summary += f"Changed source lines: {covered}/{len(measured)}; floor: {floor}%\n"
        previous = None
        if git("ls-tree", "--name-only", args.base, "--", FLOOR):
            previous = read_floor(git("show", f"{args.base}:{FLOOR}"))
        if previous is not None and floor is None:
            failure = "coverage floor cannot be removed"
        elif floor is not None:
            try:
                check_floor(floor, previous, covered, len(measured))
            except ValueError as error:
                failure = str(error)
        for name, number in sorted(measured):
            if not coverage[(name, number)]:
                print(f"::warning file={name},line={number}::Changed source line was not covered on any platform")
    else:
        summary += "Report only: no comparison commit.\n"
    print(summary, end="")
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a") as output:
            output.write(summary)
    if failure:
        raise ValueError(failure)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"coverage: {error}", file=sys.stderr)
        sys.exit(1)
