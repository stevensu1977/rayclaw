use crate::config::Config;
use crate::logging;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_INSTANCE_NAME: &str = "default";
const DEFAULT_LOG_LINES: usize = 200;

fn linux_service_name(name: &str) -> String {
    format!("rayclaw-gateway-{name}.service")
}

fn mac_label(name: &str) -> String {
    format!("ai.rayclaw.gateway.{name}")
}

fn log_stdout_file(name: &str) -> String {
    format!("rayclaw-gateway-{name}.log")
}

fn log_stderr_file(name: &str) -> String {
    format!("rayclaw-gateway-{name}.error.log")
}

#[derive(Debug, Clone)]
struct ServiceContext {
    exe_path: PathBuf,
    working_dir: PathBuf,
    config_path: Option<PathBuf>,
    runtime_logs_dir: PathBuf,
}

/// Validate an instance name: alphanumeric + hyphens, 1-64 chars.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(anyhow!(
            "Instance name must be 1-64 characters, got {}",
            name.len()
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(anyhow!(
            "Instance name must contain only alphanumeric characters and hyphens: '{name}'"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(anyhow!(
            "Instance name must not start or end with a hyphen: '{name}'"
        ));
    }
    Ok(())
}

/// Extract `--name <NAME>` from args, returning (name, remaining_args).
fn extract_name(args: &[String]) -> Result<(String, Vec<String>)> {
    let mut name = DEFAULT_INSTANCE_NAME.to_string();
    let mut remaining = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--name" {
            name = iter
                .next()
                .ok_or_else(|| anyhow!("--name requires a value"))?
                .clone();
        } else {
            remaining.push(arg.clone());
        }
    }
    validate_name(&name)?;
    Ok((name, remaining))
}

pub fn handle_gateway_cli(args: &[String]) -> Result<()> {
    let Some(action) = args.first().map(|s| s.as_str()) else {
        print_gateway_help();
        return Ok(());
    };

    // `list` and `help` don't need --name parsing
    match action {
        "list" => return list_instances(),
        "help" | "--help" | "-h" => {
            print_gateway_help();
            return Ok(());
        }
        _ => {}
    }

    let (name, rest) = extract_name(&args[1..])?;

    match action {
        "install" => install(&name),
        "uninstall" => uninstall(&name),
        "start" => start(&name),
        "stop" => stop(&name),
        "status" => status(&name),
        "logs" => logs(&name, rest.first().map(|s| s.as_str())),
        _ => Err(anyhow!(
            "Unknown gateway action: {}. Run: rayclaw gateway help",
            action
        )),
    }
}

pub fn print_gateway_help() {
    println!(
        r#"Gateway service management (multi-instance)

USAGE:
    rayclaw gateway <ACTION> [--name <NAME>]

ACTIONS:
    install      Install and enable persistent gateway service
    uninstall    Disable and remove persistent gateway service
    start        Start gateway service
    stop         Stop gateway service
    status       Show gateway service status
    logs [N]     Show last N lines of gateway logs (default: 200)
    list         List all installed gateway instances
    help         Show this message

OPTIONS:
    --name <NAME>   Instance name (default: "default")

EXAMPLES:
    rayclaw gateway install                  Install with name "default"
    rayclaw gateway install --name bot-cn    Install as "bot-cn"
    rayclaw gateway status --name bot-cn     Check status of "bot-cn"
    rayclaw gateway list                     Show all instances
"#
    );
}

fn install(name: &str) -> Result<()> {
    let ctx = build_context()?;
    if cfg!(target_os = "macos") {
        install_macos(&ctx, name)
    } else if cfg!(target_os = "linux") {
        install_linux(&ctx, name)
    } else {
        Err(anyhow!(
            "Gateway service is only supported on macOS and Linux"
        ))
    }
}

