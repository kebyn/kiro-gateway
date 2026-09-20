use std::{fs, process::Command};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kiro-gateway"));
    command.env_clear();
    command
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn generate_config_to_stdout_needs_no_runtime_environment() {
    let output = command().arg("--generate-config").output().unwrap();
    assert!(output.status.success(), "{}", output_text(&output.stderr));

    let config: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(config["admin"]["enabled"], true);
    assert_eq!(config["credential_source"], "env");
    assert_eq!(config["client_api_key"].as_str().unwrap().len(), 64);
    assert_eq!(config["admin_api_key"].as_str().unwrap().len(), 64);
    assert!(output_text(&output.stderr).contains("KIRO_ACCESS_TOKEN"));
}

#[test]
fn generate_config_does_not_copy_environment_secrets() {
    let secrets = [
        ("KIRO_CLIENT_API_KEY", "environment-client-secret"),
        ("KIRO_ADMIN_API_KEY", "environment-admin-secret"),
        ("KIRO_ACCESS_TOKEN", "environment-access-secret"),
        ("KIRO_REFRESH_TOKEN", "environment-refresh-secret"),
        ("KIRO_API_KEY", "environment-upstream-secret"),
    ];
    let mut process = command();
    process.arg("--generate-config");
    for (name, value) in secrets {
        process.env(name, value);
    }

    let output = process.output().unwrap();
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    let stdout = output_text(&output.stdout);
    for (_, secret) in secrets {
        assert!(!stdout.contains(secret));
    }
}

#[test]
fn generate_config_creates_a_new_private_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("generated.json");
    let output = command().arg("--generate-config").arg(&path).output().unwrap();
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    assert!(output.stdout.is_empty());

    let config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(config["admin"]["enabled"], true);
    assert_eq!(config["credential_source"], "env");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn generate_config_refuses_to_overwrite_an_existing_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing.json");
    fs::write(&path, "original content\n").unwrap();

    let output = command().arg("--generate-config").arg(&path).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), "original content\n");
    assert!(output_text(&output.stderr).contains("cannot create generated config"));
}

#[test]
fn generate_config_reports_a_missing_parent_directory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing").join("generated.json");

    let output = command().arg("--generate-config").arg(&path).output().unwrap();
    assert!(!output.status.success());
    assert!(output_text(&output.stderr).contains("cannot create generated config"));
}

#[test]
fn generate_config_conflicts_with_check_config() {
    let output = command().args(["--generate-config", "--check-config"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = output_text(&output.stderr);
    assert!(stderr.contains("cannot be used with"));
    assert!(stderr.contains("--generate-config"));
    assert!(stderr.contains("--check-config"));
}

#[test]
fn help_explains_minimum_runtime_credentials() {
    let output = command().arg("--help").output().unwrap();
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    let stdout = output_text(&output.stdout);
    for expected in [
        "KIRO_CLIENT_API_KEY",
        "KIRO_ACCESS_TOKEN",
        "KIRO_REFRESH_TOKEN",
        "KIRO_API_KEY",
        "KIRO_ADMIN_API_KEY",
        "does not create upstream Kiro credentials",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?} in help output");
    }
}
