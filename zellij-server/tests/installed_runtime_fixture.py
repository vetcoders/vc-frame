"""Private product RuntimePack for native/browser boundary acceptance.

The public Frame door is the real product wrapper and the engine is a physical
libexec copy. Existing installed Python/native dependencies and the signed app
terminal are read-only donors with recorded identities. The user's app/config
remain read-only; publication and execution use isolated private product state.
"""

import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import time


def runtime_snapshot(root):
    """Content and permission receipt for this exact private generation."""
    return {
        str(path.relative_to(root)): {
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "mode": stat.S_IMODE(path.stat().st_mode),
            "flags": getattr(path.stat(), "st_flags", 0),
        }
        for path in sorted(root.rglob("*")) if path.is_file()
    }


def thaw_private_runtime(scratch):
    """Clear this Darwin fixture's immutable flags before owned scratch removal."""
    supplied = Path(scratch).absolute()
    assert not supplied.is_symlink()
    scratch = supplied.resolve(strict=False)
    assert scratch.parent == Path("/tmp").resolve(strict=True)
    assert scratch.name.startswith(("vcrp-", "vcwrail-", "vcunbound-", "vcinstall-smoke-"))
    if not scratch.exists():
        return dict(scratch=str(scratch), status="already-absent", attempts=0)
    # A kill-session ACK can precede native daemon/child teardown. Never thaw
    # or remove bytes while any process still consumes this exact private tree.
    deadline = time.monotonic() + 10
    attempts = 0
    while True:
        attempts += 1
        census = subprocess.run(
            ["/usr/sbin/lsof", "-nP", "-w", "+D", str(scratch)],
            capture_output=True, text=True, timeout=5)
        if census.returncode == 1 and not census.stdout.strip() and not census.stderr.strip():
            break
        # Darwin lsof may return 1 with valid matching rows when its wider
        # process census is partial; those rows are still active consumers.
        if census.returncode not in (0, 1) or census.stderr.strip():
            raise RuntimeError("private scratch consumer census failed; refusing teardown: " + census.stderr.strip())
        if time.monotonic() >= deadline:
            raise RuntimeError("private scratch still has consumers; refusing teardown:\n" + census.stdout)
        time.sleep(.1)
    proof = dict(scratch=str(scratch), status="no-consumers", attempts=attempts)
    print(json.dumps(dict(private_cleanup_census=proof)))
    releases = scratch / "installed/releases"
    if not releases.exists():
        return proof
    assert not releases.is_symlink() and releases.resolve() == releases
    for generation in releases.iterdir():
        assert generation.is_dir() and not generation.is_symlink()
        paths = [generation, *generation.rglob("*")]
        assert all(not path.is_symlink() for path in paths)
        for path in paths:
            os.chflags(path, path.stat().st_flags & ~stat.UF_IMMUTABLE)
    return proof


