"""Generate deterministic offline workloads for the optimization comparison."""
import json
from pathlib import Path
import sys


def generate(directory: Path) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    packages = {
        f"p{i}": [f"p{i}@1.0.0", "", {"dependencies": {
            f"p{j}": "1.0.0" for j in range(i + 1, min(i + 4, 150))}}, "sha512-AAAA"]
        for i in range(150)
    }
    workspaces = {"": {"name": "bench", "dependencies": {"p0": "1.0.0"}}}
    workspaces.update({f"packages/w{i}": {"name": f"w{i}", "dependencies": {"p0": "1.0.0"}}
                       for i in range(200)})
    (directory / "bun-closures.lock").write_text(json.dumps({
        "lockfileVersion": 1, "workspaces": workspaces, "packages": packages}))
    (directory / "bun-prefetch.lock").write_text(json.dumps({
        "lockfileVersion": 1, "workspaces": {"": {"name": "bench"}},
        "packages": {f"p{i}": [f"p{i}@https://example.test/source{i % 8}.tgz", {}]
                     for i in range(64)}}))
    fake = directory / "fake-bin"
    fake.mkdir(exist_ok=True)
    nix = fake / "nix"
    nix.write_text('#!/bin/sh\nsleep "${BENCH_PREFETCH_DELAY:-0.01}"\nprintf \'{"hash":"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","storePath":"/nix/store/fake"}\\n\'\n')
    nix.chmod(0o755)


if __name__ == "__main__":
    generate(Path(sys.argv[1]))
