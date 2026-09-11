#!/usr/bin/env python3
"""Regression tests for Android package failures before archive publication."""

import importlib.util
import shutil
import struct
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location(
    "check_android_jar", HERE / "check-android-jar.py"
)
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
PREFIX = "inc.reactor"
PATH_PREFIX = "inc/reactor/"


def class_file(name, descriptor="Ljava/lang/Object;"):
    def utf8(text):
        value = text.encode()
        return b"\x01" + struct.pack(">H", len(value)) + value

    # A class with a field: descriptors must be checked even without Fieldref.
    pool = utf8(name) + b"\x07\x00\x01" + utf8("java/lang/Object") + b"\x07\x00\x03"
    pool += utf8("field") + utf8(descriptor)
    return (
        b"\xca\xfe\xba\xbe\x00\x00\x00\x34"
        + struct.pack(">H", 7)
        + pool
        + struct.pack(">HHHHH", 1, 2, 4, 0, 1)
        + struct.pack(">HHHH", 1, 5, 6, 0)
        + b"\x00\x00\x00\x00"
    )


class AndroidJarTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.jar = self.root / "libwebrtc.jar"

    def write_jar(
        self, prefix=PATH_PREFIX, descriptor="Ljava/lang/Object;", omit_jni=False
    ):
        with zipfile.ZipFile(self.jar, "w") as archive:
            for name in ("org/webrtc/WebRtcClassLoader", "org/jni_zero/JniZero"):
                if omit_jni and "jni_zero" in name:
                    continue
                archive.writestr(
                    prefix + name + ".class", class_file(prefix + name, descriptor)
                )

    def test_matching_jar(self):
        self.write_jar(descriptor="Linc/reactor/org/webrtc/WebRtcClassLoader;")
        self.assertEqual(checker.check(self.jar, PREFIX), 2)

    def test_p6_unrelocated_jar(self):
        self.write_jar(prefix="")
        with self.assertRaisesRegex(ValueError, "Missing native bootstrap"):
            checker.check(self.jar, PREFIX)

    def test_renaming_only_zip_entries_is_not_enough(self):
        self.write_jar(descriptor="Lorg/webrtc/WebRtcClassLoader;")
        with self.assertRaisesRegex(ValueError, "bytecode reference"):
            checker.check(self.jar, PREFIX)

    def test_jni_runtime_must_be_relocated_too(self):
        self.write_jar(omit_jni=True)
        with self.assertRaisesRegex(ValueError, "org/jni_zero/JniZero"):
            checker.check(self.jar, PREFIX)

    def test_no_unrelocated_classes_can_remain(self):
        self.write_jar()
        with zipfile.ZipFile(self.jar, "a") as archive:
            archive.writestr("org/webrtc/Other.class", class_file("org/webrtc/Other"))
        with self.assertRaisesRegex(ValueError, "Unrelocated Java class"):
            checker.check(self.jar, PREFIX)

    def test_packager_fails_before_archiving_missing_or_wrong_jar(self):
        scripts = self.root / "webrtc-build"
        scripts.mkdir()
        for filename in ("package.sh", "check-android-jar.py"):
            shutil.copy(HERE / filename, scripts / filename)
        (self.root / "WEBRTC_VERSION").write_text("REACTOR_PATCH_LEVEL=7\n")
        output = scripts / "out/android-arm64-release"
        (output / "dist/lib").mkdir(parents=True)
        (output / "dist/lib/libwebrtc.a").touch()
        (output / "args.gn").write_text('android_jni_package_prefix = "inc.reactor"\n')
        jar = output / "lib.java/sdk/android/libwebrtc.jar"
        for present in (False, True):
            if present:
                self.write_jar(prefix="")
                jar.parent.mkdir(parents=True)
                shutil.copy(self.jar, jar)
            result = subprocess.run(
                ["bash", str(scripts / "package.sh"), "android", "arm64"],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Android JAR validation failed", result.stderr)
            self.assertFalse((scripts / "dist").exists())


if __name__ == "__main__":
    unittest.main()