fn uninstall(name: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        uninstall_macos(name)
    } else if cfg!(target_os = "linux") {
        uninstall_linux(name)
    } else {
        Err(anyhow!(
            "Gateway service is only supported on macOS and Linux"
        ))
    }
}

fn start(name: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        start_macos(name)
    } else if cfg!(target_os = "linux") {
        start_linux(name)
    } else {
        Err(anyhow!(
            "Gateway service is only supported on macOS and Linux"
        ))
    }
}

fn stop(name: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        stop_macos(name)
    } else if cfg!(target_os = "linux") {
        stop_linux(name)
    } else {
        Err(anyhow!(
            "Gateway service is only supported on macOS and Linux"
        ))
    }
}

fn status(name: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        status_macos(name)
    } else if cfg!(target_os = "linux") {
        status_linux(name)
    } else {
        Err(anyhow!(
            "Gateway service is only supported on macOS and Linux"
        ))
    }
}

fn logs(name: &str, lines_arg: Option<&str>) -> Result<()> {
    let lines = parse_log_lines(lines_arg)?;
    let ctx = build_context()?;
    println!(
        "== gateway logs [{name}]: {} ==",
        ctx.runtime_logs_dir.display()
    );
    let tailed = logging::read_last_lines_from_logs(&ctx.runtime_logs_dir, lines)?;
    if tailed.is_empty() {
        println!("(no log lines found)");
    } else {
        println!("{}", tailed.join("\n"));
    }
    Ok(())
}

fn list_instances() -> Result<()> {
    if cfg!(target_os = "macos") {
        list_instances_macos()
    } else if cfg!(target_os = "linux") {
        list_instances_linux()
    } else {
        Err(anyhow!(
            "Gateway service is only supported on macOS and Linux"
        ))
    }
}

fn list_instances_linux() -> Result<()> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let unit_dir = PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user");

    let pattern = unit_dir.join("rayclaw-gateway-*.service");
    let pattern_str = pattern.to_string_lossy();

    let entries: Vec<_> = glob::glob(&pattern_str)
        .map(|paths| paths.filter_map(|p| p.ok()).collect())
        .unwrap_or_default();

    if entries.is_empty() {
        println!("No gateway instances installed.");
        return Ok(());
    }

    println!("NAME                 STATUS       UNIT");
    println!("----                 ------       ----");

    for entry in &entries {
        let filename = entry
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        // rayclaw-gateway-<name>.service → extract <name>
        let name = filename
            .strip_prefix("rayclaw-gateway-")
            .and_then(|s| s.strip_suffix(".service"))
            .unwrap_or(&filename);

        let service = linux_service_name(name);
        let status = match run_command("systemctl", &["--user", "is-active", &service]) {
            Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
            Err(_) => "unknown".to_string(),
        };

        println!("{:<20} {:<12} {}", name, status, entry.display());
    }
    Ok(())
}

fn list_instances_macos() -> Result<()> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let agents_dir = PathBuf::from(home).join("Library").join("LaunchAgents");
    let pattern = agents_dir.join("ai.rayclaw.gateway.*.plist");
    let pattern_str = pattern.to_string_lossy();

    let entries: Vec<_> = glob::glob(&pattern_str)
        .map(|paths| paths.filter_map(|p| p.ok()).collect())
        .unwrap_or_default();

    if entries.is_empty() {
        println!("No gateway instances installed.");
        return Ok(());
    }

    println!("NAME                 PLIST");
    println!("----                 -----");

    for entry in &entries {
        let filename = entry
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        // ai.rayclaw.gateway.<name>.plist → extract <name>
        let name = filename
            .strip_prefix("ai.rayclaw.gateway.")
            .and_then(|s| s.strip_suffix(".plist"))
            .unwrap_or(&filename);

        println!("{:<20} {}", name, entry.display());
    }
    Ok(())
}

