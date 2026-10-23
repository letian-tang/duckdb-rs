#!/usr/bin/env python3
"""Exercise loader/extension/core archive ordering without rebuilding DuckDB.

On Linux this runs the real rustc final link with the system linker. On macOS
it also checks ELF archive ordering with rust-lld --warn-backrefs, because ld64
resolves backward references and does not expose this GNU ld failure.
"""
import os
from pathlib import Path
import platform
import shlex
import subprocess
import tempfile


def run(args, *, check=True):
    return subprocess.run(args, text=True, capture_output=True, check=check)


with tempfile.TemporaryDirectory(prefix="duckdb-link-order-") as directory:
    root = Path(directory)
    sources = {
        "core": "static int loaded; void reg(int n){loaded=n;} int engine_open(void){return loaded;}",
        "extension": "int duckdb_extension_json_describe(void){return 42;}",
        "loader": "extern int duckdb_extension_json_describe(void); extern void reg(int); __attribute__((constructor)) static void init(void){reg(duckdb_extension_json_describe());}",
    }
    cc = shlex.split(os.environ.get("CC", "cc"))
    ar = shlex.split(os.environ.get("AR", "ar"))
    for name, source in sources.items():
        (root / f"{name}.c").write_text(source)
        run([*cc, "-c", str(root / f"{name}.c"), "-o", str(root / f"{name}.o")])
        run([*ar, "crs", str(root / f"lib{name}.a"), str(root / f"{name}.o")])
    (root / "api.rs").write_text('unsafe extern "C" { fn engine_open() -> i32; } pub fn value() -> i32 { unsafe { engine_open() } }')
    (root / "main.rs").write_text('fn main(){ assert_eq!(probe::value(),42); }')
    for bundle in [True, False]:
        suffix = "old" if bundle else "fixed"
        rlib = root / f"libprobe_{suffix}.rlib"
        modifier = "" if bundle else ",-bundle"
        ordinary = "static=" if bundle else "static:-bundle="
        run(["rustc", "--edition=2024", "--crate-type=rlib", "--crate-name=probe", str(root / "api.rs"), "-Lnative=" + str(root), "-l", f"static:+whole-archive{modifier}=loader", "-l", ordinary + "extension", "-l", ordinary + "core", "-o", str(rlib)])
        result = run(["rustc", "--edition=2024", str(root / "main.rs"), "--extern", f"probe={rlib}", "-Lnative=" + str(root), "--print", "link-args", "-o", str(root / suffix)], check=False)
        if not bundle or platform.system() != "Linux":
            assert result.returncode == 0, result.stderr
            run([str(root / suffix)])
            if not bundle:
                inputs = shlex.split(result.stdout)
                def position(name):
                    return next(i for i, arg in enumerate(inputs) if arg == "-l" + name or arg.endswith("/lib" + name + ".a"))
                assert position("loader") < position("extension") < position("core"), result.stdout
        elif result.returncode == 0:
            print("System linker accepts backward references; testing ELF ordering separately.")
        else:
            assert "duckdb_extension_json_describe" in result.stderr, result.stderr
            print("Original Rust bundle layout: missing extension symbol reproduced.")
    print("Fixed unbundled Rust final link and static initializer: PASS")

    # rust-lld's back-reference check detects inputs GNU ld would have skipped.
    if platform.system() == "Darwin":
        sysroot = Path(run(["rustc", "--print", "sysroot"]).stdout.strip())
        host = next(x[6:] for x in run(["rustc", "-vV"]).stdout.splitlines() if x.startswith("host: "))
        lld = sysroot / "lib/rustlib" / host / "bin/rust-lld"
        os.environ["DYLD_LIBRARY_PATH"] = str(sysroot / "lib") + os.pathsep + os.environ.get("DYLD_LIBRARY_PATH", "")
        for name in sources:
            run(["clang", "--target=x86_64-unknown-linux-gnu", "-c", str(root / f"{name}.c"), "-o", str(root / f"{name}.elf.o")])
        (root / "entry.c").write_text("extern int engine_open(void); void _start(void){engine_open();}")
        run(["clang", "--target=x86_64-unknown-linux-gnu", "-c", str(root / "entry.c"), "-o", str(root / "entry.o")])
        base = [str(lld), "-flavor", "gnu", "--warn-backrefs", "--fatal-warnings", "-o", str(root / "elf-probe"), str(root / "entry.o")]
        loader = [str(root / "loader.elf.o")]
        libs = ["--start-lib", str(root / "extension.elf.o"), "--end-lib", "--start-lib", str(root / "core.elf.o"), "--end-lib"]
        old = run(base + libs + loader, check=False)
        assert old.returncode != 0 and "backward reference" in old.stderr, old.stderr
        run(base + loader + libs)
        print("ELF strict archive-order regression: original FAIL, fixed PASS (rust-lld back-reference check; not GNU ld execution)")
