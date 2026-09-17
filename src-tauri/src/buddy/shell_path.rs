//! PATH & proxy env fix-up when launched from Finder, port of
//! `src/main/buddy/shell-path.ts` (v1.3.1: login-shell resolution for
//! posix/fish/csh + proxy inheritance from v1.3.0).

use regex::Regex;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const ENV_MARKER: &str = "__BUDDY_ENV__:";
const SYSTEM_BIN_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];
const SHELL_ENV_TIMEOUT: Duration = Duration::from_millis(5000);

pub fn install_hint_for(command: &str) -> Option<&'static str> {
    match command {
        "kimi" => Some("curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash"),
        "claude" => Some("npm install -g @anthropic-ai/claude-code"),
        "codex" => Some("npm install -g @openai/codex"),
        "cursor-agent" | "agent" => Some("curl -fsS https://cursor.com/install | bash"),
        "agy" => Some("curl -fsSL https://antigravity.google/cli/install.sh | bash"),
        "opencode" => Some("go install github.com/sst/opencode@latest"),
        _ => None,
    }
}

pub const PROXY_VARS: [&str; 8] = [
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
];

pub const PROXY_PAIRS: [(&str, &str); 4] = [
    ("http_proxy", "HTTP_PROXY"),
    ("https_proxy", "HTTPS_PROXY"),
    ("all_proxy", "ALL_PROXY"),
    ("no_proxy", "NO_PROXY"),
];

