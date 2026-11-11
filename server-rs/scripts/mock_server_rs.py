"""Standalone Rust mem0-desktop server for e2e scripts.

    python3 scripts/mock_server_rs.py --port 9901 [--data-dir DIR] [--log FILE]

Mirrors scripts/mock_server.py but launches the Rust binary
(server-rs/target/debug/mem0-desktop-rs or release). Prints READY <port> once /health
answers 200.
"""

import argparse
import os
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=9901)
    ap.add_argument("--data-dir", default=None)
    ap.add_argument("--log", default=None)
    args = ap.parse_args()

    env = {k: v for k, v in os.environ.items() if not k.endswith("_API_KEY")}
    env.pop("MEM0_DATA_DIR", None)
    env["MEM0_DATA_DIR"] = args.data_dir or tempfile.mkdtemp(prefix="mock-server-rs-")
    env["MEM0_TELEMETRY"] = "false"
    env["AUTH_DISABLED"] = "true"
    env["MEM0_PORT"] = str(args.port)

    binary = REPO / "server-rs/target/release/mem0-desktop-rs"
    if not binary.exists():
        binary = REPO / "server-rs/target/debug/mem0-desktop-rs"
    if not binary.exists():
        print("Rust binary not built — run `cargo build -p mem0-desktop-rs` in server-rs/", file=sys.stderr)
        return 1

    log = open(args.log, "w") if args.log else subprocess.PIPE
    proc = subprocess.Popen(
        [str(binary)], cwd=REPO, env=env, stdout=log, stderr=subprocess.STDOUT, text=True,
    )

    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.time() + 60
    while time.time() < deadline:
        if proc.poll() is not None:
            print(f"server exited early (code {proc.returncode})", file=sys.stderr)
            return 1
        try:
            with opener.open(f"http://127.0.0.1:{args.port}/health", timeout=2) as response:
                if response.status == 200:
                    print(f"READY {args.port}", flush=True)
                    proc.wait()
                    return proc.returncode or 0
        except Exception:
            time.sleep(0.2)
    print("server did not become ready in 60s", file=sys.stderr)
    proc.kill()
    return 1


if __name__ == "__main__":
    sys.exit(main())
