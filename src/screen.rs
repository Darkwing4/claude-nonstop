use std::sync::LazyLock;

use regex::Regex;

static BUSY: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)esc to interrupt",
        r"(?i)\bretrying in\b",
        r"(?i)\battempt \d+/\d+",
        r"(?i)will retry in",
        r"(?i)waiting for \d+ background agents? to finish",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("busy regex"))
    .collect()
});
static BOX_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[─━│┃╭╮╰╯┌┐└┘┄┈▏▕]").expect("box regex"));

pub fn strip_ansi(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != 0x1b {
            out.push(raw[i]);
            i += 1;
            continue;
        }
        let Some(next) = raw.get(i + 1) else {
            break;
        };
        match next {
            b'[' => {
                i += 2;
                while i < raw.len() && (0x20..=0x3f).contains(&raw[i]) {
                    i += 1;
                }
                if i < raw.len() {
                    i += 1;
                }
            }
            b']' | b'P' | b'_' | b'X' | b'^' => {
                i += 2;
                while i < raw.len() {
                    if raw[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if raw[i] == 0x1b && raw.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'(' | b')' | b'*' | b'+' | b'#' | b'%' => i += 3,
            _ => i += 2,
        }
    }
    String::from_utf8_lossy(&out).replace('\r', "")
}

pub fn is_busy(screen: &str) -> bool {
    BUSY.iter().any(|pattern| pattern.is_match(screen))
}

pub fn input_line(screen: &str, glyph: char) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let index = lines.iter().rposition(|line| is_prompt_line(line, glyph))?;
    let mut parts = vec![strip_prompt(lines[index], glyph)];
    for line in &lines[index + 1..] {
        if is_prompt_line(line, glyph) {
            break;
        }
        let rest = BOX_CHARS.replace_all(line, "");
        let rest = rest.trim();
        if rest.is_empty() || is_footer(rest) {
            break;
        }
        parts.push(rest.to_owned());
    }
    Some(collapse(&parts.join(" ")))
}

fn is_footer(line: &str) -> bool {
    line.starts_with('⏵') || line.starts_with('⏸') || line.starts_with('?') || line.starts_with('✻')
}

fn is_prompt_line(line: &str, glyph: char) -> bool {
    let cleaned = BOX_CHARS.replace_all(line, "");
    cleaned.trim_start().starts_with(glyph)
}

fn strip_prompt(line: &str, glyph: char) -> String {
    let cleaned = BOX_CHARS.replace_all(line, "");
    let trimmed = cleaned.trim_start();
    let after = trimmed.strip_prefix(glyph).unwrap_or(trimmed);
    after.trim().to_owned()
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_sequences_are_removed() {
        let raw = b"\x1b[31mred\x1b[0m \x1b]0;title\x07plain \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ end\r\n";
        assert_eq!(strip_ansi(raw), "red plain link end\n");
        assert_eq!(strip_ansi(b"\x1b(Bok\x1b"), "ok");
    }

    #[test]
    fn busy_footer_is_detected() {
        assert!(is_busy("✻ Thinking… (esc to interrupt)"));
        assert!(is_busy(
            "✻ Waiting for API response · will retry in 2m 29s · check your network"
        ));
        assert!(is_busy("API Error (529) · Retrying in 5s · attempt 3/10"));
        assert!(!is_busy("❯ \n⏵⏵ auto mode on"));
    }

    #[test]
    fn input_line_is_read_from_the_prompt() {
        let screen = "● done\n────\n❯ continue please\n────\n⏵⏵ auto mode on\n";
        assert_eq!(input_line(screen, '❯'), Some("continue please".to_owned()));
        let eaten = "────\n│ ❯ ontinue please │\n────\n";
        assert_eq!(input_line(eaten, '❯'), Some("ontinue please".to_owned()));
        let wrapped = "❯ Continue where you\n  left off.\n\n⏵⏵ auto mode on\n";
        assert_eq!(
            input_line(wrapped, '❯'),
            Some("Continue where you left off.".to_owned())
        );
        assert_eq!(input_line("❯ \n", '❯'), Some(String::new()));
        assert_eq!(
            input_line("● done\n❯ \n⏵⏵ auto mode on\n", '❯'),
            Some(String::new())
        );
    }
}
