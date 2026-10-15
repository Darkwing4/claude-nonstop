use std::path::PathBuf;
use std::sync::LazyLock;

pub struct HarnessEnv {
    pub claude_bin: PathBuf,
    pub nonstop_bin: PathBuf,
    pub recorder_bin: PathBuf,
    pub artifacts_dir: PathBuf,
    pub sandboxes_dir: PathBuf,
}

static HARNESS_ENV: LazyLock<HarnessEnv> = LazyLock::new(|| HarnessEnv {
    claude_bin: required("E2E_CLAUDE_BIN"),
    nonstop_bin: required("E2E_NONSTOP_BIN"),
    recorder_bin: required("E2E_RECORDER_BIN"),
    artifacts_dir: required("E2E_ARTIFACTS_DIR"),
    sandboxes_dir: std::env::temp_dir().join("cn-e2e"),
});

pub fn harness_env() -> &'static HarnessEnv {
    &HARNESS_ENV
}

fn required(name: &str) -> PathBuf {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => panic!("{name} is not set: run the scenarios through e2e/run.sh"),
    }
}
