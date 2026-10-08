use std::{
    path::Path,
    process::{Output, Stdio},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, sync::Mutex, time::timeout};

const MAX_FILES: usize = 12;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_FILE_BYTES: usize = 1024 * 1024;
const MAX_COMMANDS: usize = 8;
const MAX_COMMAND_BYTES: usize = 4_096;
const MAX_PACKAGES: usize = 24;
const MAX_PACKAGE_BYTES: usize = 128;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const GUEST_MEMORY_LIMIT_MIB: usize = 1_536;
const GUEST_TASK_LIMIT: usize = 256;

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionPlan {
    pub execute: bool,
    pub explanation: String,
    #[serde(default)]
    pub system_packages: Vec<String>,
    #[serde(default)]
    pub files: Vec<ExecutionFile>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output: String,
}

#[async_trait]
pub trait CodeRunner: Send + Sync {
    async fn execute(&self, plan: ExecutionPlan) -> Result<ExecutionResult>;
}

pub type SharedCodeRunner = Arc<dyn CodeRunner>;

pub struct LimaRunner {
    instance: String,
    template: String,
    execution_timeout: Duration,
    execution_lock: Mutex<()>,
}

impl LimaRunner {
    pub fn new(instance: String, template: String, execution_timeout: Duration) -> Result<Self> {
        if execution_timeout.is_zero() {
            bail!("execution timeout must be greater than zero");
        }
        if instance.is_empty()
            || !instance.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            bail!("LIMA_INSTANCE contains unsupported characters");
        }
        Ok(Self {
            instance,
            template,
            execution_timeout,
            execution_lock: Mutex::new(()),
        })
    }

    pub async fn ensure_ready(&self) -> Result<()> {
        let instances =
            run_limactl_output(["--tty=false", "list", "--quiet"], Duration::from_secs(30))
                .await
                .context("failed to list Lima instances")?;
        if !instances.status.success()
            && !limactl_reports_no_instances(&String::from_utf8_lossy(&instances.stderr))
            && !limactl_reports_no_instances(&String::from_utf8_lossy(&instances.stdout))
        {
            bail!(
                "limactl list exited unsuccessfully: {}",
                String::from_utf8_lossy(&instances.stderr).trim()
            );
        }
        if instance_list_contains(&instances.stdout, &self.instance) {
            run_limactl(
                ["--tty=false", "start", self.instance.as_str()],
                Duration::from_secs(180),
            )
            .await
            .context("failed to start existing Lima instance")?;
            return Ok(());
        }

        let template = Path::new(&self.template)
            .canonicalize()
            .with_context(|| format!("Lima template does not exist: {}", self.template))?;
        let template = template
            .to_str()
            .context("Lima template path is not valid UTF-8")?;
        run_limactl(
            [
                "--tty=false",
                "start",
                "--name",
                self.instance.as_str(),
                template,
            ],
            Duration::from_secs(600),
        )
        .await
        .context("failed to create or start isolated Lima instance")?;
        Ok(())
    }
}

#[async_trait]
impl CodeRunner for LimaRunner {
    async fn execute(&self, plan: ExecutionPlan) -> Result<ExecutionResult> {
        validate_plan(&plan)?;
        if !plan.execute {
            bail!("execution plan is marked as disabled");
        }
        // apt and the VM-wide CPU/memory budget are shared. Running plans concurrently would
        // break both the package-manager and resource-isolation contracts.
        let _execution_guard = self.execution_lock.lock().await;

        let script = build_guest_script(&plan, self.execution_timeout)?;
        let mut child = Command::new("limactl")
            .args(["--tty=false", "shell", self.instance.as_str(), "bash", "-s"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to launch limactl")?;
        child
            .stdin
            .take()
            .context("failed to open limactl stdin")?
            .write_all(script.as_bytes())
            .await
            .context("failed to send execution plan to Lima")?;

        let host_timeout = self.execution_timeout + Duration::from_secs(15);
        let output = match timeout(host_timeout, child.wait_with_output()).await {
            Ok(output) => output.context("limactl execution failed")?,
            Err(_) => {
                return Ok(ExecutionResult {
                    exit_code: None,
                    timed_out: true,
                    output: "execution exceeded the host-side time limit".to_owned(),
                });
            }
        };
        let mut combined = output.stdout;
        if !output.stderr.is_empty() {
            combined.extend_from_slice(b"\n[limactl stderr]\n");
            combined.extend_from_slice(&output.stderr);
        }
        combined.truncate(MAX_OUTPUT_BYTES);

        Ok(ExecutionResult {
            exit_code: output.status.code(),
            timed_out: matches!(output.status.code(), Some(124) | Some(137)),
            output: String::from_utf8_lossy(&combined).into_owned(),
        })
    }
}

async fn run_limactl<'a>(
    args: impl IntoIterator<Item = &'a str>,
    command_timeout: Duration,
) -> Result<Output> {
    let output = run_limactl_output(args, command_timeout).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("limactl exited unsuccessfully: {}", stderr.trim());
    }
    Ok(output)
}

