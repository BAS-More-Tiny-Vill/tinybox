"""Verify the shipped module archive layout without compiling native code."""
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parent.parent

class PackageTests(unittest.TestCase):
    def test_arch_package_install_disables_only_landlock_filesystem_sandbox(self):
        """Pacman gets the filesystem-only sandbox exception in nested Arch builds."""
        target = ROOT / "target"; target.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=target) as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            bash = shutil.which("bash")
            self.assertIsNotNone(bash)
            (bin_dir / "bash").symlink_to(bash)
            capture = root / "pacman-args"
            pacman = bin_dir / "pacman"
            pacman.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PACMAN_ARGS\"\n")
            pacman.chmod(0o755)
            env = {"PATH": str(bin_dir), "PACMAN_ARGS": str(capture)}
            result = subprocess.run(
                [str(ROOT / "scripts/install-distro-packages.sh")],
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            args = capture.read_text().splitlines()
            self.assertIn("--disable-sandbox-filesystem", args)
            self.assertNotIn("--disable-sandbox", args)
            self.assertEqual(
                args,
                ["-Syu", "--disable-sandbox-filesystem", "--noconfirm", "base-devel", "cmake",
                 "curl", "git", "gzip", "jq", "openssl", "perl", "util-linux"],
            )

    def test_archive_accepts_dotted_release_host_key_and_attests_library(self):
        """Package a dotted release host key and preserve the archive attestation."""
        target = ROOT / "target"; target.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=target) as directory:
            root = Path(directory); (root/"scripts").mkdir(); (root/"bin").mkdir()
            shutil.copy2(ROOT/"scripts/build-module.sh", root/"scripts/build-module.sh")
            for file in ["LICENSE", "README.md", "MODULE.md"]: (root/file).write_text("fixture")
            cargo = root/"bin/cargo"
            cargo.write_text(
                "#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\n"
                "root=Path.cwd()\n"
                "if 'metadata' in sys.argv: print(json.dumps({'target_directory':str(root/'target')}))\n"
                "else:\n"
                " target=sys.argv[sys.argv.index('--target')+1]\n"
                " output=root/'target'/target/'release'; output.mkdir(parents=True,exist_ok=True)\n"
                " (output/'libtinybox.so').write_bytes(b'compiled-fixture')\n"
            )
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=str(root/"bin")+os.pathsep+os.environ["PATH"])
            env.pop("CARGO_TARGET_DIR", None)
            result = subprocess.run(
                ["bash", str(root/"scripts/build-module.sh"),
                 "--crate", "tinybox-module", "--version", "0.1.22",
                 "--target", "x86_64-unknown-linux-gnu", "--host", "ubuntu-24.04-x86_64"],
                cwd=root, env=env, text=True, capture_output=True, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            archive = root/"release-assets/tinybox-0.1.22-ubuntu-24.04-x86_64.tar.gz"
            with tarfile.open(archive) as package:
                data = package.extractfile("./libtinybox.so").read()
                manifest = tomllib.loads(package.extractfile("./modules.toml").read().decode())
                self.assertEqual(data, b"compiled-fixture")
                self.assertEqual(manifest, {"libtinybox.so": hashlib.sha256(data).hexdigest()})

    def test_linux_verifier_hands_admitted_archive_to_workspace_owner(self):
        """Run the prebuilt admission verifier as the mounted-workspace owner."""
        target = ROOT / "target"; target.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=target) as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            events = root / "handoff-events"
            chown_args = root / "chown-args"
            chown = bin_dir / "chown"
            chown.write_text(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CHOWN_ARGS\"\n"
                "printf 'chown\\n' >> \"$HANDOFF_EVENTS\"\n"
            )
            chown.chmod(0o755)
            setpriv = bin_dir / "setpriv"
            setpriv.write_text(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SET_PRIV_ARGS\"\n"
                "printf 'setpriv\\n' >> \"$HANDOFF_EVENTS\"\n"
                "while [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n"
            )
            setpriv.chmod(0o755)
            cargo = bin_dir / "cargo"
            cargo.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CARGO_ARGS\"\nexit 0\n")
            cargo.chmod(0o755)
            uname = bin_dir / "uname"
            uname.write_text("#!/bin/sh\necho Linux\n")
            uname.chmod(0o755)
            verifier = root / "verify-module-fixture"
            verifier.write_text(
                "#!/bin/sh\n"
                "test -f \"$1\" && test -f \"$(dirname \"$1\")/modules.toml\" && "
                "cmp -s \"$1\" \"$EXPECTED_LIBRARY\"\n"
            )
            verifier.chmod(0o755)

            library_data = b"prebuilt-module-fixture"
            archive = root / "tinybox.tar.gz"
            manifest = f'"libtinybox.so" = "{hashlib.sha256(library_data).hexdigest()}"\n'
            with tarfile.open(archive, "w:gz") as package:
                for name, data in [
                    ("./libtinybox.so", library_data),
                    ("./modules.toml", manifest.encode()),
                ]:
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    package.addfile(info, io.BytesIO(data))

            expected_library = root / "expected.so"
            expected_library.write_bytes(library_data)
            observed_args = root / "setpriv-args"
            cargo_args = root / "cargo-args"
            host_uid = os.getuid() + 1
            host_gid = os.getgid() + 1
            env = dict(
                os.environ,
                HOST_UID=str(host_uid),
                HOST_GID=str(host_gid),
                EXPECTED_LIBRARY=str(expected_library),
                SET_PRIV_ARGS=str(observed_args),
                CHOWN_ARGS=str(chown_args),
                HANDOFF_EVENTS=str(events),
                CARGO_ARGS=str(cargo_args),
                PATH=str(bin_dir) + os.pathsep + os.environ["PATH"],
            )
            result = subprocess.run(
                [
                    str(ROOT / "scripts/verify-module.sh"),
                    "--archive", str(archive),
                    "--uid", env["HOST_UID"],
                    "--gid", env["HOST_GID"],
                    "--verifier-bin", str(verifier),
                ],
                cwd=ROOT, env=env, text=True, capture_output=True, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(observed_args.exists(), "admission verifier did not run via setpriv")
            self.assertFalse(cargo_args.exists(), "prebuilt verifier unexpectedly invoked Cargo")
            self.assertEqual(
                events.read_text().splitlines(),
                ["chown", "setpriv"],
                "the extracted workspace must be handed to its owner before privilege drop",
            )
            actual_chown_args = chown_args.read_text().splitlines()
            actual_args = observed_args.read_text().splitlines()
            self.assertEqual(
                actual_args[:7],
                [
                    "--reuid",
                    str(host_uid),
                    "--regid",
                    str(host_gid),
                    "--clear-groups",
                    "--",
                    str(verifier),
                ],
            )
            extracted_library = Path(actual_args[7])
            self.assertEqual(extracted_library.name, "libtinybox.so")
            self.assertEqual(extracted_library.parent.parent, ROOT / "target")
            self.assertEqual(
                actual_chown_args,
                [f"{host_uid}:{host_gid}", str(extracted_library.parent)],
            )

    def test_linux_verifier_rejects_archive_traversal_before_handoff(self):
        """Reject unsafe archive paths before invoking setpriv or the verifier."""
        target = ROOT / "target"; target.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=target) as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            setpriv_args = root / "setpriv-args"
            setpriv = bin_dir / "setpriv"
            setpriv.write_text(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SET_PRIV_ARGS\"\n"
            )
            setpriv.chmod(0o755)
            uname = bin_dir / "uname"
            uname.write_text("#!/bin/sh\necho Linux\n")
            uname.chmod(0o755)
            archive = root / "unsafe.tar.gz"
            with tarfile.open(archive, "w:gz") as package:
                data = b"escape"
                info = tarfile.TarInfo("../escape")
                info.size = len(data)
                package.addfile(info, io.BytesIO(data))
            env = dict(
                os.environ,
                HOST_UID=str(os.getuid()),
                HOST_GID=str(os.getgid()),
                SET_PRIV_ARGS=str(setpriv_args),
                PATH=str(bin_dir) + os.pathsep + os.environ["PATH"],
            )
            result = subprocess.run(
                [
                    str(ROOT / "scripts/verify-module.sh"),
                    "--archive", str(archive),
                    "--uid", env["HOST_UID"],
                    "--gid", env["HOST_GID"],
                    "--verifier-bin", str(root / "unused-verifier"),
                ],
                cwd=ROOT, env=env, text=True, capture_output=True, check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("archive member escapes the extraction directory", result.stderr)
            self.assertFalse(setpriv_args.exists(), "unsafe archive reached setpriv")

    def test_archive_attests_host_library_bytes_from_a_symlinked_checkout(self):
        """Package the actual build script and verify the shipped bytes and attestation."""
        target = ROOT / "target"; target.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=target) as directory:
            root = Path(directory); (root/"scripts").mkdir(); (root/"bin").mkdir()
            shutil.copy2(ROOT/"scripts/build-module.sh", root/"scripts/build-module.sh")
            for file in ["LICENSE","README.md","MODULE.md"]: (root/file).write_text("fixture")
            cargo = root/"bin/cargo"
            cargo.write_text("#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\n"
                "root=Path.cwd()\n"
                "if 'metadata' in sys.argv: print(json.dumps({'target_directory':str(root/'target')}))\n"
                "else:\n"
                " target=sys.argv[sys.argv.index('--target')+1]\n"
                " output=root/'target'/target/'release'; output.mkdir(parents=True,exist_ok=True)\n"
                " (output/'libtinybox.so').write_bytes(b'compiled-fixture')\n")
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=str(root/"bin")+os.pathsep+os.environ["PATH"])
            env.pop("CARGO_TARGET_DIR", None)
            alias = root/"checkout-link"; alias.symlink_to(root, target_is_directory=True)
            result = subprocess.run(["bash",str(alias/"scripts/build-module.sh"),
                "--crate","tinybox-module","--version","1.2.3-beta.1",
                "--target","x86_64-unknown-linux-gnu","--host","linux-fixture"],
                cwd=root,env=env,text=True,capture_output=True)
            self.assertEqual(result.returncode,0,result.stderr)
            archive=root/"release-assets/tinybox-1.2.3-beta.1-linux-fixture.tar.gz"
            with tarfile.open(archive) as package:
                data=package.extractfile("./libtinybox.so").read()
                manifest=tomllib.loads(package.extractfile("./modules.toml").read().decode())
                self.assertEqual(data,b"compiled-fixture")
                self.assertEqual(manifest,{"libtinybox.so":hashlib.sha256(data).hexdigest()})
                for name in ["./LICENSE","./README.md","./MODULE.md"]:
                    self.assertIsNotNone(package.extractfile(name))

if __name__ == "__main__": unittest.main()
