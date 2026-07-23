/// macOS owns Portal's proven CoreGraphics relative-pointer capture path.
/// Every other platform must opt into its still-experimental Tao + browser
/// Pointer Lock contract at build time.
pub(crate) const fn relative_pointer_capture_enabled_for(
    target_is_macos: bool,
    experimental_non_macos: bool,
) -> bool {
    target_is_macos || experimental_non_macos
}

pub(crate) const fn relative_pointer_capture_enabled() -> bool {
    relative_pointer_capture_enabled_for(
        cfg!(target_os = "macos"),
        cfg!(feature = "experimental-non-macos-pointer-capture"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_macos_relative_pointer_capture_requires_the_experimental_feature() {
        assert!(relative_pointer_capture_enabled_for(true, false));
        assert!(relative_pointer_capture_enabled_for(true, true));
        assert!(!relative_pointer_capture_enabled_for(false, false));
        assert!(relative_pointer_capture_enabled_for(false, true));
    }

    #[test]
    fn compiled_policy_matches_target_and_feature_configuration() {
        assert_eq!(
            relative_pointer_capture_enabled(),
            cfg!(target_os = "macos") || cfg!(feature = "experimental-non-macos-pointer-capture")
        );
    }
}
