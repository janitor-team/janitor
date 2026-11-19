use crate::*;

#[test]
fn test_default_user_agent() {
    assert!(DEFAULT_USER_AGENT.starts_with("janitor/worker"));
    assert!(DEFAULT_USER_AGENT.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_app_state_default() {
    let state = AppState::default();
    assert!(state.output_directory.is_none());
    assert!(state.assignment.is_none());
    assert!(state.metadata.is_none());
}

#[test]
fn test_app_state_clone() {
    let mut state = AppState::default();
    state.output_directory = Some(std::path::PathBuf::from("/tmp/output"));

    let cloned = state.clone();
    assert_eq!(state.output_directory, cloned.output_directory);
}

#[test]
fn test_dpkg_architecture_error_display() {
    let err = DpkgArchitectureError::MissingCommand;
    assert_eq!(
        err.to_string(),
        "dpkg-architecture command not found; is dpkg-dev installed?"
    );

    let err = DpkgArchitectureError::Other("Custom error".to_string());
    assert_eq!(err.to_string(), "Custom error");
}

#[test]
fn test_convert_codemod_script_failed_command_not_found() {
    let failure = convert_codemod_script_failed(127, "missing-command");
    assert_eq!(failure.code, "command-not-found");
    assert_eq!(failure.description, "Command missing-command not found");
    assert_eq!(failure.stage, vec!["codemod"]);
}

#[test]
fn test_convert_codemod_script_failed_killed() {
    let failure = convert_codemod_script_failed(137, "test-command");
    assert_eq!(failure.code, "killed");
    assert_eq!(failure.description, "Process was killed (by OOM killer?)");
    assert_eq!(failure.stage, vec!["codemod"]);
}

#[test]
fn test_convert_codemod_script_failed_generic() {
    let failure = convert_codemod_script_failed(1, "test-script");
    assert_eq!(failure.code, "command-failed");
    assert_eq!(
        failure.description,
        "Script test-script failed to run with code 1"
    );
    assert_eq!(failure.stage, vec!["codemod"]);
}

#[test]
fn test_convert_codemod_script_failed_various_codes() {
    // Test various exit codes
    for code in [2, 42, 255] {
        let failure = convert_codemod_script_failed(code, "script");
        assert_eq!(failure.code, "command-failed");
        assert!(failure.description.contains(&code.to_string()));
    }
}

// Tests for serde_json_to_py and py_to_serde_json require PyO3 runtime
// These would be better as integration tests

#[cfg(feature = "cli")]
#[test]
fn test_serde_json_conversions() {
    use pyo3::prelude::*;


    Python::attach(|py| {
        // Test null conversion
        let null_val = serde_json::Value::Null;
        let py_null = serde_json_to_py(&null_val);
        assert!(py_null.bind(py).is_none());

        // Test bool conversion
        let bool_val = serde_json::Value::Bool(true);
        let py_bool = serde_json_to_py(&bool_val);
        assert!(py_bool.bind(py).is_truthy().unwrap());

        // Test number conversion
        let num_val = serde_json::Value::Number(serde_json::Number::from(42));
        let py_num = serde_json_to_py(&num_val);
        assert_eq!(py_num.bind(py).extract::<f64>().unwrap(), 42.0);

        // Test string conversion
        let str_val = serde_json::Value::String("test".to_string());
        let py_str = serde_json_to_py(&str_val);
        assert_eq!(py_str.bind(py).extract::<String>().unwrap(), "test");

        // Test array conversion
        let arr_val = serde_json::json!([1, 2, 3]);
        let py_arr = serde_json_to_py(&arr_val);
        let list = py_arr.bind(py).cast::<pyo3::types::PyList>().unwrap();
        assert_eq!(list.len(), 3);

        // Test object conversion
        let obj_val = serde_json::json!({"key": "value"});
        let py_obj = serde_json_to_py(&obj_val);
        let dict = py_obj.bind(py).cast::<pyo3::types::PyDict>().unwrap();
        assert_eq!(
            dict.get_item("key")
                .unwrap()
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "value"
        );
    });
}

#[cfg(feature = "cli")]
#[test]
fn test_py_to_serde_json_basic() {
    use pyo3::prelude::*;


    Python::attach(|py| {
        // Test None
        let py_none = py.None();
        let json_val = py_to_serde_json(py_none.bind(py)).unwrap();
        assert_eq!(json_val, serde_json::Value::Null);

        // Test bool
        let py_bool = pyo3::types::PyBool::new(py, false);
        let json_val = py_to_serde_json(&py_bool).unwrap();
        assert_eq!(json_val, serde_json::Value::Bool(false));

        // Test float
        let py_float = pyo3::types::PyFloat::new(py, 3.14);
        let json_val = py_to_serde_json(&py_float).unwrap();
        assert!(json_val.is_number());

        // Test string
        let py_str = pyo3::types::PyString::new(py, "hello");
        let json_val = py_to_serde_json(&py_str).unwrap();
        assert_eq!(json_val, serde_json::Value::String("hello".to_string()));

        // Test list
        let py_list = pyo3::types::PyList::new(py, [1, 2, 3]).unwrap();
        let json_val = py_to_serde_json(&py_list).unwrap();
        assert!(json_val.is_array());
        assert_eq!(json_val.as_array().unwrap().len(), 3);

        // Test dict
        let py_dict = pyo3::types::PyDict::new(py);
        py_dict.set_item("test", "value").unwrap();
        let json_val = py_to_serde_json(&py_dict).unwrap();
        assert!(json_val.is_object());
        assert_eq!(
            json_val.get("test").unwrap(),
            &serde_json::Value::String("value".to_string())
        );
    });
}

#[test]
fn test_is_gce_instance_non_gce() {
    // This test will fail on actual GCE instances
    // In most test environments, metadata.google.internal won't resolve
    match is_gce_instance() {
        Ok(on_gce) => assert!(!on_gce, "should be false in non-GCE environments"),
        // A sandbox with no resolver at all fails the lookup outright.
        Err(_) => {}
    }
}

#[tokio::test]
async fn test_gce_external_ip_non_gce() {
    // This test expects to fail when not on GCE
    let result = gce_external_ip().await;
    // Should either fail to connect or return None
    assert!(result.is_err() || result.unwrap().is_none());
}