fn is_proxy_var(name: &str) -> bool {
    PROXY_VARS.contains(&name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Posix,
    Fish,
    Csh,
}

fn posix_env_script() -> String {
    [
        "[ -n \"${PATH+x}\" ] && printf '__BUDDY_ENV__:PATH=%s\\n' \"$PATH\"",
        "[ -n \"${http_proxy+x}\" ] && printf '__BUDDY_ENV__:http_proxy=%s\\n' \"$http_proxy\"",
        "[ -n \"${https_proxy+x}\" ] && printf '__BUDDY_ENV__:https_proxy=%s\\n' \"$https_proxy\"",
        "[ -n \"${all_proxy+x}\" ] && printf '__BUDDY_ENV__:all_proxy=%s\\n' \"$all_proxy\"",
        "[ -n \"${no_proxy+x}\" ] && printf '__BUDDY_ENV__:no_proxy=%s\\n' \"$no_proxy\"",
        "[ -n \"${HTTP_PROXY+x}\" ] && printf '__BUDDY_ENV__:HTTP_PROXY=%s\\n' \"$HTTP_PROXY\"",
        "[ -n \"${HTTPS_PROXY+x}\" ] && printf '__BUDDY_ENV__:HTTPS_PROXY=%s\\n' \"$HTTPS_PROXY\"",
        "[ -n \"${ALL_PROXY+x}\" ] && printf '__BUDDY_ENV__:ALL_PROXY=%s\\n' \"$ALL_PROXY\"",
        "[ -n \"${NO_PROXY+x}\" ] && printf '__BUDDY_ENV__:NO_PROXY=%s\\n' \"$NO_PROXY\"",
        "true",
    ]
    .join("\n")
}

fn fish_env_script() -> String {
    std::iter::once("PATH")
        .chain(PROXY_VARS.iter().copied())
        .map(|name| {
            format!(
                "if set -q {name}; printf '{ENV_MARKER}{name}=%s\\n' (string join : ${name}); end"
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn csh_env_script() -> String {
    std::iter::once("PATH")
        .chain(PROXY_VARS.iter().copied())
        .map(|name| format!("if ($?{name}) printf '{ENV_MARKER}{name}=%s\\n' \"${name}\""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The account's login shell (Directory Services on macOS, /etc/passwd
/// elsewhere), mirroring `os.userInfo().shell`.
fn account_login_shell() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(user) = std::env::var("USER") {
            if !user.is_empty() {
                let output = Command::new("dscl")
                    .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output();
                if let Ok(out) = output {
                    if out.status.success() {
                        let text = String::from_utf8_lossy(&out.stdout);
                        for line in text.lines() {
                            if let Some(rest) = line.strip_prefix("UserShell:") {
                                let shell = rest.trim();
                                if !shell.is_empty() {
                                    return Some(shell.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // /etc/passwd fallback (primary source on non-macOS platforms).
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        let user = std::env::var("USER").unwrap_or_default();
        for line in passwd.lines() {
            let mut fields = line.split(':');
            let name = fields.next().unwrap_or("");
            if name == user && !name.is_empty() {
                let shell = line.rsplit(':').next().unwrap_or("").trim();
                if !shell.is_empty() {
                    return Some(shell.to_string());
                }
            }
        }
    }
    None
}

/// The shell whose rc files actually define PATH: $SHELL if set, otherwise the
/// account login shell, then the OS default. Never assume the user is on zsh.
///
/// `login_shell`: `None` = detect from the account database;
/// `Some(inner)` = explicit override (inner `None` mirrors `{ loginShell: null }`).
pub fn resolve_user_shell(
    env: &HashMap<String, String>,
    login_shell: Option<Option<&str>>,
) -> String {
    if let Some(from_env) = env.get("SHELL").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return from_env.to_string();
    }
    let from_account: Option<String> = match login_shell {
        Some(value) => value.map(|s| s.to_string()),
        None => account_login_shell(),
    };
    if let Some(shell) = from_account
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return shell.to_string();
    }
    if cfg!(target_os = "macos") {
        "/bin/zsh".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

pub fn shell_kind(shell_path: &str) -> ShellKind {
    let name = Path::new(shell_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name == "fish" || name.starts_with("fish-") || name.starts_with("fish.") {
        return ShellKind::Fish;
    }
    if name == "csh" || name == "tcsh" || name.starts_with("tcsh") {
        return ShellKind::Csh;
    }
    ShellKind::Posix
}

/// `-l` loads login rc (PATH often lives there). `-i` also loads interactive
/// rc (.zshrc, .bashrc, fish interactive snippets). csh can stall on `-i`
/// without a TTY.
pub fn login_shell_args(kind: ShellKind, script: &str) -> Vec<String> {
    match kind {
        ShellKind::Csh => vec!["-l".to_string(), "-c".to_string(), script.to_string()],
        _ => vec!["-il".to_string(), "-c".to_string(), script.to_string()],
    }
}

pub fn env_script_for(kind: ShellKind) -> String {
    match kind {
        ShellKind::Fish => fish_env_script(),
        ShellKind::Csh => csh_env_script(),
        ShellKind::Posix => posix_env_script(),
    }
}

fn decoration_regexes() -> &'static [Regex] {
    static REGEXES: OnceLock<Vec<Regex>> = OnceLock::new();
    REGEXES.get_or_init(|| {
        vec![
            // OSC: ESC ] ... (BEL | ESC \)
            Regex::new("\u{1b}\\][\\s\\S]*?(?:\u{7}|\u{1b}\\\\)").unwrap(),
            // CSI: ESC [ params intermediates final
            Regex::new("\u{1b}\\[[0-9;:<=>?]*[ -/]*[@-~]").unwrap(),
            // DCS/SOS/PM/APC: ESC [PX^_] ... ESC \
            Regex::new("\u{1b}[PX^_][\\s\\S]*?\u{1b}\\\\").unwrap(),
        ]
    })
}

/// Strip OSC/CSI sequences that login shells (iTerm2, Cursor, etc.) inject
/// around prompts. Those decorations often sit on the same line as our env
/// markers and would otherwise make a start-anchored parse miss PATH entirely.
pub fn strip_terminal_decorations(text: &str) -> String {
    let mut out = text.to_string();
    for re in decoration_regexes() {
        out = re.replace_all(&out, "").into_owned();
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellEnv {
    pub path: Option<String>,
    pub proxy_env: HashMap<String, String>,
}

pub fn parse_shell_env_output(output: &str) -> ShellEnv {
    let mut result = ShellEnv::default();

    for raw_line in output.split('\n') {
        let trimmed = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let line = strip_terminal_decorations(trimmed);
        let Some(marker_at) = line.find(ENV_MARKER) else {
            continue;
        };
        let rest = &line[marker_at + ENV_MARKER.len()..];
        let Some(eq) = rest.find('=') else {
            continue;
        };
        let key = &rest[..eq];
        let val = &rest[eq + 1..];
        if key == "PATH" {
            result.path = Some(val.to_string());
        } else if is_proxy_var(key) {
            result.proxy_env.insert(key.to_string(), val.to_string());
        }
    }

    result
}

/// Run `shell` as a login shell with the env-dump script and parse its output.
/// Returns an empty `ShellEnv` when the shell cannot be spawned or exceeds
/// `timeout`.
pub fn extract_shell_env(
    shell: Option<&str>,
    timeout: Option<Duration>,
    env: &HashMap<String, String>,
) -> ShellEnv {
    let shell_to_run = shell
        .map(|s| s.to_string())
        .unwrap_or_else(|| resolve_user_shell(env, None));
    let kind = shell_kind(&shell_to_run);
    let args = login_shell_args(kind, &env_script_for(kind));
    let timeout = timeout.unwrap_or(SHELL_ENV_TIMEOUT);

    run_capturing_stdout(&shell_to_run, &args, env, timeout)
        .map(|output| parse_shell_env_output(&output))
        .unwrap_or_default()
}

fn run_capturing_stdout(
    program: &str,
    args: &[String],
    env: &HashMap<String, String>,
    timeout: Duration,
) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .envs(env)
        .spawn()
        .ok()?;

    // Read stdout on a helper thread so a noisy shell rc cannot fill the pipe
    // buffer and deadlock while we poll for exit.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).ok().map(|_| buf)
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let _ = child.wait();
    let buf = reader.join().ok()??;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Merge PATH entries: `extra_paths` take priority over the entries already in
/// `base_path`; duplicates and empties are removed.
pub fn merge_path_entries(base_path: &str, extra_paths: &[String]) -> String {
    let current: Vec<&str> = base_path.split(':').filter(|s| !s.is_empty()).collect();
    let mut merged: Vec<String> = Vec::new();
    for entry in extra_paths.iter().filter(|s| !s.is_empty()) {
        if !merged.contains(entry) {
            merged.push(entry.clone());
        }
    }
    for entry in current {
        if !merged.iter().any(|e| e == entry) {
            merged.push(entry.to_string());
        }
    }
    merged.join(":")
}

/// For each proxy pair, take the first source (in priority order) that defines
/// either casing, mirroring the value to the missing casing. An explicitly set
/// empty string counts as defined (intentional clear).
pub fn resolve_proxy_pairs(sources: &[&HashMap<String, String>]) -> HashMap<String, String> {
    let mut result = HashMap::new();

    for (lower, upper) in PROXY_PAIRS {
        for src in sources {
            let has_lower = src.contains_key(lower);
            let has_upper = src.contains_key(upper);

            if has_lower && has_upper {
                result.insert(lower.to_string(), src[lower].clone());
                result.insert(upper.to_string(), src[upper].clone());
                break;
            } else if has_lower {
                result.insert(lower.to_string(), src[lower].clone());
                result.insert(upper.to_string(), src[lower].clone());
                break;
            } else if has_upper {
                result.insert(lower.to_string(), src[upper].clone());
                result.insert(upper.to_string(), src[upper].clone());
                break;
            }
        }
    }

    result
}

/// Overlay the login-shell proxy env onto `target_env` (target values win).
pub fn apply_shell_proxy_env(
    target_env: &mut HashMap<String, String>,
    shell_proxy: &HashMap<String, String>,
) {
    let resolved = resolve_proxy_pairs(&[target_env, shell_proxy]);
    for (key, val) in resolved {
        target_env.insert(key, val);
    }
}

/// Merge a child process env: non-proxy variables from `override_env` win over
/// `base_env`; proxy pairs resolve with `override_env` > `base_env` priority
/// and are mirrored across casings.
pub fn merge_child_env(
    base_env: &HashMap<String, String>,
    override_env: Option<&HashMap<String, String>>,
) -> HashMap<String, String> {
    let mut result = HashMap::new();

    for (key, val) in base_env {
        if !is_proxy_var(key) {
            result.insert(key.clone(), val.clone());
        }
    }

    if let Some(overrides) = override_env {
        for (key, val) in overrides {
            if !is_proxy_var(key) {
                result.insert(key.clone(), val.clone());
            }
        }
    }

    let proxy_values = match override_env {
        Some(overrides) => resolve_proxy_pairs(&[overrides, base_env]),
        None => resolve_proxy_pairs(&[base_env]),
    };
    for (key, val) in proxy_values {
        result.insert(key, val);
    }

    result
}

/// Generic PATH fallbacks for GUI apps whose login-shell extraction failed.
/// Includes ~/bin, Homebrew/usr/local, and any existing `$HOME/.<name>/bin`
/// (the usual layout for user-installed CLIs). No per-tool directory names.
pub fn discover_user_bin_dirs(home: &Path, extra_system_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![home.join("bin")];

    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.') || name == "." || name == ".." {
                continue;
            }
            candidates.push(home.join(name.as_ref()).join("bin"));
        }
    }

    candidates.extend(extra_system_dirs.iter().cloned());

    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in candidates {
        if dir.exists() && !seen.contains(&dir) {
            seen.push(dir);
        }
    }
    seen
}

/// On macOS, GUI apps inherit a minimal PATH and no proxy env. Prefer the
/// login-shell PATH (appending discovered bin dirs only when missing) and
/// inherit proxy variables from the login shell.
pub fn fix_shell_path() {
    if std::env::consts::OS != "macos" {
        return;
    }
    if std::env::var("NODE_ENV").map(|v| v == "test").unwrap_or(false) {
        return;
    }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let fallbacks = discover_user_bin_dirs(
        &home,
        &SYSTEM_BIN_DIRS.iter().map(PathBuf::from).collect::<Vec<_>>(),
    );

    let env: HashMap<String, String> = std::env::vars().collect();
    let shell = resolve_user_shell(&env, None);
    let extracted = extract_shell_env(Some(&shell), None, &env);

    // Prefer the login-shell PATH; append discovered dirs only if they were missing.
    let base_path = extracted
        .path
        .clone()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let base_entries: Vec<String> = base_path
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let fallback_base = fallbacks
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    std::env::set_var("PATH", merge_path_entries(&fallback_base, &base_entries));

    let mut process_env: HashMap<String, String> = std::env::vars().collect();
    apply_shell_proxy_env(&mut process_env, &extracted.proxy_env);
    for (lower, upper) in PROXY_PAIRS {
        if let Some(val) = process_env.get(lower) {
            std::env::set_var(lower, val);
        }
        if let Some(val) = process_env.get(upper) {
            std::env::set_var(upper, val);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defines_all_expected_proxy_vars_and_pairs() {
        for name in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
        ] {
            assert!(PROXY_VARS.contains(&name));
        }
        assert_eq!(
            PROXY_PAIRS,
            [
                ("http_proxy", "HTTP_PROXY"),
                ("https_proxy", "HTTPS_PROXY"),
                ("all_proxy", "ALL_PROXY"),
                ("no_proxy", "NO_PROXY"),
            ]
        );
    }

    #[test]
    fn resolves_user_shell_preferring_env_then_account_then_default() {
        assert_eq!(
            resolve_user_shell(&env_map(&[("SHELL", "/opt/homebrew/bin/fish")]), None),
            "/opt/homebrew/bin/fish"
        );
        assert_eq!(
            resolve_user_shell(&env_map(&[("SHELL", "  /bin/bash  ")]), None),
            "/bin/bash"
        );
        assert_eq!(
            resolve_user_shell(&HashMap::new(), Some(Some("/usr/local/bin/fish"))),
            "/usr/local/bin/fish"
        );
        let expected_default = if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else {
            "/bin/sh"
        };
        assert_eq!(
            resolve_user_shell(&HashMap::new(), Some(Some(""))),
            expected_default
        );
        assert_eq!(
            resolve_user_shell(&HashMap::new(), Some(None)),
            expected_default
        );
    }

    #[test]
    fn classifies_shell_kinds() {
        assert_eq!(shell_kind("/bin/zsh"), ShellKind::Posix);
        assert_eq!(shell_kind("/bin/bash"), ShellKind::Posix);
        assert_eq!(shell_kind("/opt/homebrew/bin/fish"), ShellKind::Fish);
        assert_eq!(shell_kind("/usr/local/bin/fish-3.7"), ShellKind::Fish);
        assert_eq!(shell_kind("/bin/tcsh"), ShellKind::Csh);
        assert_eq!(shell_kind("/bin/csh"), ShellKind::Csh);
    }

    #[test]
    fn builds_kind_specific_env_scripts_and_login_args() {
        assert!(env_script_for(ShellKind::Posix).contains("${PATH+x}"));
        assert!(env_script_for(ShellKind::Fish).contains("string join : $PATH"));
        assert!(env_script_for(ShellKind::Csh).contains("$?PATH"));
        assert_eq!(
            login_shell_args(ShellKind::Posix, "true"),
            vec!["-il", "-c", "true"]
        );
        assert_eq!(
            login_shell_args(ShellKind::Fish, "true"),
            vec!["-il", "-c", "true"]
        );
        assert_eq!(
            login_shell_args(ShellKind::Csh, "true"),
            vec!["-l", "-c", "true"]
        );
    }

    #[test]
    fn merge_path_entries_dedupes_and_preserves_spaces() {
        let space_path = "/Users/test user/Special Tools/bin";
        let base_path = format!("/usr/bin:/bin:{space_path}:/opt/homebrew/bin");
        let extras = vec![
            "/opt/homebrew/bin".to_string(),
            "/usr/local/bin".to_string(),
            space_path.to_string(),
        ];

        let merged = merge_path_entries(&base_path, &extras);
        let parts: Vec<&str> = merged.split(':').collect();

        assert!(parts.contains(&space_path));
        assert!(parts.contains(&"/opt/homebrew/bin"));
        assert!(parts.contains(&"/usr/local/bin"));
        assert!(parts.contains(&"/usr/bin"));
        assert_eq!(parts.iter().filter(|p| **p == space_path).count(), 1);
        assert_eq!(
            parts.iter().filter(|p| **p == "/opt/homebrew/bin").count(),
            1
        );
    }

    #[test]
    fn merge_path_entries_prefers_extras_then_base() {
        // fix_shell_path call shape: fallbacks as base, login-shell PATH as extras.
        let shell_path = "/Users/me/.wecode-cli/bin:/usr/bin:/bin";
        let fallbacks = vec![
            "/opt/homebrew/bin".to_string(),
            "/Users/me/.wecode-cli/bin".to_string(),
            "/Users/me/.local/bin".to_string(),
        ];
        let extras: Vec<String> = shell_path
            .split(':')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        let merged = merge_path_entries(&fallbacks.join(":"), &extras);
        assert_eq!(
            merged.split(':').collect::<Vec<_>>(),
            vec![
                "/Users/me/.wecode-cli/bin",
                "/usr/bin",
                "/bin",
                "/opt/homebrew/bin",
                "/Users/me/.local/bin"
            ]
        );
    }

    #[test]
    fn merge_path_entries_preserves_original_path_on_extraction_failure() {
        let merged = merge_path_entries(
            "/original/unique/path:/usr/bin",
            &["/opt/homebrew/bin".to_string()],
        );
        assert!(merged.contains("/original/unique/path"));
        assert!(merged.contains("/opt/homebrew/bin"));
    }

    #[test]
    fn parse_shell_env_output_filters_noise_and_preserves_values() {
        let raw_output = [
            "Last login: Tue Sep 15 10:00:00 2026 on ttys001",
            "Welcome to custom zsh banner!",
            "__BUDDY_ENV__:PATH=/usr/local/bin:/usr/bin with spaces",
            "__BUDDY_ENV__:http_proxy=http://user:p%40ss_word@127.0.0.1:7893/?query=1&flag=true",
            "__BUDDY_ENV__:https_proxy=",
            "__BUDDY_ENV__:NO_PROXY=localhost,127.0.0.1",
            "__BUDDY_ENV__:MALICIOUS_VAR=should_be_ignored",
            "__BUDDY_ENV__:OTHER_ENV=ignored",
            "some other trailing noise",
        ]
        .join("\n");

        let parsed = parse_shell_env_output(&raw_output);
        assert_eq!(
            parsed.path.as_deref(),
            Some("/usr/local/bin:/usr/bin with spaces")
        );
        assert_eq!(
            parsed.proxy_env.get("http_proxy").map(String::as_str),
            Some("http://user:p%40ss_word@127.0.0.1:7893/?query=1&flag=true")
        );
        assert_eq!(
            parsed.proxy_env.get("https_proxy").map(String::as_str),
            Some("")
        );
        assert_eq!(
            parsed.proxy_env.get("NO_PROXY").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
        assert!(!parsed.proxy_env.contains_key("all_proxy"));
        assert!(!parsed.proxy_env.contains_key("MALICIOUS_VAR"));
        assert!(!parsed.proxy_env.contains_key("OTHER_ENV"));
    }

    #[test]
    fn parse_shell_env_output_recovers_path_through_osc_csi_decorations() {
        let osc = "\u{1b}]1337;RemoteHost=david@host\u{7}\u{1b}]1337;CurrentDir=/tmp\u{7}";
        let csi = "\u{1b}[32m";
        let raw_output = format!(
            "{osc}{csi}__BUDDY_ENV__:PATH=/Users/me/.wecode-cli/bin:/usr/bin\n{osc}__BUDDY_ENV__:http_proxy=http://127.0.0.1:7893"
        );

        let parsed = parse_shell_env_output(&raw_output);
        assert_eq!(
            parsed.path.as_deref(),
            Some("/Users/me/.wecode-cli/bin:/usr/bin")
        );
        assert_eq!(
            parsed.proxy_env.get("http_proxy").map(String::as_str),
            Some("http://127.0.0.1:7893")
        );
    }

    #[test]
    fn strip_terminal_decorations_removes_osc_csi_dcs() {
        let decorated = "\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7}\u{1b}[1;31mred\u{1b}[0m\u{1b}Pqmdata\u{1b}\\end";
        assert_eq!(strip_terminal_decorations(decorated), "linkredend");
    }

    #[test]
    fn discover_user_bin_dirs_scans_existing_dot_bins() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let wecode_bin = home.join(".wecode-cli/bin");
        let kimi_bin = home.join(".kimi-code/bin");
        let user_bin = home.join("bin");
        let no_bin_dir = home.join(".ssh");
        std::fs::create_dir_all(&wecode_bin).unwrap();
        std::fs::create_dir_all(&kimi_bin).unwrap();
        std::fs::create_dir_all(&user_bin).unwrap();
        std::fs::create_dir_all(&no_bin_dir).unwrap();
        std::fs::write(home.join(".zshrc"), "").unwrap();

        let system_bin = temp.path().join("opt-bin");
        std::fs::create_dir_all(&system_bin).unwrap();
        let missing_bin = temp.path().join("missing-bin");

        let discovered = discover_user_bin_dirs(&home, &[system_bin.clone(), missing_bin]);
        for expected in [&wecode_bin, &kimi_bin, &user_bin, &system_bin] {
            assert!(
                discovered.contains(expected),
                "missing {:?} in {:?}",
                expected,
                discovered
            );
        }
        assert!(!discovered.contains(&no_bin_dir));
        assert!(!discovered.iter().any(|d| d.to_string_lossy().contains("missing-bin")));
    }

    #[test]
    fn resolve_proxy_pairs_mirrors_lowercase_to_uppercase() {
        let src = env_map(&[("http_proxy", "http://127.0.0.1:7893")]);
        let resolved = resolve_proxy_pairs(&[&src]);
        assert_eq!(
            resolved.get("http_proxy").map(String::as_str),
            Some("http://127.0.0.1:7893")
        );
        assert_eq!(
            resolved.get("HTTP_PROXY").map(String::as_str),
            Some("http://127.0.0.1:7893")
        );
    }

    #[test]
    fn resolve_proxy_pairs_mirrors_uppercase_to_lowercase() {
        let src = env_map(&[("HTTPS_PROXY", "http://127.0.0.1:8443")]);
        let resolved = resolve_proxy_pairs(&[&src]);
        assert_eq!(
            resolved.get("https_proxy").map(String::as_str),
            Some("http://127.0.0.1:8443")
        );
        assert_eq!(
            resolved.get("HTTPS_PROXY").map(String::as_str),
            Some("http://127.0.0.1:8443")
        );
    }

    #[test]
    fn resolve_proxy_pairs_prioritizes_first_source() {
        let buddy_env = env_map(&[("http_proxy", "http://buddy-env:1111")]);
        let shell_env = env_map(&[
            ("http_proxy", "http://shell-env:2222"),
            ("HTTP_PROXY", "http://shell-env:2222"),
            ("https_proxy", "http://shell-env:3333"),
        ]);

        let resolved = resolve_proxy_pairs(&[&buddy_env, &shell_env]);
        assert_eq!(
            resolved.get("http_proxy").map(String::as_str),
            Some("http://buddy-env:1111")
        );
        assert_eq!(
            resolved.get("HTTP_PROXY").map(String::as_str),
            Some("http://buddy-env:1111")
        );
        assert_eq!(
            resolved.get("https_proxy").map(String::as_str),
            Some("http://shell-env:3333")
        );
        assert_eq!(
            resolved.get("HTTPS_PROXY").map(String::as_str),
            Some("http://shell-env:3333")
        );
    }

    #[test]
    fn resolve_proxy_pairs_treats_empty_string_as_intentional_clear() {
        let high = env_map(&[("http_proxy", "")]);
        let low = env_map(&[
            ("http_proxy", "http://low-proxy:7893"),
            ("HTTP_PROXY", "http://low-proxy:7893"),
        ]);

        let resolved = resolve_proxy_pairs(&[&high, &low]);
        assert_eq!(resolved.get("http_proxy").map(String::as_str), Some(""));
        assert_eq!(resolved.get("HTTP_PROXY").map(String::as_str), Some(""));
    }

    #[test]
    fn resolve_proxy_pairs_preserves_conflicting_dual_values() {
        let single = env_map(&[
            ("http_proxy", "http://lower-val:1111"),
            ("HTTP_PROXY", "http://upper-val:2222"),
        ]);

        let resolved = resolve_proxy_pairs(&[&single]);
        assert_eq!(
            resolved.get("http_proxy").map(String::as_str),
            Some("http://lower-val:1111")
        );
        assert_eq!(
            resolved.get("HTTP_PROXY").map(String::as_str),
            Some("http://upper-val:2222")
        );
    }

    #[test]
    fn apply_shell_proxy_env_applies_resolved_pairs() {
        let mut target = env_map(&[("existing_var", "val")]);
        apply_shell_proxy_env(&mut target, &env_map(&[("http_proxy", "http://127.0.0.1:7893")]));
        assert_eq!(
            target.get("existing_var").map(String::as_str),
            Some("val")
        );
        assert_eq!(
            target.get("http_proxy").map(String::as_str),
            Some("http://127.0.0.1:7893")
        );
        assert_eq!(
            target.get("HTTP_PROXY").map(String::as_str),
            Some("http://127.0.0.1:7893")
        );
    }

    #[test]
    fn merge_child_env_override_wins_and_mirrors_pairs() {
        let base_env = env_map(&[
            ("PATH", "/usr/bin"),
            ("http_proxy", "http://global-proxy:7893"),
            ("HTTP_PROXY", "http://global-proxy:7893"),
            ("https_proxy", "http://global-proxy:7893"),
            ("HTTPS_PROXY", "http://global-proxy:7893"),
            ("OTHER_VAR", "keep-me"),
        ]);
        let launcher_env = env_map(&[("http_proxy", "http://launcher-custom:8080")]);

        let child_env = merge_child_env(&base_env, Some(&launcher_env));
        assert_eq!(
            child_env.get("http_proxy").map(String::as_str),
            Some("http://launcher-custom:8080")
        );
        assert_eq!(
            child_env.get("HTTP_PROXY").map(String::as_str),
            Some("http://launcher-custom:8080")
        );
        assert_eq!(
            child_env.get("https_proxy").map(String::as_str),
            Some("http://global-proxy:7893")
        );
        assert_eq!(
            child_env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://global-proxy:7893")
        );
        assert_eq!(
            child_env.get("OTHER_VAR").map(String::as_str),
            Some("keep-me")
        );
    }

    #[test]
    fn merge_child_env_supports_explicit_proxy_clear() {
        let base_env = env_map(&[
            ("http_proxy", "http://global:7893"),
            ("HTTP_PROXY", "http://global:7893"),
        ]);
        let launcher_env = env_map(&[("http_proxy", "")]);

        let child_env = merge_child_env(&base_env, Some(&launcher_env));
        assert_eq!(child_env.get("http_proxy").map(String::as_str), Some(""));
        assert_eq!(child_env.get("HTTP_PROXY").map(String::as_str), Some(""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extract_shell_env_reads_path_and_proxy_from_login_rc() {
        let temp = tempfile::tempdir().unwrap();
        let zdotdir = temp.path().join("zsh-home");
        std::fs::create_dir_all(&zdotdir).unwrap();
        let custom_path = "/custom/isolated/test/bin";
        let custom_http_proxy = "http://127.0.0.1:9876";
        let custom_no_proxy = "localhost,127.0.0.1,.local";
        let zshrc = [
            format!("export PATH=\"{custom_path}:$PATH\""),
            format!("export http_proxy=\"{custom_http_proxy}\""),
            format!("export NO_PROXY=\"{custom_no_proxy}\""),
            "unset https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY no_proxy".to_string(),
        ]
        .join("\n");
        std::fs::write(zdotdir.join(".zshrc"), zshrc).unwrap();

        let isolated_env = env_map(&[
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
            ("HOME", zdotdir.to_str().unwrap()),
            ("ZDOTDIR", zdotdir.to_str().unwrap()),
            ("USER", "testuser"),
        ]);

        let extracted = extract_shell_env(
            Some("/bin/zsh"),
            Some(Duration::from_millis(5000)),
            &isolated_env,
        );
        let path = extracted.path.expect("login shell PATH");
        assert!(path.contains(custom_path), "PATH was {path}");
        assert_eq!(
            extracted.proxy_env.get("http_proxy").map(String::as_str),
            Some(custom_http_proxy)
        );
        assert_eq!(
            extracted.proxy_env.get("NO_PROXY").map(String::as_str),
            Some(custom_no_proxy)
        );
        assert!(!extracted.proxy_env.contains_key("https_proxy"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extract_shell_env_times_out_on_stalling_shell() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let stall = temp.path().join("stall-shell");
        std::fs::write(&stall, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&stall, std::fs::Permissions::from_mode(0o755)).unwrap();

        let extracted = extract_shell_env(
            Some(stall.to_str().unwrap()),
            Some(Duration::from_millis(300)),
            &env_map(&[("PATH", "/usr/bin:/bin")]),
        );
        assert_eq!(extracted, ShellEnv::default());
    }
}
