#!/usr/bin/env python3

import json
import os
import selectors
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SERVER = ROOT / "native/node-repl/server.mjs"
PROMPT_KEYS = (
    "addNodeModuleDirDescription",
    "jsCodeDescription",
    "jsDescription",
    "jsTimeoutDescription",
    "jsTitleDescription",
    "nodeModuleDirPathDescription",
    "resetDescription",
    "serverInstructions",
)


class NodeRepl:
    def __init__(self, temporary: Path):
        prompts = temporary / "official-prompts.json"
        prompts.write_text(
            json.dumps({key: f"CI prompt: {key}" for key in PROMPT_KEYS}),
            encoding="utf-8",
        )
        codex_home = temporary / "codex-home"
        codex_home.mkdir()
        environment = {
            **os.environ,
            "CODEX_HOME": str(codex_home),
            "NODE_REPL_CLIENT_REQUEST_TIMEOUT_MS": "250",
            "NODE_REPL_ELICITATION_TIMEOUT_MS": "250",
            "NODE_REPL_PROMPTS_PATH": str(prompts),
        }
        self.process = subprocess.Popen(
            ["node", "--experimental-vm-modules", str(SERVER)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=environment,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)

    def send(self, message):
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def read(self, timeout=5):
        events = self.selector.select(timeout)
        if not events:
            stderr = self.process.stderr.read() if self.process.poll() is not None else ""
            raise AssertionError(f"node_repl produced no response; stderr={stderr!r}")
        assert self.process.stdout is not None
        line = self.process.stdout.readline()
        if not line:
            stderr = self.process.stderr.read() if self.process.stderr else ""
            raise AssertionError(
                f"node_repl exited with {self.process.poll()}; stderr={stderr!r}"
            )
        return json.loads(line)

    def request(self, request_id, method, params=None):
        message = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self.send(message)
        response = self.read()
        assert response.get("id") == request_id, response
        return response

    def call(self, request_id, name, arguments=None):
        return self.request(
            request_id,
            "tools/call",
            {"name": name, "arguments": arguments or {}},
        )["result"]

    def js(self, request_id, code, timeout_ms=1_000):
        return self.call(
            request_id,
            "js",
            {"code": code, "timeout_ms": timeout_ms, "title": "CI validation"},
        )

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=2)


def text_content(result):
    return "\n".join(
        item["text"] for item in result["content"] if item.get("type") == "text"
    )


def main():
    with tempfile.TemporaryDirectory(prefix="codex-node-repl-ci-") as directory:
        temporary = Path(directory)
        runtime = NodeRepl(temporary)
        try:
            initialized = runtime.request(
                1,
                "initialize",
                {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "ci", "version": "1"},
                },
            )
            assert initialized["result"]["serverInfo"]["name"] == "node_repl"

            listed = runtime.request(2, "tools/list", {})
            assert {tool["name"] for tool in listed["result"]["tools"]} == {
                "js",
                "js_reset",
                "js_add_node_module_dir",
            }

            first = runtime.js(3, "let answer = 41; nodeRepl.write(answer)")
            assert not first["isError"] and "41" in text_content(first)
            second = runtime.js(4, "nodeRepl.write(answer + 1)")
            assert not second["isError"] and "42" in text_content(second)

            reset = runtime.call(5, "js_reset")
            assert "reset" in text_content(reset).lower()
            after_reset = runtime.js(6, "nodeRepl.write(typeof answer)")
            assert not after_reset["isError"] and "undefined" in text_content(after_reset)

            module_dir = temporary / "modules"
            module_dir.mkdir()
            added = runtime.call(7, "js_add_node_module_dir", {"path": str(module_dir)})
            assert text_content(added).strip() == "true"

            untrusted = temporary / "untrusted.mjs"
            untrusted.write_text("export default 1;\n", encoding="utf-8")
            rejected = runtime.js(8, f'await import("{untrusted}")')
            assert rejected["isError"] and "rejected untrusted import" in text_content(rejected)

            traversal = runtime.js(
                9, 'await nodeRepl.config.readToml("../outside.toml")'
            )
            assert traversal["isError"] and "escapes CODEX_HOME" in text_content(traversal)
            direct_write = runtime.js(
                10, 'await nodeRepl.config.writeToml("config.toml", {test: true})'
            )
            assert direct_write["isError"] and "does not allow writing" in text_content(
                direct_write
            )

            image = runtime.js(
                11,
                'nodeRepl.emitImage("data:image/png;base64,iVBORw0KGgo=")',
            )
            assert not image["isError"]
            assert any(item.get("type") == "image" for item in image["content"])

            # Browser origin-access grants are auto-accepted by the server
            # without forwarding an elicitation/create request to the client.
            auto_accepted = runtime.js(
                12,
                "nodeRepl.write(await nodeRepl.createElicitation({meta: {connector_id: 'browser-use', tool_name: 'access_browser_origin'}}))",
            )
            assert not auto_accepted["isError"], auto_accepted
            assert "accept" in text_content(auto_accepted)

            runtime.send(
                {
                    "jsonrpc": "2.0",
                    "id": 13,
                    "method": "tools/call",
                    "params": {
                        "name": "js",
                        "arguments": {
                            "code": "await nodeRepl.createElicitation({message: 'decline'})",
                            "timeout_ms": 1_000,
                        },
                    },
                }
            )
            elicitation = runtime.read()
            assert elicitation["method"] == "elicitation/create", elicitation
            runtime.send(
                {
                    "jsonrpc": "2.0",
                    "id": elicitation["id"],
                    "result": {"action": "decline", "content": {}},
                }
            )
            declined = runtime.read()
            assert declined["id"] == 13 and not declined["result"]["isError"]

            runtime.send(
                {
                    "jsonrpc": "2.0",
                    "id": 14,
                    "method": "tools/call",
                    "params": {
                        "name": "js",
                        "arguments": {
                            "code": "await nodeRepl.createElicitation({message: 'timeout'})",
                            "timeout_ms": 1_000,
                        },
                    },
                }
            )
            timeout_request = runtime.read()
            assert timeout_request["method"] == "elicitation/create", timeout_request
            timed_out = runtime.read(timeout=2)
            assert timed_out["id"] == 14 and timed_out["result"]["isError"]
            assert "timed out" in text_content(timed_out["result"])

            syntax_error = runtime.js(15, "let =")
            assert syntax_error["isError"]
        finally:
            runtime.close()

    print("node_repl protocol checks passed")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"node_repl protocol checks failed: {error}", file=sys.stderr)
        raise
