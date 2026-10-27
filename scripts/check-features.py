#!/usr/bin/env python3
"""Hardware-free CLI and GTK regression checks. Never launches the live app."""
import os
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(args, *, success=True, env=None):
    result = subprocess.run(args, cwd=ROOT, env=env, capture_output=True,
                            text=True, timeout=180)
    if (result.returncode == 0) != success:
        raise AssertionError(f"{args}: exit={result.returncode}\n{result.stdout}\n{result.stderr}")
    return result.stdout + result.stderr


run(["cargo", "build", "--locked"])
extension = ROOT / "assets/gnome-extension"
metadata = json.loads((extension / "metadata.json").read_text())
assert metadata["uuid"] == "logicenter-context@chrisdaggas.com"
if shutil.which("node"):
    checked = subprocess.run(["node", "--input-type=module", "--check"],
                             input=(extension / "extension.js").read_text(),
                             text=True, capture_output=True, timeout=10)
    assert checked.returncode == 0, checked.stderr
binary = str(ROOT / "target/debug/logicenter")
assert run([binary, "--version"]).startswith("logicenter ")
help_text = run([binary, "--help"])
for flag in ["--device", "--apply-json", "--diagnostics", "--peripherals"]:
    assert flag in help_text, f"Missing documented command {flag}"
for arguments in [["--set-dpi"], ["--set-dpi", "0"], ["--json"],
                  ["--unknown-feature"], ["--list", "--set-dpi", "800"]]:
    run([binary, *arguments], success=False)
with tempfile.TemporaryDirectory(prefix="logicenter-cli-", dir="/tmp/opencode") as directory:
    file = Path(directory) / "invalid.json"
    file.write_text('{"target_host":1}')
    output = run([binary, "--apply-json", str(file)], success=False)
    assert "explicit interactive controls" in output

# GTK's Broadway backend creates real widgets without using the user's display.
server = shutil.which("gtk4-broadwayd")
if not server:
    raise RuntimeError("gtk4-broadwayd is required for the hardware-free GTK smoke test")
with tempfile.TemporaryDirectory(prefix="logicenter-broadway-", dir="/tmp/opencode") as directory:
    environment = os.environ.copy()
    environment.update(XDG_RUNTIME_DIR=directory, GDK_BACKEND="broadway", BROADWAY_DISPLAY=":39",
                       XDG_CONFIG_HOME=str(Path(directory) / "config"),
                       GTK_A11Y="none", GSK_RENDERER="cairo", LC_ALL="C.UTF-8")
    process = subprocess.Popen([server, "--address=127.0.0.1", "--port=0", ":39"], env=environment, stdout=subprocess.DEVNULL,
                               stderr=subprocess.PIPE, text=True)
    try:
        # Wait for the actual UNIX socket rather than sleeping an arbitrary delay.
        import time
        deadline = time.monotonic() + 5
        while not any(Path(directory).glob("broadway*.socket")):
            if process.poll() is not None:
                raise RuntimeError(process.stderr.read())
            if time.monotonic() >= deadline:
                raise RuntimeError("Broadway socket did not appear")
            time.sleep(0.02)
        output = run(["cargo", "test", "--locked", "device_pages", "--", "--ignored", "--nocapture",
                      "--test-threads=1"], env=environment)
        assert "1 passed" in output and "0 failed" in output
        assert "CAMERA SIDEBAR INTEGRATION PASSED" in output
        assert "CAMERA INLINE PREVIEW INTEGRATION PASSED" in output
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
print("FEATURE INTEGRATION CHECKS PASSED")
