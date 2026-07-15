//! Provider 安装命令元数据 (静态表)。
//!
//! 移植自 `packages/web/server/lib/tunnels/install-help.js` (56 行)。

use crate::tunnels::types::TUNNEL_PROVIDER_NGROK;

/// 安装信息。
#[derive(Debug, Clone)]
pub struct InstallInfo {
    pub dependency: &'static str,
    pub install_command: &'static str,
    pub install_url: &'static str,
    pub platform: &'static str,
    pub message: String,
}

struct ProviderInstallInfo {
    dependency: &'static str,
    install_url: &'static str,
    commands_darwin: &'static str,
    commands_win32: &'static str,
    commands_linux: &'static str,
}

const CLOUDFLARE_INFO: ProviderInstallInfo = ProviderInstallInfo {
    dependency: "cloudflared",
    install_url: "https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflared/downloads/",
    commands_darwin: "brew install cloudflared",
    commands_win32: "winget install --id Cloudflare.cloudflared",
    commands_linux: "Download cloudflared from https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflared/downloads/",
};

const NGROK_INFO: ProviderInstallInfo = ProviderInstallInfo {
    dependency: "ngrok",
    install_url: "https://ngrok.com/download",
    commands_darwin: "brew install ngrok",
    commands_win32: "winget install ngrok -s msstore",
    commands_linux: "Download ngrok from https://ngrok.com/download",
};

fn get_provider_info(provider: &str) -> &'static ProviderInstallInfo {
    match provider {
        TUNNEL_PROVIDER_NGROK => &NGROK_INFO,
        _ => &CLOUDFLARE_INFO,
    }
}

fn normalize_install_platform(platform: &str) -> &'static str {
    match platform {
        "darwin" => "darwin",
        "win32" => "win32",
        "linux" => "linux",
        _ => "linux",
    }
}

fn create_missing_dependency_message(dependency: &str, install_command: &str) -> String {
    if install_command.starts_with("Download ") {
        format!("{} is not installed. {}", dependency, install_command)
    } else {
        format!(
            "{} is not installed. Install it with: {}",
            dependency, install_command
        )
    }
}

fn current_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "darwin"
    }
    #[cfg(target_os = "windows")]
    {
        "win32"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        "linux"
    }
}

/// 获取 provider 安装信息。
pub fn get_tunnel_dependency_install_info(provider: &str) -> InstallInfo {
    get_tunnel_dependency_install_info_for_platform(provider, current_platform())
}

/// 获取 provider 安装信息 (指定平台)。
pub fn get_tunnel_dependency_install_info_for_platform(
    provider: &str,
    platform: &str,
) -> InstallInfo {
    let info = get_provider_info(provider);
    let normalized_platform = normalize_install_platform(platform);
    let install_command = match normalized_platform {
        "darwin" => info.commands_darwin,
        "win32" => info.commands_win32,
        _ => info.commands_linux,
    };

    InstallInfo {
        dependency: info.dependency,
        install_command,
        install_url: info.install_url,
        platform: normalized_platform,
        message: create_missing_dependency_message(info.dependency, install_command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_install_info() {
        let info = get_tunnel_dependency_install_info_for_platform("cloudflare", "darwin");
        assert_eq!(info.dependency, "cloudflared");
        assert_eq!(info.install_command, "brew install cloudflared");
        assert!(info.message.contains("cloudflared is not installed"));
    }

    #[test]
    fn ngrok_install_info() {
        let info = get_tunnel_dependency_install_info_for_platform("ngrok", "linux");
        assert_eq!(info.dependency, "ngrok");
        assert!(info.install_command.starts_with("Download"));
    }

    #[test]
    fn unknown_provider_falls_back_to_cloudflare() {
        let info = get_tunnel_dependency_install_info_for_platform("unknown", "win32");
        assert_eq!(info.dependency, "cloudflared");
    }

    #[test]
    fn unknown_platform_falls_back_to_linux() {
        let info = get_tunnel_dependency_install_info_for_platform("ngrok", "freebsd");
        assert_eq!(info.platform, "linux");
    }
}
