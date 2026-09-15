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
CONFIGURE = HERE / "configure-android-jni-build.py"
PREFIX = "inc.reactor"
PATH_PREFIX = "inc/reactor/"

# Trimmed to the structure configure-android-jni-build.py keys off; the real
# sdk/android/BUILD.gn differs only in the length of the deps list.
UPSTREAM_BUILD_GN = """\
if (is_android) {
  dist_jar("libwebrtc") {
    _target_dir_name = get_label_info(":$target_name", "dir")
    output = "${root_out_dir}/lib.java${_target_dir_name}/${target_name}.jar"
    direct_deps_only = true
    use_unprocessed_jars = true
    requires_android = true

    deps = [
      ":base_java",
      ":peerconnection_java",
      "../../third_party/jni_zero:jni_zero_java",
    ]
  }

  rtc_android_library("libjingle_peerconnection_java") {
    sources = [ "src/java/org/webrtc/Empty.java" ]
  }
}
"""


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

    def test_generated_jni_wrapper_must_be_included(self):
        self.write_jar(descriptor="Linc/reactor/org/webrtc/JniCommonJni;")
        with self.assertRaisesRegex(
            ValueError, "Missing Android runtime class.*JniCommonJni"
        ):
            checker.check(self.jar, PREFIX)
        name = "inc/reactor/org/webrtc/JniCommonJni"
        with zipfile.ZipFile(self.jar, "a") as archive:
            archive.writestr(name + ".class", class_file(name))
        self.assertEqual(checker.check(self.jar, PREFIX), 3)

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


class AndroidBuildGnTest(unittest.TestCase):
    """The GN edits that decide what lands in the published JAR."""

    def configure(self, source=UPSTREAM_BUILD_GN):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.build_gn = Path(temp.name) / "BUILD.gn"
        self.build_gn.write_text(source)
        result = subprocess.run(
            ["python3", str(CONFIGURE), str(self.build_gn)],
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return self.build_gn.read_text()

    def test_dist_jar_keeps_transitive_jni_wrappers(self):
        self.assertIn("direct_deps_only = false", self.configure())

    def test_dist_jar_relocates_both_owned_namespaces(self):
        configured = self.configure()
        self.assertIn(
            '"org.webrtc.**->${android_jni_package_prefix}.org.webrtc"', configured
        )
        self.assertIn(
            '"org.jni_zero.**->${android_jni_package_prefix}.org.jni_zero"', configured
        )

    def test_registration_srcjar_is_not_consumed_via_srcjar_deps(self):
        # android_library() appends jar_excluded_patterns = [ "*/*GEN_JNI.class" ]
        # to any target whose srcjar_deps mention "jni", and javac applies that
        # exclusion — so GEN_JNI would be stripped before the dist JAR is merged.
        configured = self.configure()
        self.assertNotIn("srcjar_deps =", configured)
        self.assertIn(
            'srcjars = [ "$target_gen_dir/'
            'libjingle_peerconnection_so__jni_registration.srcjar" ]',
            configured,
        )

    def test_gen_jni_target_is_reachable_from_the_dist_jar(self):
        configured = self.configure()
        self.assertIn(
            'rtc_android_library("reactor_jni_registration_java")', configured
        )
        dist_jar_deps = configured[configured.index('dist_jar("libwebrtc")') :]
        self.assertIn('":reactor_jni_registration_java",', dist_jar_deps)

    def test_gen_jni_compiles_against_the_dist_jar_classpath(self):
        # GEN_JNI's native stubs are declared over the SDK's own Java types.
        configured = self.configure()
        registration = configured[
            configured.index('rtc_android_library("reactor_jni_registration_java")') :
            configured.index('dist_jar("libwebrtc")')
        ]
        for dep in (
            '":libjingle_peerconnection_so__jni_registration",',
            '":base_java",',
            '":peerconnection_java",',
            '"../../third_party/jni_zero:jni_zero_java",',
        ):
            self.assertIn(dep, registration)

    def test_rerunning_is_a_no_op(self):
        configured = self.configure()
        self.assertEqual(self.configure(configured), configured)


if __name__ == "__main__":
    unittest.main()