async fn run_limactl_output<'a>(
    args: impl IntoIterator<Item = &'a str>,
    command_timeout: Duration,
) -> Result<Output> {
    timeout(
        command_timeout,
        Command::new("limactl")
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("limactl timed out")?
    .context("failed to launch limactl")
}

fn limactl_reports_no_instances(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no instance") || stderr.contains("no instances")
}

fn instance_list_contains(stdout: &[u8], instance: &str) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .any(|line| line.trim() == instance)
}

fn validate_plan(plan: &ExecutionPlan) -> Result<()> {
    if plan.files.len() > MAX_FILES {
        bail!("execution plan contains too many files");
    }
    if plan.commands.is_empty() || plan.commands.len() > MAX_COMMANDS {
        bail!("execution plan contains an invalid number of commands");
    }
    if plan.system_packages.len() > MAX_PACKAGES {
        bail!("execution plan contains too many packages");
    }

    let mut total_bytes = 0;
    for file in &plan.files {
        validate_relative_path(&file.path)?;
        let size = file.content.len();
        if size > MAX_FILE_BYTES {
            bail!("execution file is too large: {}", file.path);
        }
        total_bytes += size;
    }
    if total_bytes > MAX_TOTAL_FILE_BYTES {
        bail!("execution files exceed the total size limit");
    }
    for command in &plan.commands {
        if command.trim().is_empty() || command.len() > MAX_COMMAND_BYTES {
            bail!("execution command is empty or too large");
        }
    }
    for package in &plan.system_packages {
        if package.is_empty()
            || package.len() > MAX_PACKAGE_BYTES
            || package.starts_with('-')
            || !package.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
            })
        {
            bail!("invalid apt package name");
        }
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
        || !path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.' | b'/'))
    {
        bail!("execution file path is unsafe: {}", path.display());
    }
    Ok(())
}

