//! SSH 命令解析器 — 忠实移植 ssh-manager.mjs 的 parseSshCommand + 安全过滤。
//!
//! 安全性核心: 用户不能注入 ControlMaster / ControlPath / ControlPersist /
//! BatchMode / ProxyCommand 等 -o 选项，也不能使用 -M/-S/-O/-N/-t/-T/-f/-G/-W
//! 等 primary flags (这些被管理器内部使用)。

pub use super::types::ParsedSsh;

/// POSIX word splitting (处理单引号/双引号/反斜杠转义)。
///
/// 移植 ssh-manager.mjs:92-129 `splitShellWords`。
pub fn split_shell_words(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];

        // 反斜杠转义 (单引号内不转义)
        if ch == '\\' && !in_single {
            index += 1;
            if index < chars.len() {
                current.push(chars[index]);
            }
            index += 1;
            continue;
        }

        // 单引号
        if ch == '\'' && !in_double {
            in_single = !in_single;
            index += 1;
            continue;
        }

        // 双引号
        if ch == '"' && !in_single {
            in_double = !in_double;
            index += 1;
            continue;
        }

        // 空白分隔 (引号外)
        if ch.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            index += 1;
            continue;
        }

        current.push(ch);
        index += 1;
    }

    if in_single || in_double {
        return Err("Unclosed quote in SSH command".to_string());
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    Ok(tokens)
}

/// POSIX 单引号转义。
///
/// 移植 ssh-manager.mjs:24 `shellQuote`。
/// `it's` → `'it'\''s'`
pub fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

/// 被禁止的 primary flags (管理器内部使用)。
///
/// 移植 ssh-manager.mjs:131-133 `isDisallowedPrimaryFlag`。
fn is_disallowed_primary_flag(token: &str) -> bool {
    matches!(
        token,
        "-M" | "-S" | "-O" | "-N" | "-t" | "-T" | "-f" | "-G" | "-W" | "-v" | "-V" | "-q" | "-n"
            | "-s"
            | "-e"
            | "-E"
            | "-g"
    )
}

