#!/usr/bin/env python3
"""CI duration watch (#160).

Runs after every main-branch completion of the `CI`/`CodeQL Advanced` workflows (see
.github/workflows/ci-duration-watch.yml). Compares each successful job's wall-clock duration
against the per-job budget in .github/ci-budgets.json, and -- only once a job is over budget on
two consecutive main runs of the same workflow, to avoid a single cold-cache run (e.g. right
after a Cargo.lock bump) raising a false alarm -- files or updates a GitHub issue labelled
`ci-slow`.

Requires `gh` on PATH, authenticated (GH_TOKEN) with `actions:read`/`issues:write` against the
repo named by GH_REPO or --repo.

Usage:
    ci_duration_watch.py --run-id <run-id> [--repo owner/name] [--dry-run]
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

BUDGETS_PATH = Path(__file__).resolve().parent.parent / "ci-budgets.json"
LABEL = "ci-slow"
CONSECUTIVE_REQUIRED = 2


def gh_api(path: str, repo: str, jq: str | None = None) -> object:
    cmd = ["gh", "api", "-H", "Accept: application/vnd.github+json", f"repos/{repo}/{path}"]
    if jq is not None:
        cmd += ["--jq", jq]
    out = subprocess.run(cmd, capture_output=True, text=True, check=True).stdout
    return out


def gh_api_json(path: str, repo: str) -> dict:
    return json.loads(gh_api(path, repo))


def parse_ts(s: str) -> datetime:
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def job_minutes(job: dict) -> float | None:
    if not job.get("started_at") or not job.get("completed_at"):
        return None
    return (parse_ts(job["completed_at"]) - parse_ts(job["started_at"])).total_seconds() / 60.0


def fetch_run(repo: str, run_id: int) -> dict:
    return gh_api_json(f"actions/runs/{run_id}", repo)


def fetch_jobs(repo: str, run_id: int) -> list[dict]:
    data = gh_api_json(f"actions/runs/{run_id}/jobs?per_page=100", repo)
    return data.get("jobs", [])


def previous_main_run(repo: str, workflow_id: int, before_run_id: int) -> dict | None:
    """Most recent successful main-branch run of the same workflow, strictly before before_run_id."""
    # No `event=push` filter: both watched workflows also run on `schedule` (see ci.yml's own
    # weekly Monday cron, added for #117/#127) -- pelt/decode's own jobs mostly only execute on
    # that schedule run except on a directly-touching PR, so filtering to push-only here would
    # mean the "previous" run for those jobs is almost never found and the 2-run streak check
    # can never confirm, silently defeating the watcher for the exact regression class (#154)
    # it exists to catch. `branch=main&status=success` is sufficient on its own to mean "the
    # previous successful run of this workflow on main", regardless of what triggered it.
    data = gh_api_json(
        f"actions/workflows/{workflow_id}/runs"
        f"?branch=main&status=success&per_page=30",
        repo,
    )
    for run in data.get("workflow_runs", []):
        if run["id"] < before_run_id:
            return run
    return None


def slowest_steps(job: dict, n: int = 3) -> list[tuple[str, float]]:
    out = []
    for step in job.get("steps", []) or []:
        if not step.get("started_at") or not step.get("completed_at"):
            continue
        mins = (parse_ts(step["completed_at"]) - parse_ts(step["started_at"])).total_seconds() / 60.0
        out.append((step["name"], mins))
    out.sort(key=lambda t: -t[1])
    return out[:n]


def find_open_issue(repo: str, title_substr: str) -> dict | None:
    # Listed directly (no --search): GitHub's search index has propagation lag, so a run that
    # searched for an issue another run just created could miss it and file a duplicate. Listing
    # every open ci-slow issue and matching titles locally in Python has no such lag. --limit
    # 1000 (gh paginates transparently past its ~30-100/page API default) covers this repo's
    # realistically bounded population many times over -- each distinct (job, workflow) pair
    # gets at most one open issue ever, so the count tracks job count (~15-20), not run count.
    data = json.loads(
        subprocess.run(
            [
                "gh", "issue", "list", "-R", repo,
                "--label", LABEL, "--state", "open",
                "--json", "number,title", "--limit", "1000",
            ],
            capture_output=True, text=True, check=True,
        ).stdout
    )
    for issue in data:
        if title_substr in issue["title"]:
            return issue
    return None


def ensure_label(repo: str, dry_run: bool) -> None:
    if dry_run:
        return
    subprocess.run(
        ["gh", "label", "create", LABEL, "--repo", repo, "--color", "d93f0b",
         "--description", "A CI job is over its .github/ci-budgets.json budget", "--force"],
        capture_output=True, text=True,
    )


def file_or_comment(
    repo: str, workflow_name: str, job_name: str, minutes: float, budget: float,
    prev_minutes: float, run_url: str, prev_run_url: str, steps: list[tuple[str, float]],
    dry_run: bool,
) -> None:
    title = f"CI slow: {job_name} over budget ({minutes:.0f}m > {budget:.0f}m)"
    body_lines = [
        f"`{job_name}` in workflow `{workflow_name}` has been over its budget of {budget:.0f}m "
        f"on {CONSECUTIVE_REQUIRED} consecutive main-branch runs:",
        "",
        f"- {run_url}: {minutes:.1f}m",
        f"- {prev_run_url}: {prev_minutes:.1f}m",
        "",
        "Slowest steps in the most recent run:",
    ]
    for name, mins in steps:
        body_lines.append(f"- {name}: {mins:.1f}m")
    body_lines += [
        "",
        "Budget is set in `.github/ci-budgets.json`. If this slowdown is expected and accepted, "
        "raise the budget there in the fixing PR instead of just closing this issue.",
    ]
    body = "\n".join(body_lines)

    existing = find_open_issue(repo, f"CI slow: {job_name} over budget")
    if existing:
        print(f"Updating existing issue #{existing['number']}: {title}")
        if not dry_run:
            subprocess.run(
                ["gh", "issue", "comment", str(existing["number"]), "-R", repo, "--body", body],
                check=True,
            )
        return

    print(f"Filing new issue: {title}")
    if not dry_run:
        subprocess.run(
            ["gh", "issue", "create", "-R", repo, "--title", title, "--label", LABEL, "--body", body],
            check=True,
        )


def file_missing_budget(repo: str, workflow_name: str, job_name: str, dry_run: bool) -> None:
    title = f"CI: add a budget for {job_name} ({workflow_name})"
    existing = find_open_issue(repo, f"add a budget for {job_name}")
    body = (
        f"`{job_name}` ran in workflow `{workflow_name}` but has no entry in "
        f"`.github/ci-budgets.json`, so ci-duration-watch can't tell if it's slow. Add one."
    )
    if existing:
        return
    print(f"Filing new issue: {title}")
    if not dry_run:
        subprocess.run(
            ["gh", "issue", "create", "-R", repo, "--title", title, "--label", LABEL, "--body", body],
            check=True,
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--repo", default=None, help="owner/name; defaults to $GH_REPO")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    import os
    repo = args.repo or os.environ.get("GH_REPO")
    if not repo:
        print("error: --repo or GH_REPO required", file=sys.stderr)
        return 2

    budgets_all = json.loads(BUDGETS_PATH.read_text())

    run = fetch_run(repo, args.run_id)
    workflow_name = run["name"]
    workflow_id = run["workflow_id"]
    budgets = budgets_all.get(workflow_name)
    if budgets is None:
        print(f"No budgets configured for workflow '{workflow_name}', nothing to check.")
        return 0
    budgets = {k: v for k, v in budgets.items() if not k.startswith("_")}

    jobs = fetch_jobs(repo, args.run_id)
    ensure_label(repo, args.dry_run)

    prev_run = None
    prev_jobs_by_name: dict[str, dict] = {}

    for job in jobs:
        if job.get("conclusion") != "success":
            continue
        name = job["name"]
        minutes = job_minutes(job)
        if minutes is None:
            continue

        if name not in budgets:
            file_missing_budget(repo, workflow_name, name, args.dry_run)
            continue

        budget = budgets[name]
        if minutes <= budget:
            continue

        if prev_run is None:
            prev_run = previous_main_run(repo, workflow_id, args.run_id)
            if prev_run is not None:
                prev_jobs_by_name = {j["name"]: j for j in fetch_jobs(repo, prev_run["id"])}

        if prev_run is None:
            print(f"{name}: over budget ({minutes:.1f}m > {budget}m) but no prior main run to "
                  f"confirm a 2-run streak -- skipping for now.")
            continue

        prev_job = prev_jobs_by_name.get(name)
        prev_minutes = job_minutes(prev_job) if prev_job else None
        if prev_minutes is None or prev_minutes <= budget:
            print(f"{name}: over budget this run ({minutes:.1f}m > {budget}m) but not last run "
                  f"({prev_minutes}m) -- not yet a 2-run streak, skipping.")
            continue

        file_or_comment(
            repo, workflow_name, name, minutes, budget, prev_minutes,
            run["html_url"], prev_run["html_url"], slowest_steps(job), args.dry_run,
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
