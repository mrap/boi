#!/usr/bin/env python3
"""ollama_react_worker.py — Bounded ReAct loop for Ollama-backed BOI workers.

Reads a prompt file, sends it to a local Ollama model, and parses the
response for Action: / Observation: lines in a ReAct loop.  When the model
emits a final answer (no Action: line) or the loop bound is exceeded, the
script exits.

Usage:
    python3 ollama_react_worker.py --model gemma4:26b --prompt-file prompt.txt

Exit codes:
    0  finished successfully (final answer produced)
    1  error (connection failure, bad model, etc.)
    2  loop bound exceeded without final answer
"""

import argparse
import json
import os
import sys
import urllib.error
import urllib.request

MAX_TURNS = 30
OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://localhost:11434")


def ollama_generate(model: str, prompt: str) -> str:
    payload = json.dumps({"model": model, "prompt": prompt, "stream": False}).encode()
    req = urllib.request.Request(
        f"{OLLAMA_URL}/api/generate",
        data=payload,
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=300) as resp:
            data = json.loads(resp.read())
            return data.get("response", "")
    except urllib.error.URLError as exc:
        print(f"[ollama_react_worker] Connection error: {exc}", file=sys.stderr)
        sys.exit(1)


def run_react_loop(model: str, prompt: str) -> int:
    context = prompt
    for turn in range(MAX_TURNS):
        response = ollama_generate(model, context)
        print(response)

        lines = response.splitlines()
        action_line = next((l for l in lines if l.startswith("Action:")), None)
        if action_line is None:
            # No Action: means the model produced a final answer.
            return 0

        action = action_line[len("Action:"):].strip()
        # Minimal tool dispatch: echo back a stub observation so the loop continues.
        # Real tool dispatch would run the action and capture its output.
        observation = f"Observation: (tool '{action}' not available in ReAct stub)"
        print(observation)
        context = context + "\n" + response + "\n" + observation

    print("[ollama_react_worker] Loop bound exceeded", file=sys.stderr)
    return 2


def main():
    parser = argparse.ArgumentParser(description="Ollama ReAct worker for BOI")
    parser.add_argument("--model", required=True, help="Ollama model tag (e.g. gemma4:26b)")
    parser.add_argument("--prompt-file", required=True, help="Path to prompt file")
    args = parser.parse_args()

    prompt_path = os.path.expanduser(args.prompt_file)
    try:
        with open(prompt_path, encoding="utf-8") as f:
            prompt = f.read()
    except OSError as exc:
        print(f"[ollama_react_worker] Cannot read prompt file: {exc}", file=sys.stderr)
        sys.exit(1)

    sys.exit(run_react_loop(args.model, prompt))


if __name__ == "__main__":
    main()
