use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::install_hook::{read_settings, write_settings};
use crate::paths::Paths;

pub const MAX_RETRIES_ENV: &str = "CLAUDE_CODE_MAX_RETRIES";
pub const DEFAULT_MAX_RETRIES: u8 = 1;

pub fn install(paths: &Paths, requested: Option<u8>) -> Result<Vec<String>> {
    let settings_path = paths.claude_settings();
    let (mut settings, existed) = read_settings(&settings_path)?;
    let current = configured(&settings);
    let wanted = requested
        .map(|count| count.to_string())
        .or_else(|| current.clone())
        .unwrap_or_else(|| DEFAULT_MAX_RETRIES.to_string());

    if current.as_deref() == Some(wanted.as_str()) {
        return Ok(vec![format!(
            "{MAX_RETRIES_ENV}={wanted} in {} kept",
            settings_path.display()
        )]);
    }

    env_object(&mut settings, &settings_path)?
        .insert(MAX_RETRIES_ENV.to_owned(), Value::String(wanted.clone()));
    write_settings(&settings_path, &settings, existed)?;
    Ok(vec![format!(
        "{MAX_RETRIES_ENV}={wanted} in {}",
        settings_path.display()
    )])
}

pub fn uninstall(paths: &Paths) -> Result<Vec<String>> {
    let settings_path = paths.claude_settings();
    let (mut settings, existed) = read_settings(&settings_path)?;
    if !existed || configured(&settings).is_none() {
        return Ok(Vec::new());
    }

    let env = env_object(&mut settings, &settings_path)?;
    env.remove(MAX_RETRIES_ENV);
    if env.is_empty() {
        settings.remove("env");
    }
    write_settings(&settings_path, &settings, true)?;
    Ok(vec![format!(
        "{MAX_RETRIES_ENV} removed from {}",
        settings_path.display()
    )])
}

pub fn configured(settings: &Map<String, Value>) -> Option<String> {
    let value = settings.get("env")?.get(MAX_RETRIES_ENV)?;
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn env_object<'a>(
    settings: &'a mut Map<String, Value>,
    settings_path: &std::path::Path,
) -> Result<&'a mut Map<String, Value>> {
    settings
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            Error::Invalid(format!(
                "env in {} is not a JSON object",
                settings_path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env_of(paths: &Paths) -> Option<Value> {
        let (settings, _) = read_settings(&paths.claude_settings()).unwrap();
        settings.get("env").cloned()
    }

    #[test]
    fn default_is_written_once_and_a_chosen_value_is_kept() {
        let paths = Paths::temp("retries-default");
        std::fs::write(
            paths.claude_settings(),
            json!({ "env": { "OTHER": "x" }, "theme": "dark" }).to_string(),
        )
        .unwrap();

        install(&paths, None).unwrap();
        assert_eq!(
            env_of(&paths),
            Some(json!({ "OTHER": "x", MAX_RETRIES_ENV: "1" }))
        );

        install(&paths, Some(4)).unwrap();
        install(&paths, None).unwrap();
        assert_eq!(
            env_of(&paths),
            Some(json!({ "OTHER": "x", MAX_RETRIES_ENV: "4" }))
        );

        uninstall(&paths).unwrap();
        assert_eq!(env_of(&paths), Some(json!({ "OTHER": "x" })));
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn uninstall_drops_an_env_left_empty() {
        let paths = Paths::temp("retries-empty");
        install(&paths, Some(2)).unwrap();
        uninstall(&paths).unwrap();

        let (settings, _) = read_settings(&paths.claude_settings()).unwrap();
        assert!(settings.get("env").is_none());
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn a_non_object_env_is_refused_untouched() {
        let paths = Paths::temp("retries-broken-env");
        std::fs::write(paths.claude_settings(), r#"{"env":"oops"}"#).unwrap();

        assert!(install(&paths, Some(1)).is_err());
        assert_eq!(
            std::fs::read_to_string(paths.claude_settings()).unwrap(),
            r#"{"env":"oops"}"#
        );
        std::fs::remove_dir_all(&paths.home).unwrap();
    }
}
