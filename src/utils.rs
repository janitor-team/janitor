// Core utility functions migrated from py/janitor/__init__.py

use std::collections::HashMap;

/// The version of the janitor package
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Split environment variables from a command string.
///
/// This function parses a command string that may start with environment variable
/// assignments (in the form KEY=VALUE) and separates them from the actual command.
///
/// # Arguments
/// * `command` - The command string to parse
///
/// # Returns
/// A tuple containing (environment_variables, remaining_command)
///
/// # Examples
/// ```
/// use janitor::utils::splitout_env;
/// use std::collections::HashMap;
///
/// let (env, cmd) = splitout_env("FOO=bar BAZ=qux echo hello");
/// assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
/// assert_eq!(env.get("BAZ"), Some(&"qux".to_string()));
/// assert_eq!(cmd, "echo hello");
/// ```
pub fn splitout_env(command: &str) -> (HashMap<String, String>, String) {
    // Use shlex to properly split the command while respecting quotes
    let args = match shlex::split(command) {
        Some(args) => args,
        None => {
            // If shlex parsing fails, return empty env and original command
            return (HashMap::new(), command.to_string());
        }
    };

    let split = args
        .iter()
        .position(|arg| !arg.contains('='))
        .unwrap_or(args.len());
    let mut env_args = args;
    let remaining_args = env_args.split_off(split);
    let env = env_args
        .iter()
        .filter_map(|arg| arg.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<HashMap<_, _>>();

    // Reconstruct the remaining command
    let remaining_command = if remaining_args.is_empty() {
        String::new()
    } else {
        shlex::try_join(remaining_args.iter().map(|s| s.as_str()))
            .unwrap_or_else(|_| remaining_args.join(" "))
    };

    (env, remaining_command)
}

/// Get the default user agent string for HTTP requests
pub fn default_user_agent() -> String {
    format!("janitor/{}", VERSION)
}

/// Override the User-Agent used by Python-backed HTTP clients.
///
/// Mirrors `py/janitor/__init__.set_user_agent`: rebinds
/// `breezy.transport.http.default_user_agent` and installs an
/// `urllib.request` opener whose default headers carry the value. The
/// Rust ArtifactManager implementations that shell out to breezy (via
/// breezyshim) pick this up transparently.
///
/// `None` restores the default `janitor/{version}` string.
pub fn set_user_agent(user_agent: Option<&str>) -> pyo3::PyResult<()> {
    use pyo3::prelude::*;
    use pyo3::types::PyTuple;

    let ua = user_agent
        .map(String::from)
        .unwrap_or_else(default_user_agent);

    Python::attach(|py| {
        let http = py.import("breezy.transport.http")?;
        let make_default = PyModule::from_code(
            py,
            std::ffi::CString::new("def make(v):\n    return lambda: v\n")
                .unwrap()
                .as_c_str(),
            std::ffi::CString::new("_janitor_ua.py").unwrap().as_c_str(),
            std::ffi::CString::new("_janitor_ua").unwrap().as_c_str(),
        )?;
        let closure = make_default.getattr("make")?.call1((ua.as_str(),))?;
        http.setattr("default_user_agent", closure)?;

        let urllib_http = py.import("breezy.transport.http.urllib")?;
        let handler = urllib_http.getattr("AbstractHTTPHandler")?;
        let defaults = handler.getattr("_default_headers")?;
        defaults.set_item("User-agent", ua.as_str())?;

        let urllib_request = py.import("urllib.request")?;
        let opener = urllib_request.call_method0("build_opener")?;
        let header = PyTuple::new(py, ["User-agent", ua.as_str()])?;
        opener.setattr("addheaders", vec![header])?;
        urllib_request.call_method1("install_opener", (opener,))?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_splitout_env_no_env() {
        let (env, cmd) = splitout_env("echo hello world");
        assert!(env.is_empty());
        assert_eq!(cmd, "echo hello world");
    }

    #[test]
    fn test_splitout_env_single_env() {
        let (env, cmd) = splitout_env("FOO=bar echo hello");
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(cmd, "echo hello");
    }

    #[test]
    fn test_splitout_env_multiple_env() {
        let (env, cmd) = splitout_env("FOO=bar BAZ=qux echo hello world");
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(env.get("BAZ"), Some(&"qux".to_string()));
        assert_eq!(cmd, "echo hello world");
    }

    #[test]
    fn test_splitout_env_quoted_values() {
        let (env, cmd) = splitout_env(r#"FOO="bar baz" echo hello"#);
        assert_eq!(env.get("FOO"), Some(&"bar baz".to_string()));
        assert_eq!(cmd, "echo hello");
    }

    #[test]
    fn test_splitout_env_equals_in_value() {
        let (env, cmd) = splitout_env("URL=http://example.com/path?a=b echo test");
        assert_eq!(
            env.get("URL"),
            Some(&"http://example.com/path?a=b".to_string())
        );
        assert_eq!(cmd, "echo test");
    }

    #[test]
    fn test_splitout_env_only_env() {
        let (env, cmd) = splitout_env("FOO=bar BAZ=qux");
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(env.get("BAZ"), Some(&"qux".to_string()));
        assert_eq!(cmd, "");
    }

    #[test]
    fn test_splitout_env_empty_command() {
        let (env, cmd) = splitout_env("");
        assert!(env.is_empty());
        assert_eq!(cmd, "");
    }

    #[test]
    fn test_splitout_env_equals_in_command() {
        let (env, cmd) = splitout_env("FOO=bar echo a=b");
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
        // The shlex library quotes arguments containing = which is correct behavior
        assert!(cmd == "echo a=b" || cmd == "echo 'a=b'");
    }

    #[test]
    fn set_user_agent_rebinds_breezy_default() {
        use pyo3::prelude::*;

        set_user_agent(Some("janitor-test/1.2.3")).expect("set_user_agent failed");

        Python::attach(|py| {
            let http = py.import("breezy.transport.http").unwrap();
            let seen: String = http
                .getattr("default_user_agent")
                .unwrap()
                .call0()
                .unwrap()
                .extract()
                .unwrap();
            assert_eq!(seen, "janitor-test/1.2.3");
        });
    }
}
