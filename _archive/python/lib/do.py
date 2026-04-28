# do.py — Context gathering and response parsing for `boi do`.
#
# Functions:
#   gather_context()     — Run boi commands to collect system state
#   build_prompt()       — Build the LLM prompt from template + context
#   parse_response()     — Extract and validate JSON from LLM output
#   classify_destructive() — Safety-net check for destructive commands

import json
import os
import re
import subprocess
from pathlib import Path

BOI_DIR = Path.home() / "boi"
PROJECTS_DIR = Path.home() / ".boi" / "projects"
TEMPLATE_PATH = BOI_DIR / "templates" / "do-prompt.md"

# Queue ID pattern: q-NNN
_QUEUE_ID_RE = re.compile(r"\bq-\d+\b")

# Commands considered destructive
_DESTRUCTIVE_KEYWORDS = frozenset(
    [
        "cancel",
        "stop",
        "purge",
        "delete",
        "skip",
        "next",
        "block",
        "edit",
        "dispatch",
    ]
)


def _run_boi(args: list[str], timeout: int = 15) -> str:
    """Run a boi subcommand and return stdout. Returns empty string on failure."""
    boi_sh = str(BOI_DIR / "boi.sh")
    try:
        result = subprocess.run(
            [boi_sh] + args,
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=str(BOI_DIR),
        )
        return result.stdout.strip()
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
        return ""


def gather_context(user_input: str) -> dict:
    """Run boi commands to collect current system state.

    Returns a dict with keys: status, queue, workers, projects, spec.
    Values are raw JSON strings (or empty string if unavailable).
    """
    context = {
        "status": _run_boi(["status", "--json"]),
        "queue": _run_boi(["queue", "--json"]),
        "workers": _run_boi(["workers", "--json"]),
        "projects": "",
        "spec": "",
    }

    # Gather project list if projects directory exists
    if PROJECTS_DIR.is_dir():
        context["projects"] = _run_boi(["project", "list", "--json"])

    # If user mentions a queue ID, fetch its spec
    match = _QUEUE_ID_RE.search(user_input)
    if match:
        queue_id = match.group(0)
        context["spec"] = _run_boi(["spec", queue_id, "--json"])

    return context


def build_prompt(user_input: str, context: dict) -> str:
    """Build the full LLM prompt from the template and gathered context.

    Substitutes {{BOI_STATUS}}, {{BOI_QUEUE}}, {{BOI_WORKERS}},
    {{BOI_PROJECTS}}, {{BOI_SPEC}}, {{USER_INPUT}} placeholders.
    """
    if not TEMPLATE_PATH.is_file():
        raise FileNotFoundError(f"Template not found: {TEMPLATE_PATH}")

    template = TEMPLATE_PATH.read_text(encoding="utf-8")

    replacements = {
        "{{BOI_STATUS}}": context.get("status", ""),
        "{{BOI_QUEUE}}": context.get("queue", ""),
        "{{BOI_WORKERS}}": context.get("workers", ""),
        "{{BOI_PROJECTS}}": context.get("projects", ""),
        "{{BOI_SPEC}}": context.get("spec", ""),
        "{{USER_INPUT}}": user_input,
    }

    for placeholder, value in replacements.items():
        template = template.replace(placeholder, value)

    return template


def parse_response(response_text: str) -> dict:
    """Extract and validate JSON from Claude's response.

    Handles responses wrapped in markdown code blocks (```json ... ```).
    Validates required fields: commands (list[str]), explanation (str), destructive (bool).

    Returns the parsed dict.
    Raises ValueError on invalid or missing response.
    """
    text = response_text.strip()

    # Try to extract JSON from markdown code block
    code_block_match = re.search(
        r"```(?:json)?\s*\n(.*?)\n\s*```",
        text,
        re.DOTALL,
    )
    if code_block_match:
        text = code_block_match.group(1).strip()
    else:
        # Try to find raw JSON object
        brace_match = re.search(r"\{.*\}", text, re.DOTALL)
        if brace_match:
            text = brace_match.group(0)

    try:
        data = json.loads(text)
    except json.JSONDecodeError as e:
        raise ValueError(f"Failed to parse JSON from response: {e}")

    if not isinstance(data, dict):
        raise ValueError(f"Expected JSON object, got {type(data).__name__}")

    # Validate required fields
    if "commands" not in data:
        raise ValueError("Missing required field: commands")
    if not isinstance(data["commands"], list):
        raise ValueError("'commands' must be a list")
    for i, cmd in enumerate(data["commands"]):
        if not isinstance(cmd, str):
            raise ValueError(
                f"commands[{i}] must be a string, got {type(cmd).__name__}"
            )

    if "explanation" not in data:
        raise ValueError("Missing required field: explanation")
    if not isinstance(data["explanation"], str):
        raise ValueError("'explanation' must be a string")

    if "destructive" not in data:
        raise ValueError("Missing required field: destructive")
    if not isinstance(data["destructive"], bool):
        raise ValueError("'destructive' must be a boolean")

    return data


def classify_destructive(commands: list[str]) -> bool:
    """Safety-net check: classify commands as destructive based on keywords.

    Returns True if any command contains a destructive keyword.
    This overrides the LLM's classification as a safety measure.
    """
    for cmd in commands:
        # Normalize to lowercase for matching
        lower = cmd.lower()
        for keyword in _DESTRUCTIVE_KEYWORDS:
            # Match keyword as a word boundary (not substring of another word)
            if re.search(r"\b" + re.escape(keyword) + r"\b", lower):
                return True
    return False