fn build_guest_script(plan: &ExecutionPlan, execution_timeout: Duration) -> Result<String> {
    validate_plan(plan)?;
    let mut script = String::from(
        "#!/bin/bash\nset -euo pipefail\nworkdir=$(sudo -n mktemp -d /opt/discord-code-XXXXXX)\ntrap 'sudo -n rm -rf \"$workdir\"' EXIT\nsudo -n chown \"$(id -u):$(id -g)\" \"$workdir\"\ncd \"$workdir\"\numask 077\n",
    );
    for file in &plan.files {
        if let Some(parent) = Path::new(&file.path).parent()
            && !parent.as_os_str().is_empty()
        {
            script.push_str(&format!("mkdir -p '{}'\n", parent.display()));
        }
        script.push_str(&format!(
            "printf '%s' '{}' | base64 --decode > '{}'\n",
            STANDARD.encode(file.content.as_bytes()),
            file.path
        ));
    }
    if !plan.system_packages.is_empty() {
        script.push_str("sudo -n apt-get update\n");
        script.push_str("sudo -n apt-get install -y --no-install-recommends");
        for package in &plan.system_packages {
            script.push(' ');
            script.push_str(package);
        }
        script.push('\n');
    }
    let command_script = format!("set -euo pipefail\n{}\n", plan.commands.join("\n"));
    script.push_str(&format!(
        "printf '%s' '{}' | base64 --decode > .discord-run.sh\n",
        STANDARD.encode(command_script.as_bytes())
    ));
    let wrapper_script = format!(
        "#!/bin/bash\nset +e\nulimit -f 524288\ntimeout --kill-after=2s {}s bash .discord-run.sh 2>&1 | head -c {}\nstatus=${{PIPESTATUS[0]}}\nexit \"$status\"\n",
        execution_timeout.as_secs(),
        MAX_OUTPUT_BYTES
    );
    script.push_str(&format!(
        "printf '%s' '{}' | base64 --decode > .discord-wrapper.sh\n",
        STANDARD.encode(wrapper_script.as_bytes())
    ));
    script.push_str("cd /\nsudo -n chown -R nobody:nogroup \"$workdir\"\nset +e\n");
    script.push_str(&format!(
        "sudo -n systemd-run --quiet --wait --pipe --collect \\\n  --uid=nobody \\\n  --working-directory=\"$workdir\" \\\n  --setenv=\"HOME=$workdir\" \\\n  --setenv='PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' \\\n  --setenv='LANG=C.UTF-8' \\\n  --setenv='RUSTUP_HOME=/usr/local/rustup' \\\n  --setenv='CARGO_HOME=/usr/local/cargo' \\\n  --setenv='ELAN_HOME=/usr/local/elan' \\\n  --property=NoNewPrivileges=yes \\\n  --property=MemoryMax={GUEST_MEMORY_LIMIT_MIB}M \\\n  --property=MemorySwapMax=0 \\\n  --property=CPUQuota=150% \\\n  --property=TasksMax={GUEST_TASK_LIMIT} \\\n  --property=KillMode=control-group \\\n  --property=RuntimeMaxSec={}s \\\n  --property=PrivateTmp=yes \\\n  --property=PrivateDevices=yes \\\n  --property=ProtectSystem=strict \\\n  --property=ProtectHome=yes \\\n  --property=\"ReadWritePaths=$workdir\" \\\n  --property=ProtectKernelTunables=yes \\\n  --property=ProtectKernelModules=yes \\\n  --property=ProtectControlGroups=yes \\\n  --property=ProtectHostname=yes \\\n  --property=RestrictSUIDSGID=yes \\\n  --property=RestrictRealtime=yes \\\n  --property=LockPersonality=yes \\\n  /bin/bash \"$workdir/.discord-wrapper.sh\"\nstatus=$?\nexit \"$status\"\n",
        execution_timeout.as_secs().saturating_add(5)
    ));
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_plan() -> ExecutionPlan {
        ExecutionPlan {
            execute: true,
            explanation: "compile sample".into(),
            system_packages: vec![],
            files: vec![ExecutionFile {
                path: "src/main.rs".into(),
                content: "fn main() { println!(\"ok\"); }".into(),
            }],
            commands: vec!["rustc src/main.rs && ./src/main".into()],
        }
    }

    #[test]
    fn rejects_path_traversal() {
        let mut plan = valid_plan();
        plan.files[0].path = "../host.txt".into();
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn guest_script_does_not_interpolate_source_or_command() {
        let plan = valid_plan();
        let script = build_guest_script(&plan, Duration::from_secs(20)).unwrap();
        assert!(!script.contains(&plan.files[0].content));
        assert!(!script.contains(&plan.commands[0]));
        assert!(script.contains("base64 --decode"));
        assert!(script.contains("systemd-run"));
        assert!(script.contains("--uid=nobody"));
        assert!(script.contains("ProtectSystem=strict"));
        assert!(script.contains("MemoryMax=1536M"));
        assert!(script.contains("RUSTUP_HOME=/usr/local/rustup"));
        assert!(script.contains("CARGO_HOME=/usr/local/cargo"));
        assert!(script.contains("ELAN_HOME=/usr/local/elan"));
        assert!(!script.contains("+  --"));
    }

    #[test]
    fn instance_names_are_matched_as_complete_lines() {
        let output = b"other\ndiscord-code-runner\ndiscord-code-runner-2\n";
        assert!(instance_list_contains(output, "discord-code-runner"));
        assert!(!instance_list_contains(output, "discord"));
    }

    #[test]
    fn rejects_unreasonably_long_package_names() {
        let mut plan = valid_plan();
        plan.system_packages = vec!["a".repeat(MAX_PACKAGE_BYTES + 1)];
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn accepts_lima_no_instance_list_message() {
        assert!(limactl_reports_no_instances("No instance found"));
        assert!(limactl_reports_no_instances("no instances"));
        assert!(!limactl_reports_no_instances("permission denied"));
    }
}
