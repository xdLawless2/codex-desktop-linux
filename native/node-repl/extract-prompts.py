#!/usr/bin/env python3

import json
import pathlib
import sys


def extract(data: bytes, start: str, end: str) -> str:
    start_bytes = start.encode()
    end_bytes = end.encode()
    start_index = data.index(start_bytes)
    end_index = data.index(end_bytes, start_index) + len(end_bytes)
    return data[start_index:end_index].decode("utf-8")


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: extract-prompts.py <official-node_repl> <output-json>")

    data = pathlib.Path(sys.argv[1]).read_bytes()
    prompts = {
        "serverInstructions": extract(
            data,
            "Use `js` to run JavaScript in the persistent Node-backed kernel.",
            "rather than filesystem paths under `./node_modules`.",
        ),
        "jsDescription": extract(
            data,
            "Run JavaScript in a persistent Node-backed kernel with top-level await.",
            "Prefer `nodeRepl.write(...)` for text or formatted values and "
            "`nodeRepl.emitImage(...)` for images.",
        ),
        "jsTitleDescription": extract(
            data,
            "Short user-facing description of what this code block is doing.",
            "or `Render chart preview`.",
        ),
        "jsCodeDescription": extract(
            data,
            "JavaScript source to execute in the persistent Node-backed kernel.",
            "or `await nodeRepl.emitImage(pngBuffer)`.",
        ),
        "jsTimeoutDescription": extract(
            data,
            "Optional execution timeout in milliseconds.",
            "when omitted.",
        ),
        "resetDescription": extract(
            data,
            "Reset the persistent JavaScript kernel and clear all bindings",
            "cannot recover from conflicting declarations.",
        ),
        "addNodeModuleDirDescription": extract(
            data,
            "Add an absolute `node_modules` directory to the REPL-wide",
            "when it was already present.",
        ),
        "nodeModuleDirPathDescription": (
            "Absolute path to a node_modules directory to add to Node package resolution."
        ),
    }
    pathlib.Path(sys.argv[2]).write_text(
        json.dumps(prompts, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