fn parse_log_lines(lines_arg: Option<&str>) -> Result<usize> {
    match lines_arg {
        None => Ok(DEFAULT_LOG_LINES),
        Some(raw) => {
            let parsed = raw
                .parse::<usize>()
                .with_context(|| format!("Invalid log line count: {}", raw))?;
            if parsed == 0 {
                return Err(anyhow!("Log line count must be greater than 0"));
            }
            Ok(parsed)
        }
    }
}

fn build_context() -> Result<ServiceContext> {
    let exe_path = std::env::current_exe().context("Failed to resolve current binary path")?;
    let working_dir = std::env::current_dir().context("Failed to resolve current directory")?;
    let config_path = resolve_config_path(&working_dir);
    let runtime_logs_dir = resolve_runtime_logs_dir(&working_dir);

    Ok(ServiceContext {
        exe_path,
        working_dir,
        config_path,
        runtime_logs_dir,
    })
}

fn resolve_config_path(cwd: &Path) -> Option<PathBuf> {
    if let Ok(from_env) = std::env::var("RAYCLAW_CONFIG") {
        let path = PathBuf::from(from_env);
        return Some(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        });
    }

    for candidate in ["rayclaw.config.yaml", "rayclaw.config.yml"] {
        let path = cwd.join(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn resolve_runtime_logs_dir(cwd: &Path) -> PathBuf {
    match Config::load() {
        Ok(cfg) => PathBuf::from(cfg.runtime_data_dir()).join("logs"),
        Err(_) => cwd.join("runtime").join("logs"),
    }
}

fn run_command(cmd: &str, args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("Failed to execute command: {} {}", cmd, args.join(" ")))?;
    Ok(output)
}

fn ensure_success(output: std::process::Output, cmd: &str, args: &[&str]) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(anyhow!(
        "Command failed: {} {}\nstdout: {}\nstderr: {}",
        cmd,
        args.join(" "),
        stdout.trim(),
        stderr.trim()
    ))
}

// ── Linux (systemd --user) ──────────────────────────────────────────────

fn linux_unit_path(name: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user")
        .join(linux_service_name(name)))
}

fn render_linux_unit(ctx: &ServiceContext, name: &str) -> String {
    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str(&format!("Description=RayClaw Gateway [{name}]\n"));
    unit.push_str("After=network.target\n\n");
    unit.push_str("[Service]\n");
    unit.push_str("Type=simple\n");
    unit.push_str(&format!("WorkingDirectory={}\n", ctx.working_dir.display()));
    unit.push_str(&format!("ExecStart={} start\n", ctx.exe_path.display()));
    unit.push_str("Environment=RAYCLAW_GATEWAY=1\n");
    if let Some(config_path) = &ctx.config_path {
        unit.push_str(&format!(
            "Environment=RAYCLAW_CONFIG={}\n",
            config_path.display()
        ));
    }
    unit.push_str("Restart=always\n");
    unit.push_str("RestartSec=5\n\n");
    unit.push_str("[Install]\n");
    unit.push_str("WantedBy=default.target\n");
    unit
}

fn install_linux(ctx: &ServiceContext, name: &str) -> Result<()> {
    let unit_path = linux_unit_path(name)?;
    let unit_dir = unit_path
        .parent()
        .ok_or_else(|| anyhow!("Invalid unit path"))?;
    std::fs::create_dir_all(unit_dir)
        .with_context(|| format!("Failed to create {}", unit_dir.display()))?;
    std::fs::write(&unit_path, render_linux_unit(ctx, name))
        .with_context(|| format!("Failed to write {}", unit_path.display()))?;

    let service = linux_service_name(name);
    ensure_success(
        run_command("systemctl", &["--user", "daemon-reload"])?,
        "systemctl",
        &["--user", "daemon-reload"],
    )?;
    ensure_success(
        run_command("systemctl", &["--user", "enable", "--now", &service])?,
        "systemctl",
        &["--user", "enable", "--now", &service],
    )?;

    println!(
        "Installed and started gateway [{name}]: {}",
        unit_path.display()
    );
    Ok(())
}

