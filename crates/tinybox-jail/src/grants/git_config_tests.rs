//! Git include decoding and malformed-value rejection coverage.
use super::*;
use tinybox_bus::ProtectedPaths;

#[test]
fn include_values_decode_git_escapes_and_preserve_quoted_whitespace() {
    assert_eq!(
        include_value(r##"  " a\n\t\b\\\"#; " # comment"##),
        Some(" a\n\t\u{0008}\\\"#; ".into())
    );
    assert_eq!(include_value("  safe  ; comment"), Some("safe".into()));
    assert_eq!(include_value(r#"a""b"#), Some("ab".into()));
}

#[test]
fn malformed_or_empty_include_values_are_not_granted() {
    for value in [
        "",
        " # comment",
        "; comment",
        "\"\"",
        "\"unterminated",
        "\\",
        r"\q",
    ] {
        assert_eq!(include_value(value), None, "{value}");
    }
}

#[test]
fn empty_xdg_config_home_is_unset_in_git_root_selection() {
    assert_eq!(xdg_root(Some(std::ffi::OsStr::new(""))), None);
    assert_eq!(xdg_root(None), None);
    assert_eq!(xdg_root(Some(std::ffi::OsStr::new("config-root"))), None);
}

#[test]
fn git_include_values_join_continuation_lines() {
    let config = format!("[include]\npath = ~/work.{}\nconf\n", char::from(92));
    assert_eq!(include_paths(&config), ["~/work.conf"]);
    assert_eq!(
        include_paths("[include]\npath = \"~/work.\\\nconf\"\n"),
        ["~/work.conf"]
    );
    assert_eq!(
        include_paths("[include]\npath = first # comment\\\npath = second\n"),
        ["first", "second"]
    );
    assert_eq!(
        include_paths("[include]\npath = first\\\\\npath = second\n"),
        ["first\\", "second"]
    );
    assert_eq!(
        include_paths("[include]\npath = dangling\\"),
        Vec::<String>::new()
    );
}

#[test]
fn absolute_xdg_config_home_is_used() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    assert_eq!(xdg_root(Some(root.path().as_os_str())), Some(root.path()));
    Ok(())
}

#[test]
fn absolute_xdg_config_home_replaces_home_config_root() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let xdg = tempfile::tempdir()?;
    std::fs::create_dir_all(home.path().join(".config/git"))?;
    std::fs::create_dir_all(xdg.path().join("git"))?;
    std::fs::write(home.path().join(".config/git/config"), "")?;
    std::fs::write(xdg.path().join("git/config"), "")?;
    std::fs::write(home.path().join(".config/git/ignore"), "")?;
    std::fs::write(xdg.path().join("git/ignore"), "")?;
    let protected = ProtectedPaths::default();
    let mut builder = Builder::new(Some(home.path()), &protected);
    builder.add_gitconfig_at(home.path(), Some(xdg.path().as_os_str()))?;
    assert!(
        builder
            .fixed_read_only
            .contains(&xdg.path().join("git/config"))
    );
    assert!(
        !builder
            .fixed_read_only
            .contains(&home.path().join(".config/git/config"))
    );
    assert!(
        builder
            .grants
            .read_only
            .contains(&xdg.path().join("git/ignore").canonicalize()?)
    );
    assert!(
        !builder
            .grants
            .read_only
            .contains(&home.path().join(".config/git/ignore").canonicalize()?)
    );
    Ok(())
}

#[test]
fn conditional_include_sections_accept_horizontal_tabs() {
    assert_eq!(
        include_paths("[includeIf\t\"gitdir:~/work/\"]\npath = ~/conditional.conf\n"),
        ["~/conditional.conf"]
    );
}

#[test]
fn include_path_on_section_header_is_discovered() {
    assert_eq!(
        include_paths("[include] path = ~/shared.conf\n"),
        ["~/shared.conf"]
    );
    assert_eq!(
        include_paths("[includeIf \"gitdir:~/work/\"] path = ../shared.conf\n"),
        ["../shared.conf"]
    );
}
