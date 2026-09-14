#!/usr/bin/env python3
"""Configure WebRTC's Android dist jar without line-number-sensitive patches."""
from pathlib import Path
import re
import sys


path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("sdk/android/BUILD.gn")
text = path.read_text()

dist = re.compile(r'(dist_jar\("libwebrtc"\) \{.*?\n)(    direct_deps_only = )true', re.S)
text, count = dist.subn(r'\1\2false', text, count=1)
if count != 1:
    raise SystemExit("expected exactly one libwebrtc direct_deps_only setting")

marker = '    requires_android = true\n'
rules = '''\n    # Reactor: relocate WebRTC and JNI Zero Java bytecode together.\n    if (android_jni_package_prefix != "") {\n      renaming_rules = [\n        "org.webrtc.**->${android_jni_package_prefix}.org.webrtc",\n        "org.jni_zero.**->${android_jni_package_prefix}.org.jni_zero",\n      ]\n    }\n'''
if rules.strip() not in text:
    pos = text.index(marker, text.index('dist_jar("libwebrtc")')) + len(marker)
    text = text[:pos] + rules + text[pos:]

registration_target = '''  rtc_android_library("reactor_jni_registration_java") {
    sources = [ "src/java/org/webrtc/Empty.java" ]
    srcjar_deps = [ ":libjingle_peerconnection_so__jni_registration" ]
  }

'''
if 'rtc_android_library("reactor_jni_registration_java")' not in text:
    pos = text.index('  dist_jar("libwebrtc")')
    text = text[:pos] + registration_target + text[pos:]

deps_marker = '    deps = [\n'
dist_pos = text.index('dist_jar("libwebrtc")')
deps_list_start = text.index(deps_marker, dist_pos)
deps_pos = deps_list_start + len(deps_marker)
registration_dep = '      ":reactor_jni_registration_java",\n'
if registration_dep not in text[deps_pos : deps_pos + 256]:
    text = text[:deps_pos] + registration_dep + text[deps_pos:]


path.write_text(text)
