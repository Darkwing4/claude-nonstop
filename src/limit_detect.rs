use std::sync::LazyLock;

use regex::{Regex, RegexSet};

use crate::ledger::AgentId;
use crate::screen::is_busy;

const WINDOW_LINES: usize = 12;
const RESET_RANGE: usize = 6;

const CHROME_PATTERNS: [&str; 20] = [
    r"^\s*$",
    r"^[\s─│╭╮╰╯┌┐└┘├┤┬┴┼▏▕|]+$",
    r"^\s*│\s*[>❯][^│]*│\s*$",
    r"^\s*[❯>]\s*$",
    r"^\s*⏵⏵",
    r"(?i)allowed by auto mode",
    r"(?i)shift\+tab to (?:cycle|select)",
    r"(?i)^\s*\?\s+for shortcuts\b",
    r"\|\s*v\d+\.\d+\.\d+\b",
    r"^\s+[□◻■◼▢▪◽◾✓✔☐☑]\s+\S",
    r"(?i)^\s*\d+\s+tasks?\s+\(",
    r"^\s*…\s*\+\d+\b",
    r"(?i)/clear to save",
    r"(?i)^\s*(?:[⎿└·]\s*)?/usage-credits\b",
    r"^\s*[✻✢✽✳✴✶✷]\s",
    r"[⟳↻⌛⏳🕐-🕧]\s*resets",
    r"[●○◐◓◑◒]{5,}\s*(?:ctx:)?\d+%",
    r"^\s*\$[\d.,]+\s+⏱",
    r"(?i)Backgrounded agent \(",
    r"to manage · ",
];

const CLAUDE_LIMIT_PATTERNS: [&str; 7] = [
    r"(?i)(?:hit|exceeded|reached).*(?:your|the)\s*(?:[\w'’-]+\s+){0,3}limit",
    r"(?i)\d+-hour limit",
    r"(?i)limit reached",
    r"(?i)usage limit",
    r"(?i)out of.*usage",
    r"(?i)rate limit",
    r"(?i)try again in",
];

const CLAUDE_RESET_PATTERNS: [&str; 5] = [
    r"(?i)resets?\s+(?:at\s+)?\d{1,2}(?::\d{2})?\s*(?:am|pm)?",
    r"(?i)resets?\s+(?:on\s+)?(?:mon|tue|wed|thu|fri|sat|sun)",
    r"(?i)resets?\s+(?:on\s+)?(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\s+\d{1,2}\b",
    r"(?i)resets?\s+in[:\s]\s*\d",
    r"(?i)try again in \d+\s*(?:hours?|minutes?|h|m)",
];

static CHROME: LazyLock<RegexSet> =
    LazyLock::new(|| RegexSet::new(CHROME_PATTERNS).expect("chrome regex set"));
static ECHO_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[●⏺∙]\s*\S+\(").expect("echo start regex"));
static ECHO_CONTINUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[⎿└]|^\s+\S").expect("echo continue regex"));
static CLAUDE_LIMIT: LazyLock<RegexSet> =
    LazyLock::new(|| RegexSet::new(CLAUDE_LIMIT_PATTERNS).expect("claude limit regex set"));
static CLAUDE_RESET: LazyLock<RegexSet> =
    LazyLock::new(|| RegexSet::new(CLAUDE_RESET_PATTERNS).expect("claude reset regex set"));
static CLAUDE_REJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bnot\s+(?:your|a|an|the)\s+(?:[\w-]+\s+){0,2}limit\b").expect("reject regex")
});
static MODEL_BANNER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:reached|hit)\s+(?:your|the)\s+[\w.\s-]{0,24}?\blimit\b")
        .expect("model banner regex")
});
static MODEL_REMEDY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)/usage-credits|switch models|/model").expect("remedy regex"));

static MENU_QUESTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)what do you want to do\?").expect("menu question regex"));
static MENU_WAIT_OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)stop and wait for limit to reset").expect("menu wait option regex")
});
static MENU_CONFIRM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)enter to confirm|esc to cancel").expect("menu confirm regex")
});
static MENU_OPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*❯?\s*\d+\.\s").expect("menu option regex"));
static MENU_STOP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)stop and wait").expect("menu stop regex"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuRead {
    NoMenu,
    Unreadable,
    Steps(i32),
}

pub fn content_tail(screen: &str, lines: usize) -> Vec<String> {
    let all: Vec<&str> = screen.lines().collect();
    let mask = echo_mask(&all);
    let content: Vec<String> = all
        .iter()
        .zip(mask)
        .filter(|(line, masked)| !masked && !is_chrome(line))
        .map(|(line, _)| line.trim().to_owned())
        .collect();
    let skip = content.len().saturating_sub(lines);

    content[skip..].to_vec()
}

