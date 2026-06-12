#!/usr/bin/env python3
"""Guard against the CI and release build matrices drifting apart (issue #51).

The build matrix is deliberately maintained in two workflows — ci.yml's
`build` job compiles every shipped target on PRs, release.yml's `build` job
ships the same set — and is mirrored by `src/update.rs::asset_for` and the
README's asset table. The two YAML copies are line-for-line duplicates by
design (review §3.10 judged a reusable-workflow unification not worth it
yet); this script fails CI when they stop matching.

Run from the repository root, locally or in CI:

    python3 .github/scripts/check_matrix_sync.py
"""

import json
import sys

import yaml

CI = ".github/workflows/ci.yml"
RELEASE = ".github/workflows/release.yml"


def matrix_entries(path: str) -> list[dict]:
    """The `jobs.build.strategy.matrix.include` list of a workflow file."""
    with open(path, encoding="utf-8") as f:
        workflow = yaml.safe_load(f)
    try:
        return workflow["jobs"]["build"]["strategy"]["matrix"]["include"]
    except (KeyError, TypeError) as e:
        sys.exit(f"{path}: cannot find jobs.build.strategy.matrix.include ({e})")


def by_asset(entries: list[dict], path: str) -> dict[str, dict]:
    keyed = {}
    for entry in entries:
        asset = entry.get("asset")
        if not asset:
            sys.exit(f"{path}: matrix entry without an `asset` key: {entry}")
        if asset in keyed:
            sys.exit(f"{path}: duplicate matrix entry for asset {asset}")
        keyed[asset] = entry
    return keyed


def main() -> int:
    ci = by_asset(matrix_entries(CI), CI)
    release = by_asset(matrix_entries(RELEASE), RELEASE)

    problems = []
    for asset in sorted(set(ci) | set(release)):
        if asset not in release:
            problems.append(f"  {asset}: in {CI} but missing from {RELEASE}")
        elif asset not in ci:
            problems.append(f"  {asset}: in {RELEASE} but missing from {CI}")
        elif ci[asset] != release[asset]:
            problems.append(
                f"  {asset}: definitions differ\n"
                f"    {CI}:      {json.dumps(ci[asset], sort_keys=True)}\n"
                f"    {RELEASE}: {json.dumps(release[asset], sort_keys=True)}"
            )

    if problems:
        print(
            f"build matrices in {CI} and {RELEASE} have drifted apart;\n"
            "they must stay identical (and src/update.rs::asset_for plus the\n"
            "README asset table must mirror them — see issue #51):\n"
        )
        print("\n".join(problems))
        return 1

    print(f"build matrices in sync: {len(ci)} assets in {CI} and {RELEASE}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