/// 被禁止的 -o 选项前缀。
///
/// 移植 ssh-manager.mjs:135-138 `hasDisallowedOOption`。
fn has_disallowed_o_option(value: &str) -> bool {
    let lower = value.trim().to_lowercase();
    ["controlmaster", "controlpath", "controlpersist", "batchmode", "proxycommand"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// 允许的无参数 flags。
///
/// 移植 ssh-manager.mjs:154 `allowedFlags`。
const ALLOWED_FLAGS: &[&str] = &[
    "-4", "-6", "-A", "-a", "-C", "-K", "-k", "-X", "-x", "-Y", "-y",
];

/// 允许的带参数 flags。
///
/// 移植 ssh-manager.mjs:155 `allowedWithValues`。
const ALLOWED_WITH_VALUES: &[&str] = &[
    "-B", "-b", "-c", "-D", "-F", "-I", "-i", "-J", "-l", "-m", "-o", "-P", "-p", "-R",
];

/// 解析 SSH 命令字符串。
///
/// 移植 ssh-manager.mjs:140-218 `parseSshCommand`。
///
/// 安全过滤:
/// - 去掉开头的 `ssh`
/// - 禁止 disallowed primary flags (-M/-S/-O/-N/-t/-T/-f/-G/-W/-v/-V/-q/-n/-s/-e/-E/-g)
/// - 禁止 disallowed -o 选项 (ControlMaster/ControlPath/ControlPersist/BatchMode/ProxyCommand)
/// - 允许的 flags 和带参数 flags 才保留
/// - destination 是第一个非 - 开头的 token
/// - destination 之后不允许有 trailing arguments
pub fn parse_ssh_command(raw: &str) -> Result<ParsedSsh, String> {
    let mut tokens = split_shell_words(raw)?;

    if tokens.is_empty() {
        return Err("SSH command is empty".to_string());
    }

    // 去掉开头的 ssh
    if tokens[0] == "ssh" {
        tokens.remove(0);
    }

    if tokens.is_empty() {
        return Err("SSH command must include destination".to_string());
    }

    let allowed_flags_set: std::collections::HashSet<&str> = ALLOWED_FLAGS.iter().copied().collect();

    let mut args = Vec::new();
    let mut destination: Option<String> = None;

    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];

        if destination.is_some() {
            return Err(format!(
                "SSH command has unsupported trailing argument: {}",
                token
            ));
        }

        // 非 flag → destination
        if !token.starts_with('-') {
            destination = Some(token.trim().to_string());
            index += 1;
            continue;
        }

        // 禁止的 primary flag
        if is_disallowed_primary_flag(token) {
            return Err(format!("SSH option {} is not allowed", token));
        }

        // 允许的无参数 flag
        if allowed_flags_set.contains(token.as_str()) {
            args.push(token.clone());
            index += 1;
            continue;
        }

        // 尝试匹配带参数 flag
        let mut matched = false;
        for option in ALLOWED_WITH_VALUES {
            // 精确匹配 (token == "-p", value 是下一个 token)
            if token == *option {
                let value = tokens.get(index + 1);
                let value = value.ok_or_else(|| {
                    format!("SSH option {} requires a value", option)
                })?;
                if *option == "-o" && has_disallowed_o_option(value) {
                    return Err(format!(
                        "SSH option -o {} is not allowed",
                        value
                    ));
                }
                args.push(token.clone());
                args.push(value.clone());
                index += 2;
                matched = true;
                break;
            }

            // 合并形式 (token == "-p2222" → option="-p", value="2222")
            if token.starts_with(*option) && token.len() > option.len() {
                let value = &token[option.len()..];
                if *option == "-o" && has_disallowed_o_option(value) {
                    return Err(format!(
                        "SSH option -o {} is not allowed",
                        value
                    ));
                }
                args.push(token.clone());
                index += 1;
                matched = true;
                break;
            }
        }

        if !matched {
            return Err(format!("Unsupported SSH option: {}", token));
        }
    }

    let destination = destination.ok_or("SSH command must include destination")?;

    Ok(ParsedSsh {
        destination,
        args,
    })
}