pub fn find_limit(screen: &str, agent: AgentId) -> bool {
    let window = content_tail(screen, WINDOW_LINES);
    match agent {
        AgentId::Claude => find_claude(&window),
    }
}

pub fn rate_limit_menu(screen: &str) -> MenuRead {
    if !MENU_QUESTION.is_match(screen) || !MENU_WAIT_OPTION.is_match(screen) {
        return MenuRead::NoMenu;
    }

    if !MENU_CONFIRM.is_match(screen) && !screen.contains('❯') {
        return MenuRead::NoMenu;
    }

    let options: Vec<&str> = screen
        .lines()
        .filter(|line| MENU_OPTION.is_match(line))
        .collect();
    let cursor = options.iter().position(|line| line.contains('❯'));
    let wait = options.iter().position(|line| MENU_STOP.is_match(line));

    match (cursor, wait) {
        (Some(cursor), Some(wait)) => MenuRead::Steps(wait as i32 - cursor as i32),
        _ => MenuRead::Unreadable,
    }
}

fn is_chrome(line: &str) -> bool {
    if is_busy(line) {
        return false;
    }

    CHROME.is_match(line)
}

fn echo_mask(lines: &[&str]) -> Vec<bool> {
    let mut in_echo = false;
    lines
        .iter()
        .map(|line| {
            if ECHO_START.is_match(line) {
                in_echo = true;
            } else if in_echo && !ECHO_CONTINUE.is_match(line) {
                in_echo = false;
            }

            in_echo
        })
        .collect()
}

fn find_claude(window: &[String]) -> bool {
    let remedy_hinted = window.iter().any(|line| MODEL_REMEDY.is_match(line));
    window.iter().enumerate().any(|(index, line)| {
        if !is_claude_limit_line(line) {
            return false;
        }

        let model_banner = remedy_hinted && MODEL_BANNER.is_match(line);

        model_banner || has_reset_near(window, index)
    })
}

fn is_claude_limit_line(line: &str) -> bool {
    if starts_with_prompt(line) || CLAUDE_REJECT.is_match(line) {
        return false;
    }

    CLAUDE_LIMIT.is_match(line)
}

fn starts_with_prompt(line: &str) -> bool {
    let text = line.trim_start();
    text.starts_with('❯') || text.starts_with('>')
}

