use std::{path::Path, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

const MAX_FILES: usize = 12;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_FILE_BYTES: usize = 1024 * 1024;
const MAX_COMMANDS: usize = 8;
const MAX_COMMAND_BYTES: usize = 4_096;
const MAX_PACKAGES: usize = 24;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

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
}

impl LimaRunner {
    pub fn new(instance: String, template: String, execution_timeout: Duration) -> Result<Self> {
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
        })
    }

    pub async fn ensure_ready(&self) -> Result<()> {
        let existing = run_limactl(
            ["--tty=false", "start", self.instance.as_str()],
            Duration::from_secs(90),
        )
        .await;
        if existing.is_ok() {
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
) -> Result<()> {
    let output = timeout(
        command_timeout,
        Command::new("limactl")
            .args(args)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("limactl timed out")?
    .context("failed to launch limactl")?;
    if !output.status.success() {
        bail!("limactl exited unsuccessfully");
    }
    Ok(())
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
        "#!/bin/bash\nset -euo pipefail\nworkdir=$(mktemp -d /tmp/discord-code-XXXXXX)\ntrap 'chmod -R u+w \"$workdir\" 2>/dev/null || true; rm -rf \"$workdir\"' EXIT\ncd \"$workdir\"\n",
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
    script.push_str(&format!(
        "set +e\ntimeout --kill-after=2s {}s bash .discord-run.sh 2>&1 | head -c {}\nstatus=${{PIPESTATUS[0]}}\nexit \"$status\"\n",
        execution_timeout.as_secs(),
        MAX_OUTPUT_BYTES
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
    }
}