/// 构建 SSH 参数列表。
///
/// 移植 ssh-manager.mjs:244-248 `buildSshArgs`:
/// `[...parsed.args, ...preDestinationArgs, parsed.destination, remoteCommand?]`
pub fn build_ssh_args(
    parsed: &ParsedSsh,
    pre_destination_args: &[String],
    remote_command: Option<&str>,
) -> Vec<String> {
    let mut result = Vec::with_capacity(parsed.args.len() + pre_destination_args.len() + 2);
    result.extend(parsed.args.iter().cloned());
    result.extend(pre_destination_args.iter().cloned());
    result.push(parsed.destination.clone());
    if let Some(cmd) = remote_command {
        result.push(cmd.to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_words_basic() {
        assert_eq!(split_shell_words("ssh user@host").unwrap(), vec!["ssh", "user@host"]);
        assert_eq!(
            split_shell_words("ssh -p 2222 user@host").unwrap(),
            vec!["ssh", "-p", "2222", "user@host"]
        );
    }

    #[test]
    fn split_words_with_quotes() {
        assert_eq!(
            split_shell_words(r#"ssh -o "ServerAliveInterval=30" user@host"#).unwrap(),
            vec!["ssh", "-o", "ServerAliveInterval=30", "user@host"]
        );
        assert_eq!(
            split_shell_words("ssh -o 'ServerAliveInterval=30' user@host").unwrap(),
            vec!["ssh", "-o", "ServerAliveInterval=30", "user@host"]
        );
    }

    #[test]
    fn split_words_unclosed_quote() {
        assert!(split_shell_words("ssh 'user@host").is_err());
        assert!(split_shell_words(r#"ssh "user@host"#).is_err());
    }

    #[test]
    fn split_words_escape() {
        assert_eq!(
            split_shell_words(r"ssh user\@host").unwrap(),
            vec!["ssh", "user@host"]
        );
    }

    #[test]
    fn shell_quote_basic() {
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn parse_simple_destination() {
        let parsed = parse_ssh_command("ssh user@host").unwrap();
        assert_eq!(parsed.destination, "user@host");
        assert!(parsed.args.is_empty());
    }

    #[test]
    fn parse_without_ssh_prefix() {
        let parsed = parse_ssh_command("user@host").unwrap();
        assert_eq!(parsed.destination, "user@host");
    }

    #[test]
    fn parse_with_port() {
        let parsed = parse_ssh_command("ssh -p 2222 user@host").unwrap();
        assert_eq!(parsed.destination, "user@host");
        assert_eq!(parsed.args, vec!["-p", "2222"]);
    }

    #[test]
    fn parse_with_identity_file() {
        let parsed = parse_ssh_command("ssh -i ~/.ssh/id_ed25519 user@host").unwrap();
        assert_eq!(parsed.destination, "user@host");
        assert_eq!(parsed.args, vec!["-i", "~/.ssh/id_ed25519"]);
    }

    #[test]
    fn parse_with_agent_forwarding() {
        let parsed = parse_ssh_command("ssh -A user@host").unwrap();
        assert_eq!(parsed.destination, "user@host");
        assert_eq!(parsed.args, vec!["-A"]);
    }

    #[test]
    fn parse_with_o_option() {
        let parsed = parse_ssh_command("ssh -o ServerAliveInterval=30 user@host").unwrap();
        assert_eq!(parsed.destination, "user@host");
        assert_eq!(parsed.args, vec!["-o", "ServerAliveInterval=30"]);
    }

    #[test]
    fn parse_merged_port_form() {
        let parsed = parse_ssh_command("ssh -p2222 user@host").unwrap();
        assert_eq!(parsed.destination, "user@host");
        assert_eq!(parsed.args, vec!["-p2222"]);
    }

    #[test]
    fn parse_rejects_controlmaster() {
        assert!(parse_ssh_command("ssh -o ControlMaster=yes user@host").is_err());
        assert!(parse_ssh_command("ssh -o ControlPath=/tmp/sock user@host").is_err());
        assert!(parse_ssh_command("ssh -o ProxyCommand=evil user@host").is_err());
        assert!(parse_ssh_command("ssh -o BatchMode=yes user@host").is_err());
    }

    #[test]
    fn parse_rejects_disallowed_flags() {
        assert!(parse_ssh_command("ssh -N user@host").is_err());
        assert!(parse_ssh_command("ssh -O check user@host").is_err());
        assert!(parse_ssh_command("ssh -M user@host").is_err());
        assert!(parse_ssh_command("ssh -f user@host").is_err());
        assert!(parse_ssh_command("ssh -t user@host").is_err());
    }

    #[test]
    fn parse_rejects_trailing_args() {
        assert!(parse_ssh_command("ssh user@host extra").is_err());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_ssh_command("").is_err());
        assert!(parse_ssh_command("ssh").is_err());
    }

    #[test]
    fn parse_rejects_no_destination() {
        assert!(parse_ssh_command("ssh -A").is_err());
    }

    #[test]
    fn build_ssh_args_correct_order() {
        let parsed = parse_ssh_command("ssh -A -p 2222 user@host").unwrap();
        let pre = vec![
            "-o".to_string(),
            "ControlMaster=yes".to_string(),
        ];
        let args = build_ssh_args(&parsed, &pre, None);
        assert_eq!(
            args,
            vec!["-A", "-p", "2222", "-o", "ControlMaster=yes", "user@host"]
        );
    }

    #[test]
    fn build_ssh_args_with_remote_command() {
        let parsed = parse_ssh_command("ssh user@host").unwrap();
        let pre = vec!["-T".to_string()];
        let args = build_ssh_args(&parsed, &pre, Some("uname -a"));
        assert_eq!(args, vec!["-T", "user@host", "uname -a"]);
    }
}
