use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::atomic_file;
use crate::error::{Error, Result};
use crate::paths::Paths;

pub const HOOK_MARKER: &str = "claude-nonstop hook";
const HOOK_TIMEOUT: u64 = 5;

pub fn hook_command(binary: &Path) -> String {
    let path = binary.display();
    format!("test -x {path} && {path} hook || exit 0")
}

fn is_generated(command: &str) -> bool {
    let Some(rest) = command.strip_prefix("test -x ") else {
        return false;
    };
    let Some((binary, _)) = rest.split_once(" && ") else {
        return false;
    };

    command == hook_command(Path::new(binary))
}

pub fn install(paths: &Paths, binary: &Path) -> Result<Vec<String>> {
    let settings_path = paths.claude_settings();
    let mut messages = Vec::new();
    let (mut settings, existed) = read_settings(&settings_path)?;
    if existed {
        let backup = backup_path(&settings_path);
        if !backup.exists() {
            let bytes = std::fs::read(&settings_path)
                .map_err(|source| Error::io(&settings_path, "read", source))?;
            atomic_file::write(&backup, &bytes, mode_of(&settings_path))?;
            messages.push(format!("backup {}", backup.display()));
        }
    }

    if installed_commands(&settings)
        .iter()
        .any(|command| !is_generated(command))
    {
        messages.push(format!(
            "StopFailure hook is customized in {}, left as is",
            settings_path.display()
        ));
        return Ok(messages);
    }

    let (before, after) = {
        let groups = stop_failure_groups(&mut settings);
        let before = groups.len();
        prune_ours(groups);
        groups.push(json!({ "hooks": [{ "type": "command", "command": hook_command(binary), "timeout": HOOK_TIMEOUT }] }));
        (before, groups.len())
    };
    write_settings(&settings_path, &settings, existed)?;
    messages.push(format!(
        "StopFailure hook {} in {}",
        if before == after { "updated" } else { "added" },
        settings_path.display()
    ));
    Ok(messages)
}

pub fn uninstall(paths: &Paths) -> Result<Vec<String>> {
    let settings_path = paths.claude_settings();
    let (mut settings, existed) = read_settings(&settings_path)?;
    if !existed {
        return Ok(Vec::new());
    }
    let had = installed_commands(&settings).len();
    if had == 0 {
        return Ok(Vec::new());
    }
    let groups = stop_failure_groups(&mut settings);
    prune_ours(groups);
    let hooks = settings.get_mut("hooks").and_then(Value::as_object_mut);
    if let Some(hooks) = hooks {
        if hooks
            .get("StopFailure")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            hooks.remove("StopFailure");
        }
        if hooks.is_empty() {
            settings.remove("hooks");
        }
    }
    write_settings(&settings_path, &settings, true)?;
    Ok(vec![format!(
        "StopFailure hook removed from {}",
        settings_path.display()
    )])
}

pub fn installed_commands(settings: &Map<String, Value>) -> Vec<String> {
    settings
        .get("hooks")
        .and_then(|hooks| hooks.get("StopFailure"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|entry| entry.get("command").and_then(Value::as_str))
        .filter(|command| command.contains(HOOK_MARKER))
        .map(str::to_owned)
        .collect()
}

pub fn read_settings(path: &Path) -> Result<(Map<String, Value>, bool)> {
    let Some(bytes) = atomic_file::read_optional(path)? else {
        return Ok((Map::new(), false));
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })?;
    match value {
        Value::Object(map) => Ok((map, true)),
        _ => Err(Error::Invalid(format!(
            "{} is not a JSON object",
            path.display()
        ))),
    }
}

fn stop_failure_groups(settings: &mut Map<String, Value>) -> &mut Vec<Value> {
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let groups = hooks
        .as_object_mut()
        .expect("hooks is an object")
        .entry("StopFailure")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !groups.is_array() {
        *groups = Value::Array(Vec::new());
    }
    groups.as_array_mut().expect("StopFailure is an array")
}

fn prune_ours(groups: &mut Vec<Value>) {
    for group in groups.iter_mut() {
        if let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            entries.retain(|entry| {
                !entry
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(HOOK_MARKER))
            });
        }
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|entries| !entries.is_empty())
    });
}

pub fn write_settings(path: &Path, settings: &Map<String, Value>, existed: bool) -> Result<()> {
    if !existed {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| Error::io(parent, "create", source))?;
        }
    }
    let mut text =
        serde_json::to_string_pretty(&Value::Object(settings.clone())).map_err(|source| {
            Error::Json {
                path: path.to_path_buf(),
                source,
            }
        })?;
    text.push('\n');
    let mode = if existed { mode_of(path) } else { 0o600 };
    atomic_file::write(path, text.as_bytes(), mode)
}