def materialize_installed_runtime(source_runtime, binary, python, scratch,
                                  config_mutator=None, *, require_clean=True):
    """Return runtime/entry/engine/python/config/env plus immutable receipts.

    config_mutator(config, env, runtime) may change config.kdl preferences and
    add extra layouts after canonical publication. Managed defaults are checked
    unchanged, then the private generation is sealed without changing its modes.
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
    assert sys.platform == "darwin", "this installed fixture uses a signed Darwin donor"
    donor = (Path.home() / ".local/share/vibecrafted/tools/vibecrafted-current").resolve(strict=True)
    terminal_host = Path("/Applications/Vibecrafted.app/Contents/MacOS/vc-terminal").resolve(strict=True)
    donor_provenance = json.loads((donor / "runtime-pack-provenance.json").read_text())
    donor_revision = donor_provenance["source_revisions"]["vibecrafted"]
    donor_python_pin = subprocess.check_output(
        ["git", "-C", str(source_runtime), "show", donor_revision + ":scripts/lib/portable-python-artifact.json"])
    assert donor_python_pin == (source_runtime / "scripts/lib/portable-python-artifact.json").read_bytes(), "donor portable Python artifact differs from selected source pin"
    for item in donor_provenance["payload"]["files"]:
        if item["path"].startswith(("python/", "python-site/")) or item["path"] == "bin/vc-python":
            assert hashlib.sha256((donor / item["path"]).read_bytes()).hexdigest() == item["sha256"], item["path"]
    payload = scratch / "payload"
    shutil.copytree(donor, payload, symlinks=False)
    for path in payload.rglob("*"):
        path.chmod(path.stat().st_mode | (0o700 if path.is_dir() else 0o200))
    payload.chmod(payload.stat().st_mode | 0o700)
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
    runtime_home = scratch / "installed"
    env.update(VC_FRAME_SOCKET_DIR=str(sockets), ZELLIJ_SOCKET_DIR=str(sockets),
               VIBECRAFTED_RUNTIME_HOME=str(runtime_home),
               VIBECRAFTED_LAUNCHER_BIN=str(scratch / "launchers"),
               VIBECRAFTED_CONTROL_PLANE=str(scratch / "disabled-control-plane.json"))
    commands = []

    def run(argv):
        result = subprocess.run(list(map(str, argv)), env=env, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        commands.append(dict(argv=list(map(str, argv)), exit=result.returncode, output=result.stdout))
        (scratch / "materialization.json").write_text(json.dumps(commands, indent=2) + "\n")
        assert result.returncode == 0, (argv, result.stdout)
        return result.stdout

    # Canonical source payload excludes repository-only state. Replace complete
    # source-owned subtrees, retaining explicitly identified native donor bytes.
    stage = scratch / "source-payload"
    run([bootstrap_python, source_runtime / "scripts/distribution_manifest.py", "stage",
         "--source", source_runtime, "--destination", stage,
         "--owner-repo", "vetcoders/vibecrafted", "--source-revision", source_sha])
    native_magic = {b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xca\xfe\xba\xbe", b"\x7fELF"}
    donor_native = [path for path in (donor / "bin").iterdir()
                    if path.is_file() and path.read_bytes()[:4] in native_magic]
    for path in stage.iterdir():
        target = payload / path.name
        if target.is_dir():
            shutil.rmtree(target)
        elif target.exists():
            target.unlink()
        if path.is_dir():
            shutil.copytree(path, target)
        else:
            shutil.copy2(path, target)
    for path in donor_native:
        shutil.copy2(path, payload / "bin" / path.name)
    shutil.copy2(donor / "bin/python3", payload / "bin/python3")
    for name in ("runtime-manifest.json", "runtime-pack-provenance.json", "source-provenance.json", "runtime-foundations.json"):
        (payload / name).unlink(missing_ok=True)
    shutil.copy2(terminal_host, payload / "libexec/vc-terminal")
    shutil.copy2(binary, payload / "libexec/vc-frame")
    for name in ("vc-frame", "vc-terminal"):
        shutil.copy2(payload / "scripts" / (name + "-product-entry.sh"), payload / "bin" / name)
        (payload / "bin" / name).chmod(0o755)
    # Existing native Python doors remain bound to the exact same pinned donor
    # CPython library; render only missing declared script launchers, no compile.
    for package in ("vibecrafted-core", "vibecrafted-mcp"):
        run([bootstrap_python, source_runtime / "scripts/render-python-entrypoint-launchers.py",
             "--pyproject", source_runtime / package / "pyproject.toml", "--bin-dir", payload / "bin"])
    runtime_stamp = json.loads(run([
        bootstrap_python, "-c",
        "import json,runpy,sys;from pathlib import Path;"
        "source=Path(sys.argv[1]);runtime=Path(sys.argv[2]);revision=sys.argv[3];"
        "sys.path.insert(0,str(source/'scripts'));"
        "owner=runpy.run_path(str(source/'scripts/vetcoders_install.py'));"
        "version=owner['get_install_version'](source);"
        "expected=owner['get_framework_version'](source).split('+',1)[0].strip()+'+g'+revision[:8];"
        "assert version==expected,(version,expected);"
        "stamped=owner['stamp_install_version'](runtime,version);"
        "print(json.dumps(dict(version=version,source_revision=revision,files=[str(path.relative_to(runtime)) for path in stamped])))",
        source_runtime, payload, source_sha]))
    run([bootstrap_python, source_runtime / "scripts/distribution_manifest.py", "carrier",
         "--source", source_runtime, "--output", payload / "source-provenance.json",
         "--owner-repo", "vetcoders/vibecrafted", "--source-revision", source_sha])
    run([payload / "bin/python3", payload / "vibecrafted-core/vibecrafted_core/runtime_pack_contract.py",
         "write-foundations", "--root", payload])
    install_output = run([bootstrap_python, payload / "scripts/vetcoders_install.py", "runtime-install",
                         "--payload-root", payload, "--runtime-home", runtime_home, "--skip-foundations"])
    resolution = json.loads(run([bootstrap_python, payload / "scripts/vetcoders_install.py", "runtime-resolve",
                                "--runtime-home", runtime_home, "--json"]))
    assert resolution["status"] == "ready", resolution
    runtime = runtime_home / "releases" / runtime_stamp["version"]
    assert runtime.is_dir() and not runtime.is_symlink()
    bindir = runtime / "bin"
    selected_python = bindir / "python3"
    entry, engine = bindir / "vc-frame", runtime / "libexec/vc-frame"
    env.update(VIBECRAFTED_RUNTIME_ROOT=str(runtime), VIBECRAFTED_ROOT=str(runtime),
               VIBECRAFTED_RUNTIME_BIN=str(bindir), VIBECRAFTED_CORE_DIR=str(runtime / "vibecrafted-core"),
               VIBECRAFTED_VC_FRAME_BIN=str(engine), VIBECRAFTED_PYTHON=str(selected_python))
    env["PATH"] = str(bindir) + ":" + env.get("PATH", "/usr/bin:/bin")
    config = Path(env["HOME"]) / ".config/vibecrafted/vc-frame"
    env["VC_FRAME_CONFIG_DIR"] = str(config)
    env["VC_FRAME_CONFIG_FILE"] = str(config / "config.kdl")
    if config_mutator:
        defaults = runtime_snapshot(config)
        config_mutator(config, env, runtime)
        after = runtime_snapshot(config)
        assert all(after.get(path) == before for path, before in defaults.items() if path != "config.kdl"), "fixture mutated an installed managed default instead of adding an extra layout"
    # Preserve canonical carrier modes while preventing actual startup writes.
    # chmod(0444) would itself invalidate the install owner's 0644 contract.
    assert runtime.parent == scratch / "installed/releases" and runtime.resolve() == runtime
    sealed = [*runtime.rglob("*"), runtime]
    assert all(not path.is_symlink() for path in sealed)
    for path in sealed:
        os.chflags(path, path.stat().st_flags | stat.UF_IMMUTABLE)
    resolution = json.loads(run([bootstrap_python, payload / "scripts/vetcoders_install.py", "runtime-resolve",
                                "--runtime-home", runtime_home, "--json"]))
    assert resolution["status"] == "ready", resolution
    components = {str(path.relative_to(donor)): hashlib.sha256(path.read_bytes()).hexdigest()
                  for path in donor_native}
    return dict(runtime=runtime, entry=entry, engine=engine, python=selected_python,
                config=config, env=env, source_sha=source_sha, source_clean=not source_status.strip(),
                snapshot=runtime_snapshot(runtime), runtime_stamp=runtime_stamp,
                wrapper_sha256=hashlib.sha256(entry.read_bytes()).hexdigest(),
                binary_sha256=hashlib.sha256(engine.read_bytes()).hexdigest(),
                python_artifact=json.loads((runtime / "scripts/lib/portable-python-artifact.json").read_text()),
                materialization_commands=commands, runtime_resolution=resolution,
                donor=dict(root=str(donor), version=(donor / "VERSION").read_text().strip(),
                           source_revisions=donor_provenance["source_revisions"],
                           provenance_sha256=hashlib.sha256((donor / "runtime-pack-provenance.json").read_bytes()).hexdigest(),
                           python_pin_sha256=hashlib.sha256(donor_python_pin).hexdigest(),
                           python_door_sha256=hashlib.sha256((donor / "bin/python3").read_bytes()).hexdigest(),
                           native_components=components, terminal=str(terminal_host),
                           terminal_sha256=hashlib.sha256(terminal_host.read_bytes()).hexdigest(),
                           python_site="read-only donor dependencies; not claimed built from selected source"),
                installer_output=install_output)
