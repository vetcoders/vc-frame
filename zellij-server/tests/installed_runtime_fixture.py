"""Private product RuntimePack for native/browser boundary acceptance.

The public Frame door is the real product wrapper, the engine is a physical
libexec copy, and Python is the release's pinned portable CPython artifact.
No installed app, global config, provider or developer fallback is used.
"""

import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess


def runtime_snapshot(root):
    """Content and permission receipt for this exact private generation."""
    return {
        str(path.relative_to(root)): {
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "mode": stat.S_IMODE(path.stat().st_mode),
        }
        for path in sorted(root.rglob("*")) if path.is_file()
    }


def materialize_installed_runtime(source_runtime, binary, python, scratch,
                                  config_mutator=None, *, require_clean=True):
    """Return runtime/entry/engine/python/config/env plus immutable receipts.

    config_mutator(config, env, runtime) may adapt only the unpublished private
    writable product configuration. The carried generation is then sealed.
    ``python`` is retained in the shared fixture interface but is never used as
    the selected interpreter: the canonical portable artifact supplies it.
    """
    bootstrap_python = Path(python).expanduser().absolute()
    assert bootstrap_python.is_file(), "a known bootstrap Python is required for the artifact manifest"
    source_runtime = Path(source_runtime).resolve(strict=True)
    binary = Path(binary).resolve(strict=True)
    scratch = Path(scratch).resolve(strict=True)
    source_sha = subprocess.check_output(
        ["git", "-C", str(source_runtime), "rev-parse", "HEAD"], text=True).strip()
    source_status = subprocess.check_output(
        ["git", "-C", str(source_runtime), "status", "--porcelain"], text=True)
    if require_clean:
        assert not source_status.strip(), "selected runtime source must be clean"
    runtime = scratch / "runtime"
    shutil.copytree(source_runtime, runtime, symlinks=False,
                    ignore=shutil.ignore_patterns(
                        ".git", ".venv", "target", "node_modules", "build", "dist",
                        ".vibecrafted", ".prview", ".remember", ".ai-context",
                        ".loctree", "__pycache__", "*.pyc", "*.pyo", ".pytest_cache",
                        ".ruff_cache", ".mypy_cache"))
    core = runtime / "vibecrafted-core"
    bindir = runtime / "bin"
    bindir.mkdir(exist_ok=True)
    engine = runtime / "libexec/vc-frame"
    engine.parent.mkdir(exist_ok=True)
    shutil.copy2(binary, engine)
    entry = bindir / "vc-frame"
    shutil.copy2(runtime / "scripts/vc-frame-product-entry.sh", entry)
    entry.chmod(0o755)
    env = {key: os.environ[key] for key in ("PATH", "USER", "LOGNAME", "LANG")
           if key in os.environ}
    env.update(TERM="xterm-256color", COLORTERM="truecolor", SHELL="/bin/bash",
               VC_FRAME_SERVER_FOREGROUND="1", VC_FRAME_ROUTE_DIAGNOSTICS="1",
               PYTHONDONTWRITEBYTECODE="1")
    for key in ("HOME", "TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
                "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "VIBECRAFTED_HOME"):
        directory = scratch / key.lower()
        directory.mkdir(exist_ok=True)
        env[key] = str(directory)
    env["XDG_CONFIG_HOME"] = str(Path(env["HOME"]) / ".config")
    Path(env["XDG_CONFIG_HOME"]).mkdir(exist_ok=True)
    sockets = scratch / "s"
    sockets.mkdir(exist_ok=True)
    env.update(VC_FRAME_SOCKET_DIR=str(sockets), ZELLIJ_SOCKET_DIR=str(sockets),
               VIBECRAFTED_RUNTIME_ROOT=str(runtime), VIBECRAFTED_ROOT=str(runtime),
               VIBECRAFTED_RUNTIME_BIN=str(bindir), VIBECRAFTED_CORE_DIR=str(core),
               VIBECRAFTED_VC_FRAME_BIN=str(engine),
               VIBECRAFTED_CONTROL_PLANE=str(scratch / "disabled-control-plane.json"))
    env["PATH"] = str(bindir) + ":" + env.get("PATH", "/usr/bin:/bin")
    seed = scratch / "python-seed"
    bootstrap_env = dict(env)
    # Only artifact selection uses the explicitly supplied host interpreter.
    # Do not consult an inherited installed runtime-pin door while this private
    # generation's checksum-pinned interpreter is still being materialized.
    bootstrap_env["PATH"] = str(bootstrap_python.parent) + ":/opt/homebrew/bin:/usr/bin:/bin"
    portable = subprocess.check_output(
        ["/bin/bash", "-c", 'set -euo pipefail; die() { printf "%s\\n" "$*" >&2; exit 1; }; '
         'source "$1"; install_portable_python "$2"', "fixture",
         str(runtime / "scripts/lib/portable-python.sh"), str(seed)],
        env=bootstrap_env, text=True).strip()
    portable = Path(portable)
    shutil.copytree(portable.parent.parent, runtime / "python", symlinks=False)
    python_bin = portable.name
    shutil.rmtree(seed)
    selected_python = bindir / "python3"
    # The release's public Python door; its concrete interpreter name comes
    # from the same checksum-verified portable artifact, not the host Python.
    selected_python.write_text(
        '#!/bin/bash\nset -euo pipefail\n'
        'runtime_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"\n'
        'export PYTHONNOUSERSITE=1\nexport PYTHONDONTWRITEBYTECODE=1\n'
        'export PYTHONPATH="$runtime_root/vibecrafted-core:$runtime_root/vibecrafted-mcp:$runtime_root/python-site"\n'
        'exec "$runtime_root/python/bin/' + python_bin + '" "$@"\n')
    selected_python.chmod(0o755)
    uv = shutil.which("uv")
    assert uv, "uv is required to materialize the selected runtime's Python dependencies"
    subprocess.run([uv, "pip", "install", "--python", str(runtime / "python/bin" / python_bin),
                    "--target", str(runtime / "python-site"), "--no-compile-bytecode",
                    "jsonschema>=4.23,<5", "PyYAML>=6.0,<7", "websockets>=15,<18"],
                   env=env, check=True)
    env["VIBECRAFTED_PYTHON"] = str(selected_python)
    config = Path(env["HOME"]) / ".config/vibecrafted/vc-frame"
    env["VC_FRAME_CONFIG_DIR"] = str(config)
    env["VC_FRAME_CONFIG_FILE"] = str(config / "config.kdl")
    subprocess.run([str(selected_python), "-c",
                    "import sys;from pathlib import Path;"
                    "from vibecrafted_core.vc_frame_staging import materialize_vc_frame_config;"
                    "materialize_vc_frame_config(Path(sys.argv[1]),Path(sys.argv[2]),"
                    "pane_shell='/bin/bash',clipboard_command=None)",
                    str(core / "vibecrafted_core/config/vc-frame"), str(config)],
                   env=env, check=True)
    if config_mutator:
        config_mutator(config, env, runtime)
    # No startup path may repair or extend the carried generation. Only the
    # materialized product config, catalog, sockets and caches are writable.
    for path in sorted(runtime.rglob("*"), reverse=True):
        path.chmod(path.stat().st_mode & ~0o222)
    runtime.chmod(runtime.stat().st_mode & ~0o222)
    result = dict(runtime=runtime, entry=entry, engine=engine,
                  python=selected_python, config=config, env=env,
                  source_sha=source_sha, source_clean=not source_status.strip(),
                  snapshot=runtime_snapshot(runtime),
                  wrapper_sha256=hashlib.sha256(entry.read_bytes()).hexdigest(),
                  binary_sha256=hashlib.sha256(engine.read_bytes()).hexdigest(),
                  python_artifact=json.loads((runtime / "scripts/lib/portable-python-artifact.json").read_text()))
    return result
