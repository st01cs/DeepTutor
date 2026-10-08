"""Desktop-shell contract for ``deeptutor start`` (Phase 0 spike).

The desktop shell owns no terminal, so it needs three things the browser flow
never needed: free ports without a prompt, a machine-readable ready/stopped
signal, and an orphan guard for the case where the shell is force-quit. These
tests pin the additive contract *and* the unchanged default behaviour, because
CLI and Web modes keep calling the same function.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from deeptutor.runtime import launcher
from deeptutor.services import app_update


def _fake_port_probe(monkeypatch, *, busy: set[int]) -> None:
    monkeypatch.setattr(
        launcher,
        "_port_accepts_connection",
        lambda port: port in busy,
    )


def test_runtime_info_writer_publishes_contract_and_merges_fields(tmp_path: Path) -> None:
    info_path = tmp_path / "desktop" / "runtime.json"
    writer = launcher.RuntimeInfoWriter(path=info_path, token="t0ken", home=tmp_path)

    writer.write("starting")
    starting = json.loads(info_path.read_text(encoding="utf-8"))
    assert starting["schema_version"] == launcher.RUNTIME_INFO_SCHEMA_VERSION
    assert starting["status"] == "starting"
    assert starting["token"] == "t0ken"
    assert starting["home"] == str(tmp_path)
    assert starting["pid"] > 0

    writer.write("ready", frontend_url="http://127.0.0.1:3782", frontend_port=3782)
    ready = json.loads(info_path.read_text(encoding="utf-8"))
    assert ready["status"] == "ready"
    assert ready["frontend_url"] == "http://127.0.0.1:3782"
    assert ready["frontend_port"] == 3782
    # Atomic replace leaves no partial file a shell could read mid-write.
    assert [path.name for path in info_path.parent.iterdir()] == ["runtime.json"]


def test_runtime_info_writer_failure_is_not_fatal(tmp_path: Path) -> None:
    """A shell that cannot read its state file must not break the launcher."""

    writer = launcher.RuntimeInfoWriter(
        path=tmp_path / "desktop" / "\0invalid",
        token="t0ken",
        home=tmp_path,
    )
    writer.write("ready")  # must not raise


def test_auto_resolve_ports_skips_taken_ports(monkeypatch) -> None:
    _fake_port_probe(monkeypatch, busy={8001, 8002, 3782})

    backend, frontend = launcher._auto_resolve_ports(
        backend_port=8001,
        frontend_port=3782,
        backend_taken=True,
        frontend_taken=True,
        check_frontend=True,
    )

    assert backend == 8003
    assert frontend == 3783


def test_conflict_resolution_auto_ports_persists_without_stdin(monkeypatch, tmp_path: Path) -> None:
    _fake_port_probe(monkeypatch, busy={8001})
    monkeypatch.setattr(launcher, "_port_listeners", lambda port: [])
    monkeypatch.setattr(launcher.sys, "stdin", None)
    saved: dict[str, int] = {}

    def fake_persist(settings_dir, backend_port, frontend_port):  # noqa: ANN001
        saved.update(backend=backend_port, frontend=frontend_port)
        return tmp_path / "system.json"

    monkeypatch.setattr(launcher, "_persist_ports", fake_persist)

    backend, frontend = launcher._resolve_port_conflicts(
        backend_port=8001,
        frontend_port=3782,
        check_frontend=True,
        settings_dir=tmp_path,
        auto_ports=True,
    )

    assert backend != 8001
    assert frontend == 3782
    assert saved == {"backend": backend, "frontend": frontend}


def test_conflict_resolution_without_auto_ports_still_exits_for_non_tty(
    monkeypatch, tmp_path: Path
) -> None:
    """The historical CLI behaviour is the default: no prompt, no silent move."""

    _fake_port_probe(monkeypatch, busy={8001})
    monkeypatch.setattr(launcher, "_port_listeners", lambda port: [])
    monkeypatch.setattr(launcher.sys, "stdin", None)

    with pytest.raises(SystemExit):
        launcher._resolve_port_conflicts(
            backend_port=8001,
            frontend_port=3782,
            check_frontend=True,
            settings_dir=tmp_path,
        )


def test_start_rejects_desktop_flags_with_detach(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("DEEPTUTOR_HOME", str(tmp_path))
    launched: list[object] = []
    monkeypatch.setattr(
        launcher,
        "_launch_detached",
        lambda *args, **kwargs: launched.append(kwargs),
    )

    with pytest.raises(SystemExit):
        launcher.start(home=tmp_path, detach=True, runtime_info=tmp_path / "runtime.json")

    assert launched == []


def test_detect_installation_reports_desktop_shell(monkeypatch) -> None:
    monkeypatch.setattr(app_update, "_running_in_container", lambda: False)
    monkeypatch.setenv(app_update.DESKTOP_SHELL_ENV, "1")

    installation = app_update.detect_installation()

    assert installation.mode == "desktop"
    assert installation.automatic_update is False
    assert "desktop app" in installation.reason


def test_detect_installation_without_env_is_not_desktop(monkeypatch) -> None:
    """The desktop branch must be strictly opt-in for CLI and Web installs."""

    monkeypatch.setattr(app_update, "_running_in_container", lambda: False)
    monkeypatch.delenv(app_update.DESKTOP_SHELL_ENV, raising=False)

    assert app_update.detect_installation().mode != "desktop"


def test_start_command_forwards_desktop_flags(monkeypatch, tmp_path: Path) -> None:
    from deeptutor_cli.main import app

    captured: dict[str, object] = {}
    monkeypatch.setattr(
        "deeptutor.runtime.launcher.start",
        lambda **kwargs: captured.update(kwargs),
    )

    result = CliRunner().invoke(
        app,
        [
            "start",
            "--home",
            str(tmp_path),
            "--no-browser",
            "--auto-ports",
            "--runtime-info",
            str(tmp_path / "runtime.json"),
            "--parent-pid",
            "4242",
        ],
    )

    assert result.exit_code == 0, result.output
    assert captured["runtime_info"] == tmp_path / "runtime.json"
    assert captured["auto_ports"] is True
    assert captured["parent_pid"] == 4242
    assert captured["open_browser"] is False


def test_start_command_defaults_keep_browser_behaviour(monkeypatch, tmp_path: Path) -> None:
    from deeptutor_cli.main import app

    captured: dict[str, object] = {}
    monkeypatch.setattr(
        "deeptutor.runtime.launcher.start",
        lambda **kwargs: captured.update(kwargs),
    )

    result = CliRunner().invoke(app, ["start", "--home", str(tmp_path)])

    assert result.exit_code == 0, result.output
    assert captured["open_browser"] is True
    assert captured["auto_ports"] is False
    assert captured["runtime_info"] is None
    assert captured["parent_pid"] is None