fn uninstall_linux(name: &str) -> Result<()> {
    let service = linux_service_name(name);
    let _ = run_command("systemctl", &["--user", "disable", "--now", &service]);
    let _ = run_command("systemctl", &["--user", "daemon-reload"]);

    let unit_path = linux_unit_path(name)?;
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("Failed to remove {}", unit_path.display()))?;
    }
    let _ = run_command("systemctl", &["--user", "daemon-reload"]);
    println!("Uninstalled gateway [{name}]");
    Ok(())
}

fn start_linux(name: &str) -> Result<()> {
    let service = linux_service_name(name);
    ensure_success(
        run_command("systemctl", &["--user", "start", &service])?,
        "systemctl",
        &["--user", "start", &service],
    )?;
    println!("Gateway [{name}] started");
    Ok(())
}

fn stop_linux(name: &str) -> Result<()> {
    let service = linux_service_name(name);
    ensure_success(
        run_command("systemctl", &["--user", "stop", &service])?,
        "systemctl",
        &["--user", "stop", &service],
    )?;
    println!("Gateway [{name}] stopped");
    Ok(())
}

fn status_linux(name: &str) -> Result<()> {
    let service = linux_service_name(name);
    let output = run_command("systemctl", &["--user", "status", &service, "--no-pager"])?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!("Gateway [{name}] is not running"))
    }
}

// ── macOS (launchctl) ───────────────────────────────────────────────────

fn mac_plist_path(name: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", mac_label(name))))
}

