#!/usr/bin/env python3
"""Validate the Java half of the Android JNI package-prefix contract."""

import argparse
import re
import struct
import zipfile
from pathlib import Path


def linkage_constants(data):
    """Read runtime class/name/descriptor references, excluding debug-only UTF8."""
    if data[:4] != b"\xca\xfe\xba\xbe":
        raise ValueError("Invalid Java class file")
    count = struct.unpack_from(">H", data, 8)[0]
    offset, index = 10, 1
    strings, references = {}, set()
    widths = {
        3: 4,
        4: 4,
        5: 8,
        6: 8,
        7: 2,
        8: 2,
        9: 4,
        10: 4,
        11: 4,
        12: 4,
        15: 3,
        16: 2,
        17: 4,
        18: 4,
        19: 2,
        20: 2,
    }
    while index < count:
        tag = data[offset]
        offset += 1
        if tag == 1:
            size = struct.unpack_from(">H", data, offset)[0]
            offset += 2
            if offset + size > len(data):
                raise ValueError("Truncated Java UTF8 constant")
            strings[index] = data[offset : offset + size]
            offset += size
        elif tag in widths:
            if tag in (7, 16):
                references.add(struct.unpack_from(">H", data, offset)[0])
            elif tag == 12:
                references.add(struct.unpack_from(">H", data, offset + 2)[0])
            offset += widths[tag]
            if tag in (5, 6):
                index += 1
        else:
            raise ValueError(f"Unknown Java constant tag {tag}")
        if offset > len(data):
            raise ValueError("Truncated Java constant pool")
        index += 1
    interfaces = struct.unpack_from(">H", data, offset + 6)[0]
    offset += 8 + interfaces * 2
    for _ in range(2):  # field_info, then method_info
        members = struct.unpack_from(">H", data, offset)[0]
        offset += 2
        for _ in range(members):
            _, _, descriptor, attributes = struct.unpack_from(">HHHH", data, offset)
            references.add(descriptor)
            offset += 8
            for _ in range(attributes):
                length = struct.unpack_from(">I", data, offset + 2)[0]
                offset += 6 + length
                if offset > len(data):
                    raise ValueError("Truncated Java member attribute")
    for reference in references:
        if reference not in strings:
            raise ValueError("Invalid Java UTF8 reference")
        yield strings[reference]


def check(jar, prefix):
    if not re.fullmatch(r"[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*", prefix):
        raise ValueError("Expected a nonempty Java package prefix")
    path_prefix = prefix.replace(".", "/") + "/"
    with zipfile.ZipFile(jar) as archive:
        names = archive.namelist()
        if len(names) != len(set(names)):
            raise ValueError("Duplicate entries in Android JAR")
        for bootstrap in ("org/webrtc/WebRtcClassLoader", "org/jni_zero/JniZero"):
            expected = path_prefix + bootstrap + ".class"
            if expected not in names:
                raise ValueError(f"Missing native bootstrap class: {expected}")
        classes = [name for name in names if name.endswith(".class")]
        for name in classes:
            if name.startswith(("org/webrtc/", "org/jni_zero/")):
                raise ValueError(f"Unrelocated Java class: {name}")
            for value in linkage_constants(archive.read(name)):
                # Check runtime linkage descriptors too: renaming ZIP entries
                # alone leaves bytecode pointing at the old namespace.
                # Debug tables and literal log tags may retain upstream names.
                for package in (
                    "org/webrtc/",
                    "org/jni_zero/",
                    "org.webrtc.",
                    "org.jni_zero.",
                ):
                    marker = package.encode()
                    full = (
                        path_prefix if "/" in package else prefix + "."
                    ).encode() + marker
                    if marker in value.replace(full, b""):
                        raise ValueError(
                            f"Unrelocated bytecode reference in {name}: {value!r}"
                        )
        return len(classes)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("jar", type=Path)
    parser.add_argument("gn_args", type=Path)
    args = parser.parse_args()
    try:
        match = re.search(
            r'^\s*android_jni_package_prefix\s*=\s*"([^"]+)"',
            args.gn_args.read_text(),
            re.MULTILINE,
        )
        if not match:
            raise ValueError("args.gn must declare android_jni_package_prefix")
        count = check(args.jar, match[1])
        print(f"Android JNI JAR verified: {count} classes, prefix {match[1]}")
    except (ValueError, OSError, IndexError, struct.error, zipfile.BadZipFile) as error:
        parser.exit(1, f"Android JAR validation failed: {error}\n")
