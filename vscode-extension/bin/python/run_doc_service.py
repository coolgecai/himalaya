#!/usr/bin/env python3
"""Launch the bundled Himalaya document MCP service.

The VSIX ships a wheelhouse so document-generation dependencies can be
installed without reaching PyPI. This launcher creates/reuses a user-level
virtualenv, installs the bundled doc-service with the `extract` extra, and then
execs `python -m himalaya_doc_service`.
"""

from __future__ import annotations

import os
import subprocess
import sys
import venv
from pathlib import Path


def _venv_python(venv_dir: Path) -> Path:
    if os.name == "nt":
        return venv_dir / "Scripts" / "python.exe"
    return venv_dir / "bin" / "python"


def _needs_install(python: Path) -> bool:
    probe = (
        "import himalaya_doc_service, pptx, docx, openpyxl, reportlab, matplotlib, PIL, pydantic; "
        "import fitz, pdfplumber"
    )
    result = subprocess.run(
        [str(python), "-c", probe],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return result.returncode != 0


def _run(args: list[str]) -> None:
    subprocess.run(args, check=True)


def main() -> None:
    bundle_root = Path(__file__).resolve().parent
    service_root = bundle_root / "himalaya_doc_service"
    wheelhouse = bundle_root / "wheelhouse"
    default_venv = Path.home() / ".Himalaya" / "doc-service-venv"
    venv_dir = Path(os.environ.get("HIMALAYA_DOC_SERVICE_VENV", str(default_venv))).expanduser()

    if not service_root.exists():
        raise SystemExit(f"missing bundled doc-service source: {service_root}")
    if not wheelhouse.exists():
        raise SystemExit(f"missing bundled doc-service wheelhouse: {wheelhouse}")

    python = _venv_python(venv_dir)
    if not python.exists():
        venv_dir.parent.mkdir(parents=True, exist_ok=True)
        venv.EnvBuilder(with_pip=True, clear=False).create(venv_dir)

    if _needs_install(python):
        _run(
            [
                str(python),
                "-m",
                "pip",
                "install",
                "--no-index",
                "--find-links",
                str(wheelhouse),
                "himalaya-doc-service[extract]==0.1.0",
            ]
        )

    os.execv(str(python), [str(python), "-m", "himalaya_doc_service"])


if __name__ == "__main__":
    main()
