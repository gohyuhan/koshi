//! Tests for the plugin domain error.

use super::*;

#[test]
fn load_error_display_includes_plugin_name_and_error_detail() {
    let plugin_error = PluginError::Load {
        plugin_name: "vim-mode".to_string(),
        error_detail: "wasm module failed to validate".to_string(),
    };
    assert_eq!(
        plugin_error.to_string(),
        "failed to load plugin `vim-mode`: wasm module failed to validate"
    );
}

#[test]
fn runtime_error_display_includes_plugin_name_and_error_detail() {
    let plugin_error = PluginError::Runtime {
        plugin_name: "status-bar".to_string(),
        error_detail: "trapped: out of bounds memory access".to_string(),
    };
    assert_eq!(
        plugin_error.to_string(),
        "plugin `status-bar` runtime error: trapped: out of bounds memory access"
    );
}

#[test]
fn load_error_display_with_empty_plugin_name_and_error_detail() {
    let plugin_error = PluginError::Load {
        plugin_name: String::new(),
        error_detail: String::new(),
    };
    assert_eq!(plugin_error.to_string(), "failed to load plugin ``: ");
}

#[test]
fn runtime_error_display_with_empty_plugin_name_and_error_detail() {
    let plugin_error = PluginError::Runtime {
        plugin_name: String::new(),
        error_detail: String::new(),
    };
    assert_eq!(plugin_error.to_string(), "plugin `` runtime error: ");
}

#[test]
fn load_error_display_does_not_escape_backticks_in_plugin_name() {
    // The error format inserts `plugin_name` without escaping backticks.
    let plugin_error = PluginError::Load {
        plugin_name: "evil`plugin".to_string(),
        error_detail: "boom".to_string(),
    };
    assert_eq!(
        plugin_error.to_string(),
        "failed to load plugin `evil`plugin`: boom"
    );
}

#[test]
fn runtime_error_display_preserves_multibyte_unicode() {
    let plugin_error = PluginError::Runtime {
        plugin_name: "プラグイン".to_string(),
        error_detail: "パニック".to_string(),
    };
    assert_eq!(
        plugin_error.to_string(),
        "plugin `プラグイン` runtime error: パニック"
    );
}

#[test]
fn load_error_category_is_plugin() {
    let plugin_error = PluginError::Load {
        plugin_name: "vim-mode".to_string(),
        error_detail: "boom".to_string(),
    };
    assert_eq!(plugin_error.category(), DomainCategory::Plugin);
}

#[test]
fn runtime_error_category_is_plugin() {
    let plugin_error = PluginError::Runtime {
        plugin_name: "vim-mode".to_string(),
        error_detail: "boom".to_string(),
    };
    assert_eq!(plugin_error.category(), DomainCategory::Plugin);
}

#[test]
fn load_error_severity_is_recoverable() {
    let plugin_error = PluginError::Load {
        plugin_name: "vim-mode".to_string(),
        error_detail: "boom".to_string(),
    };
    assert_eq!(plugin_error.get_severity(), Severity::Recoverable);
}

#[test]
fn runtime_error_severity_is_recoverable() {
    let plugin_error = PluginError::Runtime {
        plugin_name: "vim-mode".to_string(),
        error_detail: "boom".to_string(),
    };
    assert_eq!(plugin_error.get_severity(), Severity::Recoverable);
}

#[test]
fn runtime_error_display_does_not_escape_backticks_in_plugin_name() {
    let plugin_error = PluginError::Runtime {
        plugin_name: "evil`plugin".to_string(),
        error_detail: "boom".to_string(),
    };
    assert_eq!(
        plugin_error.to_string(),
        "plugin `evil`plugin` runtime error: boom"
    );
}

#[test]
fn load_error_display_preserves_multibyte_unicode() {
    let plugin_error = PluginError::Load {
        plugin_name: "プラグイン".to_string(),
        error_detail: "検証失敗".to_string(),
    };
    assert_eq!(
        plugin_error.to_string(),
        "failed to load plugin `プラグイン`: 検証失敗"
    );
}

#[test]
fn load_error_display_substitutes_brace_shaped_fields_verbatim() {
    // The format substitutes each field once and leaves braces inside fields
    // as plain text.
    let plugin_error = PluginError::Load {
        plugin_name: "{detail}".to_string(),
        error_detail: "{name}".to_string(),
    };
    assert_eq!(
        plugin_error.to_string(),
        "failed to load plugin `{detail}`: {name}"
    );
}

#[test]
fn runtime_error_display_keeps_control_characters_in_error_detail() {
    let plugin_error = PluginError::Runtime {
        plugin_name: "status-bar".to_string(),
        error_detail: "line one\nline two\ttabbed".to_string(),
    };
    assert_eq!(
        plugin_error.to_string(),
        "plugin `status-bar` runtime error: line one\nline two\ttabbed"
    );
}

#[test]
fn load_error_has_no_source() {
    let plugin_error = PluginError::Load {
        plugin_name: "vim-mode".to_string(),
        error_detail: "boom".to_string(),
    };
    assert!(std::error::Error::source(&plugin_error).is_none());
}

#[test]
fn runtime_error_has_no_source() {
    let plugin_error = PluginError::Runtime {
        plugin_name: "vim-mode".to_string(),
        error_detail: "boom".to_string(),
    };
    assert!(std::error::Error::source(&plugin_error).is_none());
}

#[test]
fn debug_output_names_variant_and_fields() {
    let plugin_error = PluginError::Load {
        plugin_name: "vim-mode".to_string(),
        error_detail: "boom".to_string(),
    };
    assert_eq!(
        format!("{plugin_error:?}"),
        "Load { plugin_name: \"vim-mode\", error_detail: \"boom\" }"
    );
}

#[test]
fn runtime_error_debug_output_names_variant_and_fields() {
    let plugin_error = PluginError::Runtime {
        plugin_name: "vim-mode".to_string(),
        error_detail: "boom".to_string(),
    };
    assert_eq!(
        format!("{plugin_error:?}"),
        "Runtime { plugin_name: \"vim-mode\", error_detail: \"boom\" }"
    );
}