fn has_reset_near(window: &[String], index: usize) -> bool {
    let first = index.saturating_sub(RESET_RANGE);
    let last = (index + RESET_RANGE).min(window.len().saturating_sub(1));

    window[first..=last]
        .iter()
        .any(|line| CLAUDE_RESET.is_match(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::strip_ansi;

    const FOOTER: &str = "\n❯ \n─────────────────────────────────────────\n⏵⏵ auto mode on (shift+tab to cycle)\n? for shortcuts\n";

    fn claude(screen: &str) -> bool {
        find_limit(screen, AgentId::Claude)
    }

    #[test]
    fn five_hour_banner_with_reset_on_next_line() {
        let screen = format!(
            "● Done with the refactor.\n\n⚠ You've hit your session limit\n· resets 6:40pm (Asia/Calcutta){FOOTER}"
        );
        assert!(claude(&screen));
    }

    #[test]
    fn weekly_banner_on_single_line() {
        let screen =
            format!("You've hit your weekly limit · resets Aug 31, 12pm (Asia/Bangkok){FOOTER}");
        assert!(claude(&screen));
    }

    #[test]
    fn bare_limit_with_reset() {
        let screen = format!("You've hit your limit · resets 2am (Europe/Moscow){FOOTER}");
        assert!(claude(&screen));
    }

    #[test]
    fn limit_with_unparsable_reset_is_still_detected() {
        let screen = format!("You've hit your limit · resets 3pm (Mars/Olympus){FOOTER}");
        assert!(claude(&screen));
    }

    #[test]
    fn upgrade_cta_variant() {
        let screen = format!(
            "⚠ You've hit your usage limit\n· resets 5:20pm · /upgrade to increase your limits{FOOTER}"
        );
        assert!(claude(&screen));
    }

    #[test]
    fn model_limit_needs_no_reset_line() {
        let screen = format!(
            "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.{FOOTER}"
        );
        assert!(claude(&screen));

        let screen = format!("You've reached your Fable limit.{FOOTER}");
        assert!(!claude(&screen));
    }

    #[test]
    fn narrow_terminal_wrap() {
        let screen = "⚠ You've hit your usage limit ·\n  resets 6:40pm\n  (Asia/Calcutta)\n\n❯ \n⏵⏵ auto mode on\n";
        assert!(claude(screen));
    }

    #[test]
    fn ansi_noise_is_stripped_before_detection() {
        let raw = "\x1b[33m⚠ You've hit your session limit\x1b[0m\n\x1b[2m· resets \x1b]8;;https://claude.ai/upgrade\x1b\\6:40pm\x1b]8;;\x1b\\ (Asia/Calcutta)\x1b[0m\n\x1b[1m❯ \x1b[0m\n";
        let screen = strip_ansi(raw.as_bytes());

        assert!(claude(&screen));
    }

    #[test]
    fn tool_echo_is_masked() {
        let screen = format!(
            "● Bash(grep 'resets 9am' notes.md)\n  ⎿  You've hit your usage limit · resets 9am\n\n● Nothing else to do.{FOOTER}"
        );
        assert!(!claude(&screen));

        let tail = content_tail(&screen, 12);
        assert_eq!(tail, vec!["● Nothing else to do.".to_owned()]);
    }

    #[test]
    fn readme_prose_without_reset_is_ignored() {
        let screen = format!(
            "● The README explains it: when you hit your usage limit, Claude Code\n  shows a banner and pauses until the window rolls over. A rate limit\n  from the API is a different thing and is retried automatically.{FOOTER}"
        );
        assert!(!claude(&screen));
    }

    #[test]
    fn throttle_notice_is_not_a_limit() {
        let screen = format!(
            "· resets 3pm\n\nAPI Error: Server is temporarily limiting requests (not your usage limit)\n\n● Retrying the request now.{FOOTER}"
        );
        assert!(!claude(&screen));
    }

    #[test]
    fn chrome_footer_is_excluded_but_banner_detected() {
        let screen = "⚠ You've hit your session limit\n· resets 6:40pm (Asia/Calcutta)\n\n❯ \n⏵⏵ auto mode on (shift+tab to cycle)\n? for shortcuts\n";
        assert!(claude(screen));

        let tail = content_tail(screen, 12);
        assert_eq!(
            tail,
            vec![
                "⚠ You've hit your session limit".to_owned(),
                "· resets 6:40pm (Asia/Calcutta)".to_owned(),
            ]
        );
    }

    #[test]
    fn busy_line_is_not_chrome() {
        let busy = "✻ Waiting for API response · will retry in 2m 29s · check your network";
        let screen = format!("● Working on it.\n\n{busy}\n\n⏵⏵ auto mode on\n");
        let tail = content_tail(&screen, 12);
        assert_eq!(tail, vec!["● Working on it.".to_owned(), busy.to_owned()]);

        let spinner = "✻ Thinking… (esc to interrupt)";
        assert!(!is_chrome(spinner));
        assert!(is_chrome("✻ Crunched for 12s"));
    }

    fn menu(first: &str, second: &str, footer: &str) -> String {
        format!(
            "You've hit your usage limit · resets 6:40pm (Asia/Calcutta)\n\nWhat do you want to do?\n{first}\n{second}\n  3. Cancel\n\n{footer}\n"
        )
    }

    #[test]
    fn menu_wait_first_needs_no_steps() {
        let screen = menu(
            "❯ 1. Stop and wait for limit to reset",
            "  2. Upgrade your plan",
            "Enter to confirm · Esc to cancel",
        );
        assert_eq!(rate_limit_menu(&screen), MenuRead::Steps(0));
    }

    #[test]
    fn menu_upgrade_first_needs_one_down() {
        let screen = menu(
            "❯ 1. Upgrade your plan",
            "  2. Stop and wait for limit to reset",
            "Enter to confirm · Esc to cancel",
        );
        assert_eq!(rate_limit_menu(&screen), MenuRead::Steps(1));
    }

    #[test]
    fn menu_cursor_below_wait_needs_one_up() {
        let screen = menu(
            "  1. Stop and wait for limit to reset",
            "❯ 2. Upgrade your plan",
            "Enter to confirm · Esc to cancel",
        );
        assert_eq!(rate_limit_menu(&screen), MenuRead::Steps(-1));
    }

    #[test]
    fn menu_without_cursor_is_unreadable() {
        let screen = menu(
            "  1. Stop and wait for limit to reset",
            "  2. Upgrade your plan",
            "Enter to confirm · Esc to cancel",
        );
        assert_eq!(rate_limit_menu(&screen), MenuRead::Unreadable);
    }

    #[test]
    fn unrelated_screen_has_no_menu() {
        let screen = "● Done.\n\n❯ what do you want to do?\n⏵⏵ auto mode on\n";

        assert_eq!(rate_limit_menu(screen), MenuRead::NoMenu);
        assert_eq!(rate_limit_menu(""), MenuRead::NoMenu);
    }
}
