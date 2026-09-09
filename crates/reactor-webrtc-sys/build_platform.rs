//! Prebuilt names are an ABI contract, not just an OS/architecture mapping.

pub fn prebuilt_platform(
    os: &str,
    arch: &str,
    abi: &str,
    target_env: &str,
) -> Option<&'static str> {
    match os {
        "macos" => match arch {
            "aarch64" => Some("mac-arm64"),
            "x86_64" => Some("mac-x64"),
            _ => None,
        },
        "ios" => {
            // aarch64-apple-ios           → device (abi = "")
            // aarch64-apple-ios-sim       → simulator (abi = "sim")
            // x86_64-apple-ios            → simulator (x64 is always sim)
            let is_sim = abi == "sim" || arch == "x86_64";
            Some(if is_sim {
                "ios-arm64-simulator"
            } else {
                "ios-arm64-device"
            })
        }
        "linux" => match (arch, target_env) {
            ("x86_64", "gnu") => Some("linux-x64"),
            ("aarch64", "gnu") => Some("linux-arm64"),
            ("x86_64", "musl") => Some("linux-musl-x64"),
            ("aarch64", "musl") => Some("linux-musl-arm64"),
            _ => None,
        },
        "android" => match arch {
            "aarch64" => Some("android-arm64"),
            _ => None,
        },
        "windows" => match arch {
            "x86_64" => Some("win-x64"),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::prebuilt_platform;

    #[test]
    fn linux_archives_are_specific_to_libc() {
        for (arch, suffix) in [("x86_64", "x64"), ("aarch64", "arm64")] {
            assert_eq!(
                prebuilt_platform("linux", arch, "", "gnu"),
                Some(format!("linux-{suffix}").as_str())
            );
            assert_eq!(
                prebuilt_platform("linux", arch, "", "musl"),
                Some(format!("linux-musl-{suffix}").as_str())
            );
            assert_eq!(prebuilt_platform("linux", arch, "", ""), None);
            assert_eq!(prebuilt_platform("linux", arch, "", "uclibc"), None);
        }
        assert_eq!(prebuilt_platform("linux", "arm", "", "musl"), None);
    }

    #[test]
    fn existing_platforms_keep_their_names() {
        for (os, arch, abi, target_env, expected) in [
            ("macos", "aarch64", "", "", "mac-arm64"),
            ("macos", "x86_64", "", "", "mac-x64"),
            ("ios", "aarch64", "", "", "ios-arm64-device"),
            ("ios", "aarch64", "sim", "", "ios-arm64-simulator"),
            ("android", "aarch64", "", "", "android-arm64"),
            ("windows", "x86_64", "", "msvc", "win-x64"),
        ] {
            assert_eq!(prebuilt_platform(os, arch, abi, target_env), Some(expected));
        }
    }
}
