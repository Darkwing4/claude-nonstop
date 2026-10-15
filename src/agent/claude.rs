use super::Agent;

const FOREGROUND_COMMANDS: &[&str] = &["node", "bun"];

pub struct Claude;

impl Agent for Claude {
    fn binary_name(&self) -> &'static str {
        "claude"
    }

    fn is_foreground_command(&self, command: &str) -> bool {
        command.starts_with("claude")
            || FOREGROUND_COMMANDS.contains(&command)
            || is_versioned_binary(command)
    }

    fn prompt_glyph(&self) -> char {
        '❯'
    }
}

fn is_versioned_binary(command: &str) -> bool {
    let parts: Vec<&str> = command.split('.').collect();

    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

pub fn session_uuid() -> String {
    let mut bytes = [0u8; 16];
    match std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
    {
        Ok(()) => {}
        Err(error) => {
            crate::log::line(
                "",
                format!("urandom unavailable ({error}), deriving a session id from the clock"),
            );

            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                ^ u128::from(std::process::id());
            bytes.copy_from_slice(&seed.to_le_bytes());
        }
    }

    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();

    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_claude_binaries_count_as_foreground() {
        for command in ["claude", "claude-native", "node", "bun", "2.1.299"] {
            assert!(Claude.is_foreground_command(command), "{command}");
        }

        for command in ["bash", "codex", "vim", "1.2", "1.2.x", "1..2"] {
            assert!(!Claude.is_foreground_command(command), "{command}");
        }
    }

    #[test]
    fn session_uuid_is_v4_shaped() {
        let id = session_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, session_uuid());
    }
}
