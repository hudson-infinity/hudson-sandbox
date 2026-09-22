"""Build and install unpublished SDK artifacts outside their source trees."""

import json
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def run(*args, **kwargs):
    subprocess.run(args, check=True, cwd=kwargs.pop("cwd", ROOT), **kwargs)


version = json.loads((ROOT / "api/openapi.json").read_text())["info"]["version"]
assert (
    json.loads((ROOT / "sdk/typescript/package.json").read_text())["version"] == version
)
assert (
    tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    == version
)
assert (
    tomllib.loads((ROOT / "sdk/python/pyproject.toml").read_text())["project"][
        "version"
    ]
    == version
)
with tempfile.TemporaryDirectory(prefix="hudson-sdk-packages-") as directory:
    stage = Path(directory)
    run(
        sys.executable,
        "-m",
        "build",
        "--wheel",
        "--no-isolation",
        "--outdir",
        str(stage),
        str(ROOT / "sdk/python"),
    )
    wheel = next(stage.glob("*.whl"))
    installed = stage / "python"
    run(
        sys.executable,
        "-m",
        "pip",
        "install",
        "--no-deps",
        "--no-index",
        "--target",
        str(installed),
        str(wheel),
    )
    smoke = 'import sys;sys.path.insert(0,sys.argv[1]);import hudson_sandbox as sdk;import importlib.metadata;assert sdk.__version__==sys.argv[2];assert importlib.metadata.version("hudson-sandbox-client")==sys.argv[2];assert sdk.models.CommandInput(argv=["true"],deadline_unix_ms=1).to_wire()["output_limit"]==1048576;assert sdk.__file__.startswith(sys.argv[1])'
    run(sys.executable, "-I", "-c", smoke, str(installed), version)
    run(
        "npm",
        "pack",
        "--pack-destination",
        str(stage),
        "--ignore-scripts",
        cwd=ROOT / "sdk/typescript",
    )
    package = next(stage.glob("*.tgz"))
    consumer = stage / "node"
    consumer.mkdir()
    (consumer / "package.json").write_text('{"private":true,"type":"module"}')
    run(
        "npm",
        "install",
        "--prefix",
        str(consumer),
        "--ignore-scripts",
        "--package-lock=false",
        "--no-audit",
        "--no-fund",
        str(package),
    )
    sample = consumer / "smoke.mts"
    sample.write_text(
        'import {Client, stringify, type CommandInput, type Input} from "@hudson-infinity/sandbox-client";\nconst input: Input<CommandInput> = { argv: ["true"], deadline_unix_ms: 9007199254740993n };\nif (!stringify(input).includes("9007199254740993") || typeof Client !== "function") throw new Error("package smoke failed");\n'
    )
    run(
        str(ROOT / "sdk/typescript/node_modules/.bin/tsc"),
        "--target",
        "ES2024",
        "--module",
        "NodeNext",
        "--moduleResolution",
        "NodeNext",
        "--strict",
        "--typeRoots",
        str(ROOT / "sdk/typescript/node_modules/@types"),
        "--types",
        "node",
        "--outDir",
        str(consumer / "out"),
        str(sample),
    )
    run("node", str(consumer / "out/smoke.mjs"))
print(
    "Unpublished Python wheel and npm tarball install/import/type smoke checks passed."
)
