#!/usr/bin/env python3
"""Configure WebRTC's Android dist jar without line-number-sensitive patches.

`sdk/android/BUILD.gn` churns on every milestone bump, so a context diff for it
goes stale far faster than the rest of patch 0002. The Android-only half of the
JNI package-prefix contract therefore lives here as structural edits:

1. `dist_jar("libwebrtc")` keeps transitive Java deps (`direct_deps_only =
   false`) so the generated `*Jni` wrappers ship inside the JAR.
2. `dist_jar("libwebrtc")` relocates `org.webrtc.*` and `org.jni_zero.*` under
   `android_jni_package_prefix`, matching the class names the JNI codegen looks
   up from native code.
3. A `reactor_jni_registration_java` target pulls `GEN_JNI` — the class every
   generated `*Jni` wrapper calls into — out of the shared library's JNI
   registration srcjar and into the dist JAR.

Usage: configure-android-jni-build.py [path/to/sdk/android/BUILD.gn]
"""
from pathlib import Path
import re
import sys

DIST_JAR = 'dist_jar("libwebrtc")'
REGISTRATION_SRCJAR = "libjingle_peerconnection_so__jni_registration"

path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("sdk/android/BUILD.gn")
text = path.read_text()

# ── the dist JAR's own dependency list ───────────────────────────────────────
# Captured before any edit: it doubles as the javac classpath for GEN_JNI,
# whose native stubs are declared in terms of these targets' types.
dist_pos = text.index(DIST_JAR)
deps_open = text.index("    deps = [\n", dist_pos) + len("    deps = [\n")
deps_close = text.index("    ]\n", deps_open)
dist_jar_deps = text[deps_open:deps_close]

# ── 1. ship the transitive generated *Jni wrappers ───────────────────────────
text, count = re.subn(
    r'(dist_jar\("libwebrtc"\) \{.*?\n)(    direct_deps_only = )true',
    r"\1\2false",
    text,
    count=1,
    flags=re.S,
)
if count != 1 and "    direct_deps_only = false\n" not in text[dist_pos:]:
    raise SystemExit("expected exactly one libwebrtc direct_deps_only setting")

# ── 2. relocate WebRTC and JNI Zero bytecode under the same prefix ───────────
marker = "    requires_android = true\n"
rules = """
    # Reactor: relocate WebRTC and JNI Zero Java bytecode together.
    if (android_jni_package_prefix != "") {
      renaming_rules = [
        "org.webrtc.**->${android_jni_package_prefix}.org.webrtc",
        "org.jni_zero.**->${android_jni_package_prefix}.org.jni_zero",
      ]
    }
"""
if rules.strip() not in text:
    pos = text.index(marker, text.index(DIST_JAR)) + len(marker)
    text = text[:pos] + rules + text[pos:]

# ── 3. carry GEN_JNI into the dist JAR ───────────────────────────────────────
# The srcjar is referenced by PATH, not through srcjar_deps: android_library()
# appends jar_excluded_patterns = [ "*/*GEN_JNI.class" ] to any target whose
# srcjar_deps mention "jni" (build/config/android/rules.gni), and javac itself
# applies that exclusion — so via srcjar_deps GEN_JNI never reaches even the
# unprocessed jar, and every *Jni wrapper in the published JAR dangles.
# Depending on the generating action directly keeps ninja's ordering intact.
registration_target = f"""\
  # Reactor: GEN_JNI declares the native stubs every generated *Jni wrapper
  # calls into. It is produced by the shared library's JNI registration srcjar,
  # which no Java target in this file consumes, so the dist JAR shipped without
  # it. Consume it by path: srcjar_deps would make android_library() strip
  # */*GEN_JNI.class from the compiled jar (see build/config/android/rules.gni).
  rtc_android_library("reactor_jni_registration_java") {{
    srcjars = [ "$target_gen_dir/{REGISTRATION_SRCJAR}.srcjar" ]

    # The registration action, plus the classpath its stubs are declared over.
    deps = [
      ":{REGISTRATION_SRCJAR}",
{dist_jar_deps}    ]
  }}

"""
if 'rtc_android_library("reactor_jni_registration_java")' not in text:
    pos = text.index(f"  {DIST_JAR}")
    text = text[:pos] + registration_target + text[pos:]

deps_marker = "    deps = [\n"
deps_pos = text.index(deps_marker, text.index(DIST_JAR)) + len(deps_marker)
registration_dep = '      ":reactor_jni_registration_java",\n'
if registration_dep not in text[deps_pos : deps_pos + 256]:
    text = text[:deps_pos] + registration_dep + text[deps_pos:]

path.write_text(text)