fn backup_path(settings_path: &Path) -> PathBuf {
    let mut name = settings_path.as_os_str().to_owned();
    name.push(".claude-nonstop-orig");
    PathBuf::from(name)
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(name: &str) -> Paths {
        let dir =
            std::env::temp_dir().join(format!("claude-nonstop-hook-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        Paths {
            home: dir.clone(),
            state: dir.join("state"),
            config_file: dir.join("config.toml"),
            claude_config: dir.join(".claude"),
        }
    }

    const EXISTING: &str = r#"{
  "permissions": {
    "defaultMode": "auto"
  },
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash ~/.claude/hooks/events-to-socket.sh"
          }
        ]
      }
    ],
    "StopFailure": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "python3 /home/me/.local/bin/tg-bridge-stop-failure-hook",
            "timeout": 30
          }
        ]
      },
      {
        "hooks": [
          {
            "type": "command",
            "command": "python3 /home/me/.local/bin/claude-api-retry-hook",
            "timeout": 30
          }
        ]
      }
    ]
  },
  "theme": "dark"
}
"#;

    #[test]
    fn install_appends_our_group_keeps_others_and_is_idempotent() {
        let paths = paths_in("append");
        let settings = paths.claude_settings();
        std::fs::write(&settings, EXISTING).unwrap();
        install(&paths, Path::new("/usr/local/bin/claude-nonstop")).unwrap();
        install(&paths, Path::new("/opt/bin/claude-nonstop")).unwrap();
        let (map, _) = read_settings(&settings).unwrap();
        let groups = map["hooks"]["StopFailure"].as_array().unwrap();
        assert_eq!(groups.len(), 3);
        assert_eq!(
            groups[0]["hooks"][0]["command"],
            "python3 /home/me/.local/bin/tg-bridge-stop-failure-hook"
        );
        assert_eq!(
            groups[1]["hooks"][0]["command"],
            "python3 /home/me/.local/bin/claude-api-retry-hook"
        );
        assert_eq!(
            groups[2]["hooks"][0]["command"],
            "test -x /opt/bin/claude-nonstop && /opt/bin/claude-nonstop hook || exit 0"
        );
        assert!(groups[2].get("matcher").is_none());
        let keys: Vec<&String> = map.keys().collect();
        assert_eq!(keys, vec!["permissions", "hooks", "theme"]);
        assert_eq!(
            std::fs::read_to_string(backup_path(&settings)).unwrap(),
            EXISTING
        );
        let text = std::fs::read_to_string(&settings).unwrap();
        assert!(text.ends_with("}\n"));
        assert!(text.contains("\n  \"permissions\": {\n    \"defaultMode\": \"auto\"\n  },"));
        uninstall(&paths).unwrap();
        let (map, _) = read_settings(&settings).unwrap();
        assert_eq!(map["hooks"]["StopFailure"].as_array().unwrap().len(), 2);
        assert!(installed_commands(&map).is_empty());
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn install_keeps_a_customized_hook_command() {
        let paths = paths_in("custom");
        let settings = paths.claude_settings();
        let custom =
            "if test -f /run/policy; then audit nonstop; else /bin/claude-nonstop hook; fi";
        std::fs::write(
            &settings,
            json!({ "hooks": { "StopFailure": [{ "hooks": [{ "type": "command", "command": custom }] }] } })
                .to_string(),
        )
        .unwrap();

        install(&paths, Path::new("/bin/claude-nonstop")).unwrap();

        let (map, _) = read_settings(&settings).unwrap();
        assert_eq!(installed_commands(&map), vec![custom.to_owned()]);
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn missing_settings_file_is_created_and_fully_removed_on_uninstall() {
        let paths = paths_in("fresh");
        let settings = paths.claude_settings();
        install(&paths, Path::new("/bin/claude-nonstop")).unwrap();
        let (map, _) = read_settings(&settings).unwrap();
        assert_eq!(installed_commands(&map).len(), 1);
        assert!(!backup_path(&settings).exists());
        uninstall(&paths).unwrap();
        let (map, _) = read_settings(&settings).unwrap();
        assert!(map.get("hooks").is_none());
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn invalid_json_is_refused_untouched() {
        let paths = paths_in("invalid");
        let settings = paths.claude_settings();
        std::fs::write(&settings, "{not json").unwrap();
        assert!(install(&paths, Path::new("/bin/claude-nonstop")).is_err());
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{not json");
        std::fs::remove_dir_all(&paths.home).unwrap();
    }
}
