#!/usr/bin/env python3
"""hamlet roadmap runner.

Feeds every prompt in roadmap/sprints/ chronologically to a headless pi
worker (`pi --print`), one session per step, waiting for each to complete
before starting the next. Enforces the roadmap feeding protocol:

  * prompt = role preamble (roadmap/ROLES.md) + prompt file verbatim
  * each worker runs with cwd = repo root so it can read CONVENTIONS.md,
    SPEC.md, and the previous HANDOFF.md itself (the prompts say so)
  * step outputs are logged under logs/roadmap/
  * progress is checkpointed in roadmap/.runner-state.json (resumable)
  * gate steps (qa/ci/audit) that report FAIL halt the run — per protocol,
    a failed gate means re-feeding the failing build prompt with findings,
    which this runner surfaces instead of silently skipping

Usage:
  python3 run_roadmap.py                 # run everything (resumable)
  python3 run_roadmap.py --dry-run       # show the plan, feed nothing
  python3 run_roadmap.py --from 03/02    # resume at sprint 03 step 02
  python3 run_roadmap.py --only 05       # only sprint 05
  python3 run_roadmap.py --model sonnet:high --timeout 3600
  python3 run_roadmap.py --retries 1     # one automatic retry per step
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parent
ROADMAP = REPO / "roadmap"
SPRINTS = ROADMAP / "sprints"
ROLES = ROADMAP / "ROLES.md"
STATE_FILE = ROADMAP / ".runner-state.json"
LOG_DIR = REPO / "logs" / "roadmap"

FRONTMATTER_RE = re.compile(r"^---\s*\n(.*?)\n---\s*\n", re.DOTALL)
GATE_FAIL_RE = re.compile(r"\b(?:GATE|AUDIT)\s+[0-9A-Za-z/:\- ]*:\s*FAIL\b", re.IGNORECASE)
GATE_PASS_RE = re.compile(r"\b(?:GATE|AUDIT)\s+[0-9A-Za-z/:\- ]*:\s*(?:PASS|CLEAN)\b", re.IGNORECASE)
GATE_ROLES = {"qa-engineer", "ci-agent", "auditor"}


@dataclass
class Step:
    sprint: str          # e.g. "03-agent-engine"
    sprint_num: str      # e.g. "03"
    step_num: str        # e.g. "02"
    role: str
    path: Path
    body: str            # prompt file content (frontmatter stripped)

    @property
    def key(self) -> str:
        return f"{self.sprint_num}/{self.step_num}"

    @property
    def label(self) -> str:
        return f"{self.sprint_num}/{self.step_num}-{self.role}"


def parse_frontmatter(text: str) -> tuple[dict, str]:
    m = FRONTMATTER_RE.match(text)
    if not m:
        return {}, text
    meta = {}
    for line in m.group(1).splitlines():
        if ":" in line:
            k, v = line.split(":", 1)
            meta[k.strip()] = v.strip()
    return meta, text[m.end():]


def load_role_preambles() -> dict[str, str]:
    """Parse roadmap/ROLES.md into {role-name: preamble-text}."""
    text = ROLES.read_text(encoding="utf-8")
    roles: dict[str, str] = {}
    current: str | None = None
    buf: list[str] = []
    for line in text.splitlines():
        m = re.match(r"^##\s+ROLE:\s*(\S+)\s*$", line)
        if m:
            if current:
                roles[current] = "\n".join(buf).strip()
            current, buf = m.group(1), []
        elif current is not None:
            if line.strip() == "---":
                roles[current] = "\n".join(buf).strip()
                current, buf = None, []
            else:
                buf.append(line)
    if current:
        roles[current] = "\n".join(buf).strip()
    return roles


def discover_steps() -> list[Step]:
    steps: list[Step] = []
    for sprint_dir in sorted(p for p in SPRINTS.iterdir() if p.is_dir()):
        num = sprint_dir.name.split("-", 1)[0]
        for prompt in sorted(sprint_dir.glob("[0-9][0-9]-*.md")):
            text = prompt.read_text(encoding="utf-8")
            meta, body = parse_frontmatter(text)
            role = meta.get("role") or prompt.stem.split("-", 1)[1]
            steps.append(Step(
                sprint=sprint_dir.name,
                sprint_num=meta.get("sprint", num),
                step_num=meta.get("step", prompt.stem.split("-", 1)[0]),
                role=role,
                path=prompt,
                body=body.strip(),
            ))
    steps.sort(key=lambda s: (s.sprint, int(s.step_num)))
    return steps


def load_state() -> dict:
    if STATE_FILE.exists():
        return json.loads(STATE_FILE.read_text(encoding="utf-8"))
    return {"completed": [], "failed": None, "history": []}


def save_state(state: dict) -> None:
    STATE_FILE.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8")


def build_prompt(step: Step, preambles: dict[str, str]) -> str:
    preamble = preambles.get(step.role)
    if preamble is None:
        sys.exit(f"ERROR: no preamble for role '{step.role}' in {ROLES} (step {step.key})")
    return (
        f"{preamble}\n\n"
        "=== YOUR TASK PROMPT (verbatim) ===\n\n"
        f"{step.body}\n\n"
        "=== PROTOCOL REMINDERS ===\n"
        f"- Repository root: {REPO}\n"
        "- Read roadmap/CONVENTIONS.md and SPEC.md sections cited above BEFORE acting.\n"
        f"- Read roadmap/sprints/{step.sprint}/HANDOFF.md (and OBJECTIVE.md) first if they exist.\n"
        "- When done, update that HANDOFF.md and make the commit(s) your prompt specifies.\n"
        "- If your prompt defines a gate verdict, end your final message with the exact\n"
        "  verdict line (e.g. 'GATE 03/05: PASS' or 'GATE 03/05: FAIL' + findings).\n"
    )


def run_pi(prompt: str, step: Step, args: argparse.Namespace) -> tuple[int, str]:
    cmd = [args.pi_bin, "--print",
           "--session-id", f"hamlet-roadmap-{step.sprint_num}-{step.step_num}"]
    if args.model:
        cmd += ["--model", args.model]
    cmd += args.extra_pi_args
    cmd.append(prompt)

    env = dict(os.environ)
    env.setdefault("HAMLET_ROADMAP_STEP", step.key)
    log_path = LOG_DIR / f"{step.sprint_num}-{step.step_num}-{step.role}.log"
    LOG_DIR.mkdir(parents=True, exist_ok=True)

    started = time.time()
    with open(log_path, "w", encoding="utf-8") as log:
        log.write(f"# step {step.key} role={step.role} prompt={step.path}\n")
        log.write(f"# cmd: pi --print --session-id hamlet-roadmap-"
                  f"{step.sprint_num}-{step.step_num} {' '.join(shlex.quote(a) for a in args.extra_pi_args)}\n")
        log.write(f"# started: {datetime.now(timezone.utc).isoformat()}\n\n")
        log.flush()
        try:
            proc = subprocess.run(
                cmd, cwd=str(REPO), env=env,
                stdout=log, stderr=subprocess.STDOUT,
                timeout=args.timeout, text=True,
            )
            rc = proc.returncode
        except subprocess.TimeoutExpired:
            log.write(f"\n# TIMEOUT after {args.timeout}s\n")
            rc = 124
    elapsed = time.time() - started
    output = log_path.read_text(encoding="utf-8", errors="replace")
    print(f"    finished rc={rc} in {elapsed:.0f}s -> {log_path.relative_to(REPO)}")
    return rc, output


def evaluate(step: Step, rc: int, output: str) -> tuple[bool, str]:
    if rc != 0:
        return False, f"pi exited with code {rc}"
    if step.role in GATE_ROLES:
        if GATE_FAIL_RE.search(output):
            return False, "gate reported FAIL"
        if not GATE_PASS_RE.search(output):
            # commit-agent gates use a precondition-stop pattern instead of a verdict line
            if re.search(r"precondition.*(not met|FAILED)|stop and (say so|report)", output, re.IGNORECASE):
                return False, "gate precondition not met (worker stopped)"
            return False, "no PASS/CLEAN verdict line found in gate output"
    return True, "ok"


def select_steps(steps: list[Step], args: argparse.Namespace) -> list[Step]:
    sel = steps
    if args.only:
        sel = [s for s in sel if s.sprint_num == args.only.zfill(2)]
    if args.from_step:
        m = re.match(r"(\d+)(?:/(\d+))?$", args.from_step)
        if not m:
            sys.exit("--from must look like 03 or 03/02")
        sp, st = int(m.group(1)), int(m.group(2) or 0)
        sel = [s for s in sel if (int(s.sprint_num), int(s.step_num)) >= (sp, st)]
    if args.to_step:
        m = re.match(r"(\d+)(?:/(\d+))?$", args.to_step)
        if not m:
            sys.exit("--to must look like 03 or 03/02")
        sp, st = int(m.group(1)), int(m.group(2) or 99)
        sel = [s for s in sel if (int(s.sprint_num), int(s.step_num)) <= (sp, st)]
    return sel


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pi-bin", default=os.environ.get("PI_BIN", "pi"))
    ap.add_argument("--model", default=os.environ.get("HAMLET_MODEL"),
                    help="pi --model pattern (default: pi's configured model)")
    ap.add_argument("--timeout", type=int, default=int(os.environ.get("HAMLET_STEP_TIMEOUT", "7200")),
                    help="seconds per step (default 7200)")
    ap.add_argument("--retries", type=int, default=0, help="auto-retries per failed step")
    ap.add_argument("--from", dest="from_step", help="start at SPRINT[/STEP], e.g. 03 or 03/02")
    ap.add_argument("--to", dest="to_step", help="stop after SPRINT[/STEP]")
    ap.add_argument("--only", help="run a single sprint, e.g. 05")
    ap.add_argument("--dry-run", action="store_true", help="print the plan, feed nothing")
    ap.add_argument("--force", action="store_true", help="re-run steps already marked completed")
    ap.add_argument("--continue-on-error", action="store_true",
                    help="keep feeding after a failure (NOT protocol-compliant; debugging only)")
    ap.add_argument("extra_pi_args", nargs="*", help="extra args passed through to pi (after --)")
    args = ap.parse_args()

    if not ROLES.exists():
        sys.exit(f"missing {ROLES}")

    preambles = load_role_preambles()
    steps = discover_steps()
    if not steps:
        sys.exit(f"no prompts found under {SPRINTS}")
    plan = select_steps(steps, args)
    state = load_state()
    completed = set(state["completed"])

    pending = [s for s in plan if args.force or s.key not in completed]

    print(f"hamlet roadmap runner — {len(steps)} prompts discovered, "
          f"{len(plan)} selected, {len(plan) - len(pending)} already completed, "
          f"{len(pending)} to run")
    for s in pending:
        print(f"  [ ] {s.key}  {s.role:<20} {s.path.relative_to(REPO)}")
    if args.dry_run:
        return 0
    if not pending:
        print("Nothing to do — roadmap complete (use --force to re-run).")
        return 0

    LOG_DIR.mkdir(parents=True, exist_ok=True)
    for i, step in enumerate(pending, 1):
        print(f"\n=== [{i}/{len(pending)}] step {step.key} ({step.role}) — {step.path.name} ===")
        prompt = build_prompt(step, preambles)
        ok, reason = False, ""
        for attempt in range(args.retries + 1):
            if attempt:
                print(f"    retry {attempt}/{args.retries} after failure: {reason}")
            rc, output = run_pi(prompt, step, args)
            ok, reason = evaluate(step, rc, output)
            if ok:
                break
        state["history"].append({
            "step": step.key, "role": step.role, "prompt": str(step.path.relative_to(REPO)),
            "ok": ok, "reason": reason, "at": datetime.now(timezone.utc).isoformat(),
        })
        if ok:
            completed.add(step.key)
            state["completed"] = sorted(completed)
            state["failed"] = None
            save_state(state)
            print(f"    PASS {step.key} ({step.role})")
        else:
            state["failed"] = {"step": step.key, "reason": reason}
            save_state(state)
            print(f"\n!!! step {step.key} ({step.role}) FAILED: {reason}")
            print(f"    log: {LOG_DIR / f'{step.sprint_num}-{step.step_num}-{step.role}.log'}")
            if step.role in GATE_ROLES:
                print("    Gate failure: per roadmap/README.md, re-feed the failing BUILD prompt")
                print("    with the gate's findings appended — do not skip the gate.")
                print("    Then resume with: python3 run_roadmap.py --from "
                      f"{step.sprint_num}/{int(step.step_num) - 1:02d} --force")
            else:
                print(f"    Resume after fixing with: python3 run_roadmap.py --from {step.key} --force")
            if not args.continue_on_error:
                return 1

    print(f"\nAll {len(pending)} steps completed. Roadmap runner finished at "
          f"{datetime.now(timezone.utc).isoformat()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