fn current_uid() -> Result<String> {
    if let Ok(uid) = std::env::var("UID") {
        if !uid.trim().is_empty() {
            return Ok(uid);
        }
    }
    let output = run_command("id", &["-u"])?;
    if !output.status.success() {
        return Err(anyhow!("Failed to determine user id"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn render_macos_plist(ctx: &ServiceContext, name: &str) -> String {
    let label = mac_label(name);
    let stdout_file = log_stdout_file(name);
    let stderr_file = log_stderr_file(name);

    let mut items = vec![
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>".to_string(),
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">".to_string(),
        "<plist version=\"1.0\">".to_string(),
        "<dict>".to_string(),
        "  <key>Label</key>".to_string(),
        format!("  <string>{label}</string>"),
        "  <key>ProgramArguments</key>".to_string(),
        "  <array>".to_string(),
        format!(
            "    <string>{}</string>",
            xml_escape(&ctx.exe_path.to_string_lossy())
        ),
        "    <string>start</string>".to_string(),
        "  </array>".to_string(),
        "  <key>WorkingDirectory</key>".to_string(),
        format!(
            "  <string>{}</string>",
            xml_escape(&ctx.working_dir.to_string_lossy())
        ),
        "  <key>RunAtLoad</key>".to_string(),
        "  <true/>".to_string(),
        "  <key>KeepAlive</key>".to_string(),
        "  <true/>".to_string(),
        "  <key>StandardOutPath</key>".to_string(),
        format!(
            "  <string>{}</string>",
            xml_escape(&ctx.working_dir.join(&stdout_file).to_string_lossy())
        ),
        "  <key>StandardErrorPath</key>".to_string(),
        format!(
            "  <string>{}</string>",
            xml_escape(&ctx.working_dir.join(&stderr_file).to_string_lossy())
        ),
    ];

    items.push("  <key>EnvironmentVariables</key>".to_string());
    items.push("  <dict>".to_string());
    items.push("    <key>RAYCLAW_GATEWAY</key>".to_string());
    items.push("    <string>1</string>".to_string());
    if let Some(config_path) = &ctx.config_path {
        items.push("    <key>RAYCLAW_CONFIG</key>".to_string());
        items.push(format!(
            "    <string>{}</string>",
            xml_escape(&config_path.to_string_lossy())
        ));
    }
    items.push("  </dict>".to_string());

    items.push("</dict>".to_string());
    items.push("</plist>".to_string());
    items.join("\n")
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\'', "&apos;")
}

fn mac_target_label(name: &str) -> Result<String> {
    let uid = current_uid()?;
    Ok(format!("gui/{uid}/{}", mac_label(name)))
}

fn install_macos(ctx: &ServiceContext, name: &str) -> Result<()> {
    let plist_path = mac_plist_path(name)?;
    let launch_agents = plist_path
        .parent()
        .ok_or_else(|| anyhow!("Invalid plist path"))?;
    std::fs::create_dir_all(launch_agents)
        .with_context(|| format!("Failed to create {}", launch_agents.display()))?;
    std::fs::write(&plist_path, render_macos_plist(ctx, name))
        .with_context(|| format!("Failed to write {}", plist_path.display()))?;

    let _ = stop_macos(name);
    start_macos(name)?;
    println!(
        "Installed and started gateway [{name}]: {}",
        plist_path.display()
    );
    Ok(())
}

fn uninstall_macos(name: &str) -> Result<()> {
    let _ = stop_macos(name);
    let plist_path = mac_plist_path(name)?;
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("Failed to remove {}", plist_path.display()))?;
    }
    println!("Uninstalled gateway [{name}]");
    Ok(())
}

fn start_macos(name: &str) -> Result<()> {
    let target = mac_target_label(name)?;
    let plist_path = mac_plist_path(name)?;
    if !plist_path.exists() {
        return Err(anyhow!(
            "Service not installed. Run: rayclaw gateway install --name {name}"
        ));
    }
    let gui_target = format!("gui/{}", current_uid()?);
    let plist_path_str = plist_path.to_string_lossy().to_string();
    let bootstrap = run_command("launchctl", &["bootstrap", &gui_target, &plist_path_str])?;
    if !bootstrap.status.success() {
        let stderr = String::from_utf8_lossy(&bootstrap.stderr);
        if !(stderr.contains("already loaded") || stderr.contains("already exists")) {
            return Err(anyhow!(
                "Command failed: launchctl bootstrap {} {}\nstderr: {}",
                gui_target,
                plist_path_str,
                stderr.trim()
            ));
        }
    }

    ensure_success(
        run_command("launchctl", &["kickstart", "-k", &target])?,
        "launchctl",
        &["kickstart", "-k", &target],
    )?;
    println!("Gateway [{name}] started");
    Ok(())
}

fn stop_macos(name: &str) -> Result<()> {
    let target = mac_target_label(name)?;
    let output = run_command("launchctl", &["bootout", &target])?;
    if output.status.success() {
        println!("Gateway [{name}] stopped");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such process")
        || stderr.contains("Could not find specified service")
        || stderr.contains("not found")
    {
        return Ok(());
    }

    Err(anyhow!(
        "Failed to stop service: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn status_macos(name: &str) -> Result<()> {
    let target = mac_target_label(name)?;
    let output = run_command("launchctl", &["print", &target])?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!("Gateway [{name}] is not running"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xml_escape() {
        let input = "a&b<c>d\"e'f";
        let escaped = xml_escape(input);
        assert_eq!(escaped, "a&amp;b&lt;c&gt;d&quot;e&apos;f");
    }

    #[test]
    fn test_validate_name_valid() {
        assert!(validate_name("default").is_ok());
        assert!(validate_name("bot-cn").is_ok());
        assert!(validate_name("my-bot-123").is_ok());
        assert!(validate_name("a").is_ok());
    }

    #[test]
    fn test_validate_name_invalid() {
        assert!(validate_name("").is_err());
        assert!(validate_name("-start").is_err());
        assert!(validate_name("end-").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("under_score").is_err());
        assert!(validate_name("dot.name").is_err());
        let long = "a".repeat(65);
        assert!(validate_name(&long).is_err());
    }

    #[test]
    fn test_service_naming() {
        assert_eq!(
            linux_service_name("default"),
            "rayclaw-gateway-default.service"
        );
        assert_eq!(
            linux_service_name("bot-cn"),
            "rayclaw-gateway-bot-cn.service"
        );
        assert_eq!(mac_label("default"), "ai.rayclaw.gateway.default");
        assert_eq!(mac_label("bot-cn"), "ai.rayclaw.gateway.bot-cn");
    }

    #[test]
    fn test_log_file_naming() {
        assert_eq!(log_stdout_file("default"), "rayclaw-gateway-default.log");
        assert_eq!(
            log_stderr_file("bot-cn"),
            "rayclaw-gateway-bot-cn.error.log"
        );
    }

    #[test]
    fn test_extract_name_default() {
        let args: Vec<String> = vec![];
        let (name, rest) = extract_name(&args).unwrap();
        assert_eq!(name, "default");
        assert!(rest.is_empty());
    }

    #[test]
    fn test_extract_name_explicit() {
        let args: Vec<String> = vec!["--name".into(), "bot-cn".into(), "100".into()];
        let (name, rest) = extract_name(&args).unwrap();
        assert_eq!(name, "bot-cn");
        assert_eq!(rest, vec!["100"]);
    }

    #[test]
    fn test_extract_name_missing_value() {
        let args: Vec<String> = vec!["--name".into()];
        assert!(extract_name(&args).is_err());
    }

    #[test]
    fn test_render_linux_unit_contains_instance_name() {
        let ctx = ServiceContext {
            exe_path: PathBuf::from("/usr/local/bin/rayclaw"),
            working_dir: PathBuf::from("/tmp/rayclaw"),
            config_path: Some(PathBuf::from("/tmp/rayclaw/rayclaw.config.yaml")),
            runtime_logs_dir: PathBuf::from("/tmp/rayclaw/runtime/logs"),
        };

        let unit = render_linux_unit(&ctx, "bot-cn");
        assert!(unit.contains("Description=RayClaw Gateway [bot-cn]"));
        assert!(unit.contains("ExecStart=/usr/local/bin/rayclaw start"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("RAYCLAW_GATEWAY=1"));
        assert!(unit.contains("RAYCLAW_CONFIG=/tmp/rayclaw/rayclaw.config.yaml"));
    }

    #[test]
    fn test_render_macos_plist_contains_instance_name() {
        let ctx = ServiceContext {
            exe_path: PathBuf::from("/usr/local/bin/rayclaw"),
            working_dir: PathBuf::from("/tmp/rayclaw"),
            config_path: Some(PathBuf::from("/tmp/rayclaw/rayclaw.config.yaml")),
            runtime_logs_dir: PathBuf::from("/tmp/rayclaw/runtime/logs"),
        };

        let plist = render_macos_plist(&ctx, "bot-en");
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("ai.rayclaw.gateway.bot-en"));
        assert!(plist.contains("<string>start</string>"));
        assert!(plist.contains("RAYCLAW_GATEWAY"));
        assert!(plist.contains("RAYCLAW_CONFIG"));
        assert!(plist.contains("rayclaw-gateway-bot-en.log"));
        assert!(plist.contains("rayclaw-gateway-bot-en.error.log"));
    }

    #[test]
    fn test_parse_log_lines_default_and_custom() {
        assert_eq!(parse_log_lines(None).unwrap(), DEFAULT_LOG_LINES);
        assert_eq!(parse_log_lines(Some("20")).unwrap(), 20);
        assert!(parse_log_lines(Some("0")).is_err());
        assert!(parse_log_lines(Some("abc")).is_err());
    }

    #[test]
    fn test_resolve_runtime_logs_dir_fallback() {
        let dir = resolve_runtime_logs_dir(Path::new("/tmp/rayclaw"));
        assert!(
            dir.ends_with("runtime/logs") || dir.ends_with("rayclaw.data/runtime/logs"),
            "unexpected logs dir: {}",
            dir.display()
        );
    }
}
