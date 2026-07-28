import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check-mise-tools-root.py"


def run(root: Path):
    return subprocess.run(
        [sys.executable, str(SCRIPT)],
        cwd=root, capture_output=True, text=True,
    )


def write(p: Path, body: str):
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(body)


def test_clean_tree_passes(tmp_path):
    write(tmp_path / "mise.toml",
          '[monorepo]\nconfig_roots=["mod"]\n[tools]\nuv="1.0"\n')
    write(tmp_path / "mod" / "mise.toml", "# tasks only\n")
    r = run(tmp_path)
    assert r.returncode == 0, r.stderr


def test_tools_in_module_fails(tmp_path):
    write(tmp_path / "mise.toml",
          '[monorepo]\nconfig_roots=["mod"]\n[tools]\nuv="1.0"\n')
    write(tmp_path / "mod" / "mise.toml", '[tools]\nruff="1.0"\n')
    r = run(tmp_path)
    assert r.returncode == 1
    assert "mod/mise.toml" in r.stderr


def test_dotted_tools_table_in_module_fails(tmp_path):
    write(tmp_path / "mise.toml",
          '[monorepo]\nconfig_roots=["mod"]\n[tools]\nuv="1.0"\n')
    write(tmp_path / "mod" / "mise.toml", '[tools.ruff]\nversion="1.0"\n')
    r = run(tmp_path)
    assert r.returncode == 1
