use std::path::{Path, PathBuf};

use claude_nonstop_e2e::scenario::Scenario;
use serde_json::{Value, json};

const HOOK_TIMEOUT_SECONDS: u64 = 5;
const UNIT_NAME: &str = "claude-nonstop.service";

#[test]
fn a1_install_adds_its_hook_and_retry_limit_next_to_user_settings() {
    let scenario = Scenario::in_tmux().without_install().start();
    let before = with_user_env(&scenario);
    let original = read_bytes(&scenario.sandbox.settings_path());

    let report = nonstop_ok(&scenario, &["install"]);

    let after = scenario.sandbox.settings();
    let stop_failure = after["hooks"]["StopFailure"]
        .as_array()
        .expect("StopFailure hooks are a list");
    assert_eq!(
        stop_failure.first(),
        before["hooks"]["StopFailure"].get(0),
        "the user's StopFailure hook stays first and untouched"
    );
    assert_eq!(
        stop_failure.get(1),
        Some(&json!({"hooks": [{
            "type": "command",
            "command": hook_command(&scenario),
            "timeout": HOOK_TIMEOUT_SECONDS,
        }]})),
        "claude-nonstop appends one group of its own"
    );
    assert_eq!(stop_failure.len(), 2);
    assert_eq!(
        after["hooks"]["SessionStart"],
        before["hooks"]["SessionStart"]
    );
    assert_eq!(after["hooks"]["Stop"], before["hooks"]["Stop"]);
    assert_eq!(
        after["env"],
        json!({"BASH_DEFAULT_TIMEOUT_MS": "120000", "CLAUDE_CODE_MAX_RETRIES": "1"}),
        "the retry limit joins the user's env as a string"
    );
    assert_eq!(
        read_bytes(&backup_path(&scenario)),
        original,
        "the settings Claude Code had before are backed up"
    );
    assert!(report.contains("StopFailure hook added"), "{report}");
}

#[test]
fn a1_second_install_changes_nothing() {
    let scenario = Scenario::in_tmux().without_install().start();
    with_user_env(&scenario);
    let original = read_bytes(&scenario.sandbox.settings_path());
    nonstop_ok(&scenario, &["install"]);
    let installed = read_bytes(&scenario.sandbox.settings_path());

    let report = nonstop_ok(&scenario, &["install"]);

    assert_eq!(
        read_bytes(&scenario.sandbox.settings_path()),
        installed,
        "a repeated install leaves settings.json byte for byte"
    );
    assert_eq!(
        read_bytes(&backup_path(&scenario)),
        original,
        "the backup keeps the settings from before the first install"
    );
    assert!(report.contains("StopFailure hook updated"), "{report}");
    assert!(report.contains("CLAUDE_CODE_MAX_RETRIES=1 in"), "{report}");
    assert!(report.contains("kept"), "{report}");
}

#[test]
fn a1_uninstall_restores_user_settings() {
    let scenario = Scenario::in_tmux().without_install().start();
    let before = with_user_env(&scenario);
    nonstop_ok(&scenario, &["install"]);

    let report = nonstop_ok(&scenario, &["uninstall"]);

    assert_eq!(
        scenario.sandbox.settings(),
        before,
        "uninstall leaves exactly the user's settings"
    );
    assert!(report.contains("StopFailure hook removed"), "{report}");
    assert!(
        report.contains("CLAUDE_CODE_MAX_RETRIES removed"),
        "{report}"
    );
}

#[test]
fn a1_install_daemon_enables_a_user_unit_and_uninstall_disables_it() {
    let scenario = Scenario::in_tmux().without_install().start();
    let unit = scenario
        .sandbox
        .home
        .join(".config/systemd/user")
        .join(UNIT_NAME);

    nonstop_ok(&scenario, &["install", "--daemon"]);

    let unit_text = std::fs::read_to_string(&unit).expect("install --daemon writes the unit");
    assert!(
        unit_text.contains(&format!(
            "ExecStart={} daemon\n",
            binary(&scenario).display()
        )),
        "{unit_text}"
    );
    assert!(
        unit_text.contains(&format!("Environment=PATH={}\n", sandbox_path(&scenario))),
        "{unit_text}"
    );
    assert_eq!(
        systemctl_calls(&scenario),
        [
            vec!["--user", "daemon-reload"],
            vec!["--user", "enable", "--now", UNIT_NAME],
        ]
    );

    nonstop_ok(&scenario, &["uninstall"]);

    assert!(!unit.exists(), "uninstall removes {}", unit.display());
    assert_eq!(
        systemctl_calls(&scenario)[2..],
        [
            vec!["--user", "disable", "--now", UNIT_NAME],
            vec!["--user", "daemon-reload"],
        ]
    );
}

#[test]
fn a1_uninstall_purge_removes_state_and_config() {
    let scenario = Scenario::in_tmux().without_install().start();
    let before = scenario.sandbox.settings();
    nonstop_ok(&scenario, &["install"]);
    nonstop_ok(
        &scenario,
        &["config", "set", "general.auto_resume", "false"],
    );
    let config = PathBuf::from(nonstop_ok(&scenario, &["config", "path"]).trim_end());
    assert!(config.is_file(), "config set writes {}", config.display());
    assert!(scenario.sandbox.state_dir.is_dir());

    let report = nonstop_ok(&scenario, &["uninstall", "--purge"]);

    assert!(
        !scenario.sandbox.state_dir.exists(),
        "purge removes {}",
        scenario.sandbox.state_dir.display()
    );
    assert!(!config.exists(), "purge removes {}", config.display());
    assert_eq!(scenario.sandbox.settings(), before);
    assert!(report.contains("removed"), "{report}");
}

fn with_user_env(scenario: &Scenario) -> Value {
    let mut settings = scenario.sandbox.settings();
    settings["env"] = json!({"BASH_DEFAULT_TIMEOUT_MS": "120000"});
    scenario.sandbox.write_settings(&settings);

    settings
}

fn nonstop_ok(scenario: &Scenario, args: &[&str]) -> String {
    let output = scenario.sandbox.nonstop(args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert!(
        output.status.success(),
        "claude-nonstop {} failed: {stdout}{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );

    stdout
}

fn binary(scenario: &Scenario) -> PathBuf {
    let link = scenario.sandbox.bin.join("claude-nonstop");

    std::fs::canonicalize(&link)
        .unwrap_or_else(|error| panic!("resolve {}: {error}", link.display()))
}

fn hook_command(scenario: &Scenario) -> String {
    let binary = binary(scenario);
    let binary = binary.display();

    format!("test -x {binary} && {binary} hook || exit 0")
}

fn backup_path(scenario: &Scenario) -> PathBuf {
    scenario
        .sandbox
        .claude_config
        .join("settings.json.claude-nonstop-orig")
}

fn sandbox_path(scenario: &Scenario) -> &str {
    scenario
        .sandbox
        .env()
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.as_str())
        .expect("the sandbox sets PATH")
}

fn systemctl_calls(scenario: &Scenario) -> Vec<Vec<String>> {
    scenario
        .sandbox
        .calls("systemctl")
        .into_iter()
        .map(|call| call.argv)
        .collect()
}

fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}
