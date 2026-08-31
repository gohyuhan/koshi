//! Tests for the plugin domain error.

use super::*;

#[test]
fn load_error_display_includes_name_and_detail() {
    let err = PluginError::Load {
        name: "vim-mode".to_string(),
        detail: "wasm module failed to validate".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "failed to load plugin `vim-mode`: wasm module failed to validate"
    );
}

#[test]
fn runtime_error_display_includes_name_and_detail() {
    let err = PluginError::Runtime {
        name: "status-bar".to_string(),
        detail: "trapped: out of bounds memory access".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "plugin `status-bar` runtime error: trapped: out of bounds memory access"
    );
}

#[test]
fn load_error_display_with_empty_name_and_detail() {
    let err = PluginError::Load {
        name: String::new(),
        detail: String::new(),
    };
    assert_eq!(err.to_string(), "failed to load plugin ``: ");
}

#[test]
fn runtime_error_display_with_empty_name_and_detail() {
    let err = PluginError::Runtime {
        name: String::new(),
        detail: String::new(),
    };
    assert_eq!(err.to_string(), "plugin `` runtime error: ");
}

#[test]
fn load_error_display_does_not_escape_backticks_in_name() {
    // The `#[error]` format substitutes `name` as plain text. A backtick in
    // `name` reaches the message unescaped.
    let err = PluginError::Load {
        name: "evil`plugin".to_string(),
        detail: "boom".to_string(),
    };
    assert_eq!(err.to_string(), "failed to load plugin `evil`plugin`: boom");
}

#[test]
fn runtime_error_display_preserves_multibyte_unicode() {
    let err = PluginError::Runtime {
        name: "プラグイン".to_string(),
        detail: "パニック".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "plugin `プラグイン` runtime error: パニック"
    );
}

#[test]
fn load_error_category_is_plugin() {
    let err = PluginError::Load {
        name: "vim-mode".to_string(),
        detail: "boom".to_string(),
    };
    assert_eq!(err.category(), DomainCategory::Plugin);
}

#[test]
fn runtime_error_category_is_plugin() {
    let err = PluginError::Runtime {
        name: "vim-mode".to_string(),
        detail: "boom".to_string(),
    };
    assert_eq!(err.category(), DomainCategory::Plugin);
}

#[test]
fn load_error_severity_is_recoverable() {
    let err = PluginError::Load {
        name: "vim-mode".to_string(),
        detail: "boom".to_string(),
    };
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn runtime_error_severity_is_recoverable() {
    let err = PluginError::Runtime {
        name: "vim-mode".to_string(),
        detail: "boom".to_string(),
    };
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn runtime_error_display_does_not_escape_backticks_in_name() {
    let err = PluginError::Runtime {
        name: "evil`plugin".to_string(),
        detail: "boom".to_string(),
    };
    assert_eq!(err.to_string(), "plugin `evil`plugin` runtime error: boom");
}

#[test]
fn load_error_display_preserves_multibyte_unicode() {
    let err = PluginError::Load {
        name: "プラグイン".to_string(),
        detail: "検証失敗".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "failed to load plugin `プラグイン`: 検証失敗"
    );
}

#[test]
fn load_error_display_substitutes_brace_shaped_fields_verbatim() {
    // `{name}` and `{detail}` are substituted once. Braces inside a field
    // reach the message as plain text.
    let err = PluginError::Load {
        name: "{detail}".to_string(),
        detail: "{name}".to_string(),
    };
    assert_eq!(err.to_string(), "failed to load plugin `{detail}`: {name}");
}

#[test]
fn runtime_error_display_keeps_control_characters_in_detail() {
    let err = PluginError::Runtime {
        name: "status-bar".to_string(),
        detail: "line one\nline two\ttabbed".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "plugin `status-bar` runtime error: line one\nline two\ttabbed"
    );
}

#[test]
fn load_error_has_no_source() {
    let err = PluginError::Load {
        name: "vim-mode".to_string(),
        detail: "boom".to_string(),
    };
    assert!(std::error::Error::source(&err).is_none());
}

#[test]
fn runtime_error_has_no_source() {
    let err = PluginError::Runtime {
        name: "vim-mode".to_string(),
        detail: "boom".to_string(),
    };
    assert!(std::error::Error::source(&err).is_none());
}

#[test]
fn debug_output_names_variant_and_fields() {
    let err = PluginError::Load {
        name: "vim-mode".to_string(),
        detail: "boom".to_string(),
    };
    assert_eq!(
        format!("{err:?}"),
        "Load { name: \"vim-mode\", detail: \"boom\" }"
    );
}
