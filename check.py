#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import subprocess
import tomllib
from dataclasses import dataclass
from pathlib import Path
from pathlib import PurePosixPath


ROOT = Path(__file__).resolve().parent
WORKSPACE_MANIFEST = ROOT / "Cargo.toml"
DEFAULT_MAX_SOURCE_FILE_LINES = 2500
DEFAULT_SOURCE_FILE_INCLUDE = ("*.rs", "**/*.rs")
IGNORED_SOURCE_DIRS = frozenset(
    {".direnv", ".git", ".hg", ".jj", ".svn", "__pycache__", "node_modules", "target", "vendor"}
)


@dataclass(frozen=True, slots=True)
class SourceFilePolicy:
    max_lines: int
    include: tuple[str, ...]
    exclude: tuple[str, ...]


def load_workspace_metadata() -> dict[str, object]:
    workspace = tomllib.loads(WORKSPACE_MANIFEST.read_text(encoding="utf-8"))
    return workspace["workspace"]["metadata"]["rust-starter"]


def load_commands(metadata: dict[str, object]) -> dict[str, list[str]]:
    commands: dict[str, list[str]] = {}
    for key in ("format_command", "clippy_command", "test_command", "doc_command", "fix_command"):
        value = metadata.get(key)
        if isinstance(value, list) and value and all(isinstance(part, str) for part in value):
            commands[key] = value
    return commands


def load_patterns(
    value: object,
    *,
    default: tuple[str, ...],
    key_path: str,
    allow_empty: bool,
) -> tuple[str, ...]:
    if value is None:
        return default
    if not isinstance(value, list) or not all(isinstance(pattern, str) and pattern for pattern in value):
        raise SystemExit(f"[check] invalid {key_path}: expected a string list")
    if not allow_empty and not value:
        raise SystemExit(f"[check] invalid {key_path}: expected at least one pattern")
    return tuple(value)


def load_source_file_policy(metadata: dict[str, object]) -> SourceFilePolicy:
    raw_policy = metadata.get("source_files")
    if raw_policy is None:
        return SourceFilePolicy(DEFAULT_MAX_SOURCE_FILE_LINES, DEFAULT_SOURCE_FILE_INCLUDE, ())
    if not isinstance(raw_policy, dict):
        raise SystemExit("[check] invalid workspace.metadata.rust-starter.source_files: expected a table")

    max_lines = raw_policy.get("max_lines", DEFAULT_MAX_SOURCE_FILE_LINES)
    if not isinstance(max_lines, int) or max_lines <= 0:
        raise SystemExit(
            "[check] invalid workspace.metadata.rust-starter.source_files.max_lines: expected a positive integer"
        )

    include = load_patterns(
        raw_policy.get("include"),
        default=DEFAULT_SOURCE_FILE_INCLUDE,
        key_path="workspace.metadata.rust-starter.source_files.include",
        allow_empty=False,
    )
    exclude = load_patterns(
        raw_policy.get("exclude"),
        default=(),
        key_path="workspace.metadata.rust-starter.source_files.exclude",
        allow_empty=True,
    )
    return SourceFilePolicy(max_lines, include, exclude)


def run(name: str, argv: list[str]) -> None:
    print(f"[check] {name}: {' '.join(argv)}", flush=True)
    proc = subprocess.run(argv, cwd=ROOT)
    if proc.returncode != 0:
        raise SystemExit(proc.returncode)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Thin Rust starter check runner")
    parser.add_argument(
        "mode",
        nargs="?",
        choices=("check", "deep", "fix"),
        default="check",
        help="Run the fast gate, include docs for the deep gate, or run the fix command.",
    )
    return parser.parse_args()


def matches_pattern(path: PurePosixPath, pattern: str) -> bool:
    if path.match(pattern):
        return True
    prefix = "**/"
    return pattern.startswith(prefix) and path.match(pattern.removeprefix(prefix))


def iter_source_files(policy: SourceFilePolicy) -> list[Path]:
    paths: list[Path] = []
    for current_root, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = sorted(name for name in dirnames if name not in IGNORED_SOURCE_DIRS)
        current = Path(current_root)
        for filename in filenames:
            path = current / filename
            relative_path = PurePosixPath(path.relative_to(ROOT).as_posix())
            if not any(matches_pattern(relative_path, pattern) for pattern in policy.include):
                continue
            if any(matches_pattern(relative_path, pattern) for pattern in policy.exclude):
                continue
            paths.append(path)
    return sorted(paths)


def line_count(path: Path) -> int:
    return len(path.read_text(encoding="utf-8").splitlines())


def enforce_source_file_policy(policy: SourceFilePolicy) -> None:
    paths = iter_source_files(policy)
    print(f"[check] source-files: max {policy.max_lines} lines", flush=True)
    violations: list[tuple[str, int]] = []
    for path in paths:
        lines = line_count(path)
        if lines > policy.max_lines:
            violations.append((path.relative_to(ROOT).as_posix(), lines))
    if not violations:
        return

    print(
        f"[check] source-files: {len(violations)} file(s) exceed the configured limit",
        flush=True,
    )
    for relative_path, lines in violations:
        print(f"[check] source-files: {relative_path}: {lines} lines", flush=True)
    raise SystemExit(1)


def main() -> None:
    metadata = load_workspace_metadata()
    commands = load_commands(metadata)
    source_file_policy = load_source_file_policy(metadata)
    args = parse_args()

    if args.mode == "fix":
        run("fix", commands["fix_command"])
        return

    enforce_source_file_policy(source_file_policy)
    run("fmt", commands["format_command"])
    run("clippy", commands["clippy_command"])
    run("test", commands["test_command"])

    if args.mode == "deep" and "doc_command" in commands:
        run("doc", commands["doc_command"])


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        raise SystemExit(130)
