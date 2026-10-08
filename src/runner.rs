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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
    time::timeout,
};

const MAX_FILES: usize = 12;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_FILE_BYTES: usize = 1024 * 1024;
const MAX_COMMANDS: usize = 8;
const MAX_COMMAND_BYTES: usize = 4_096;
const MAX_PACKAGES: usize = 24;
const MAX_PACKAGE_BYTES: usize = 128;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TRANSFER_BYTES: usize = MAX_OUTPUT_BYTES + MAX_ARTIFACT_BYTES * 2;
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
    // Output paths are selected by trusted tools, never by model-supplied commands.
    #[serde(skip)]
    pub output_files: Vec<String>,
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
    #[serde(skip)]
    pub artifacts: Vec<GeneratedArtifact>,
}

#[derive(Debug, Clone)]
pub struct GeneratedArtifact {
    pub filename: String,
    pub bytes: Vec<u8>,
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
            bail!("{}", limactl_failure("limactl list", &instances));
        }
        if instance_list_contains(&instances.stdout, &self.instance) {
            run_limactl(
                ["--tty=false", "start", self.instance.as_str()],
                Duration::from_secs(180),
            )
            .await
            .context("failed to start existing Lima instance")?;
            self.ensure_diagram_dependencies().await?;
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
        self.ensure_diagram_dependencies().await?;
        Ok(())
    }

    async fn ensure_diagram_dependencies(&self) -> Result<()> {
        let script = "set -e\nif ! (command -v lualatex >/dev/null && command -v pdftoppm >/dev/null && kpsewhich standalone.cls >/dev/null && kpsewhich tikz.sty >/dev/null && kpsewhich luatexja.sty >/dev/null); then sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends texlive-luatex texlive-pictures texlive-latex-extra texlive-lang-japanese poppler-utils; fi\n";
        let output = timeout(Duration::from_secs(600), async {
            let mut child = limactl_command()?
                .args(["--tty=false", "shell", self.instance.as_str(), "bash", "-s"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            child
                .stdin
                .take()
                .context("missing provision stdin")?
                .write_all(script.as_bytes())
                .await?;
            Ok::<_, anyhow::Error>(child.wait_with_output().await?)
        })
        .await
        .context("TikZ dependency provisioning timed out")??;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            limactl_failure("failed to prepare TikZ dependencies", &output)
        );
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
        let mut child = limactl_command()?
            .args(["--tty=false", "shell", self.instance.as_str(), "bash", "-s"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to launch limactl")?;
        let host_timeout = self.execution_timeout + Duration::from_secs(15);
        let execution = async move {
            let mut stdin = child.stdin.take().context("failed to open limactl stdin")?;
            let write_input = async move {
                stdin.write_all(script.as_bytes()).await?;
                stdin.shutdown().await
            };
            let mut stdout = child
                .stdout
                .take()
                .context("missing limactl stdout")?
                .take((MAX_TRANSFER_BYTES + 1) as u64);
            let mut stderr = child
                .stderr
                .take()
                .context("missing limactl stderr")?
                .take((MAX_OUTPUT_BYTES + 1) as u64);
            let mut stdout_bytes = Vec::new();
            let mut stderr_bytes = Vec::new();
            let (status, _, _, _) = tokio::try_join!(
                child.wait(),
                write_input,
                stdout.read_to_end(&mut stdout_bytes),
                stderr.read_to_end(&mut stderr_bytes),
            )?;
            anyhow::ensure!(
                stdout_bytes.len() <= MAX_TRANSFER_BYTES,
                "runner output exceeds transfer limit"
            );
            Ok::<_, anyhow::Error>((status, stdout_bytes, stderr_bytes))
        };
        let (status, stdout, stderr) = match timeout(host_timeout, execution).await {
            Ok(output) => output.context("limactl execution failed")?,
            Err(_) => {
                return Ok(ExecutionResult {
                    exit_code: None,
                    timed_out: true,
                    output: "execution exceeded the host-side time limit".to_owned(),
                    artifacts: vec![],
                });
            }
        };
        let (mut combined, artifacts) =
            decode_artifacts(&stdout, &plan.output_files, status.success())?;
        if !stderr.is_empty() {
            combined.extend_from_slice(b"\n[limactl stderr]\n");
            combined.extend_from_slice(&stderr);
        }
        combined.truncate(MAX_OUTPUT_BYTES);

        Ok(ExecutionResult {
            exit_code: status.code(),
            timed_out: matches!(status.code(), Some(124) | Some(137)),
            output: String::from_utf8_lossy(&combined).into_owned(),
            artifacts,
        })
    }
}

fn decode_artifacts(
    stdout: &[u8],
    expected: &[String],
    success: bool,
) -> Result<(Vec<u8>, Vec<GeneratedArtifact>)> {
    if expected.is_empty() || !success {
        return Ok((
            stdout.iter().copied().take(MAX_OUTPUT_BYTES).collect(),
            vec![],
        ));
    }
    let text = std::str::from_utf8(stdout).context("invalid artifact transport")?;
    let (log, payload) = text
        .rsplit_once("\nDISCORD_ARTIFACTS=")
        .context("missing artifact transport")?;
    #[derive(Deserialize)]
    struct WireArtifact {
        filename: String,
        data: String,
    }
    let wire: Vec<WireArtifact> = serde_json::from_str(payload.trim())?;
    anyhow::ensure!(wire.len() == expected.len(), "missing generated artifact");
    let mut artifacts = Vec::new();
    for (artifact, name) in wire.into_iter().zip(expected) {
        anyhow::ensure!(artifact.filename == *name, "unexpected artifact filename");
        anyhow::ensure!(
            artifact.data.len() <= MAX_ARTIFACT_BYTES.div_ceil(3) * 4,
            "artifact transport too large"
        );
        let bytes = STANDARD.decode(artifact.data)?;
        anyhow::ensure!(
            bytes.len() <= MAX_ARTIFACT_BYTES && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "invalid PNG artifact"
        );
        artifacts.push(GeneratedArtifact {
            filename: artifact.filename,
            bytes,
        });
    }
    Ok((
        log.as_bytes()
            .iter()
            .copied()
            .take(MAX_OUTPUT_BYTES)
            .collect(),
        artifacts,
    ))
}

// All Lima subprocesses must inherit the same compatible Windows SSH toolchain.
// Only the child environment is changed; the shell/user PATH remains untouched.
fn limactl_command() -> Result<Command> {
    #[allow(unused_mut)]
    let mut command = Command::new("limactl");
    #[cfg(windows)]
    {
        let search_path = std::env::var_os("PATH").unwrap_or_default();
        if let Some(directory) = git_unix_tools_directory(
            &search_path,
            std::env::var_os("ProgramFiles")
                .map(std::path::PathBuf::from)
                .as_deref(),
            std::env::var_os("LOCALAPPDATA")
                .map(std::path::PathBuf::from)
                .as_deref(),
        ) {
            let child_path = std::env::join_paths(
                std::iter::once(directory.clone()).chain(std::env::split_paths(&search_path)),
            )
            .context("failed to configure Lima child PATH")?;
            command.env("PATH", child_path);
            tracing::debug!(directory = %directory.display(), "selected Git SSH tools for Lima");
        }
    }
    Ok(command)
}

#[cfg(any(windows, test))]
fn git_unix_tools_directory(
    search_path: &std::ffi::OsStr,
    program_files: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    for directory in std::env::split_paths(search_path) {
        if directory.join("git.exe").is_file()
            && let Some(root) = directory.parent()
        {
            candidates.push(root.join("usr").join("bin"));
        }
    }
    if let Some(root) = program_files {
        candidates.push(root.join("Git").join("usr").join("bin"));
    }
    if let Some(root) = local_app_data {
        candidates.push(root.join("Programs").join("Git").join("usr").join("bin"));
    }
    candidates.into_iter().find(|directory| {
        ["ssh.exe", "scp.exe", "ssh-keygen.exe"]
            .iter()
            .all(|name| directory.join(name).is_file())
    })
}

fn limactl_failure(operation: &str, output: &Output) -> String {
    let bounded = |bytes: &[u8]| {
        let text: String = String::from_utf8_lossy(bytes)
            .trim()
            .chars()
            .take(1000)
            .collect();
        if text.is_empty() {
            "<empty>".to_string()
        } else {
            text
        }
    };
    let hint = if output.status.code() == Some(255)
        && output.stdout.is_empty()
        && output.stderr.is_empty()
    {
        "; Lima SSH failed without diagnostics: on Windows, install Git SSH tools with scripts/setup-lima-windows.ps1"
    } else {
        ""
    };
    format!(
        "{operation} ({}): stdout={} stderr={}{hint}",
        output.status,
        bounded(&output.stdout),
        bounded(&output.stderr)
    )
}

async fn run_limactl<'a>(
    args: impl IntoIterator<Item = &'a str>,
    command_timeout: Duration,
) -> Result<Output> {
    let output = run_limactl_output(args, command_timeout).await?;
    if !output.status.success() {
        bail!(
            "{}",
            limactl_failure("limactl exited unsuccessfully", &output)
        );
    }
    Ok(output)
}

async fn run_limactl_output<'a>(
    args: impl IntoIterator<Item = &'a str>,
    command_timeout: Duration,
) -> Result<Output> {
    timeout(
        command_timeout,
        limactl_command()?
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
    anyhow::ensure!(plan.output_files.len() <= 1, "too many output files");
    for path in &plan.output_files {
        validate_relative_path(path)?;
        anyhow::ensure!(
            !path.contains('/') && path.ends_with(".png"),
            "output must be a PNG basename"
        );
    }
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
    anyhow::ensure!(
        !path.is_empty()
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "execution file path must contain normal components"
    );
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
    if !plan.output_files.is_empty() {
        // Replace only the final exit: collection must run after the control group stops.
        let ending = "exit \"$status\"\n";
        script.truncate(script.len() - ending.len());
        script.push_str(&format!(
            "sudo -n python3 -I -c \"$(printf '%s' '{}' | base64 --decode)\" \"$workdir\" \"$status\" {} || exit 1\nexit \"$status\"\n",
            STANDARD.encode(include_str!("runner/collect_artifacts.py")),
            plan.output_files.iter().map(|name| format!("'{name}'")).collect::<Vec<_>>().join(" ")
        ));
    }
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
            output_files: vec![],
        }
    }

    #[test]
    fn discovers_complete_git_ssh_toolchain_without_changing_parent_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Portable Git");
        let cmd = root.join("cmd");
        let tools = root.join("usr").join("bin");
        std::fs::create_dir_all(&cmd).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(cmd.join("git.exe"), b"dummy").unwrap();
        let path = std::env::join_paths([cmd]).unwrap();
        assert!(git_unix_tools_directory(&path, None, None).is_none());
        for name in ["ssh.exe", "scp.exe", "ssh-keygen.exe"] {
            std::fs::write(tools.join(name), b"dummy").unwrap();
        }
        assert_eq!(git_unix_tools_directory(&path, None, None), Some(tools));
        let original = std::env::var_os("PATH");
        let _ = limactl_command().unwrap();
        assert_eq!(std::env::var_os("PATH"), original);
    }

    #[test]
    fn discovers_standard_and_per_user_git_installations() {
        let temp = tempfile::tempdir().unwrap();
        let system_tools = temp.path().join("Git").join("usr").join("bin");
        let user_tools = temp
            .path()
            .join("Programs")
            .join("Git")
            .join("usr")
            .join("bin");
        for directory in [&system_tools, &user_tools] {
            std::fs::create_dir_all(directory).unwrap();
            for name in ["ssh.exe", "scp.exe", "ssh-keygen.exe"] {
                std::fs::write(directory.join(name), b"dummy").unwrap();
            }
        }
        assert_eq!(
            git_unix_tools_directory(std::ffi::OsStr::new(""), Some(temp.path()), None),
            Some(system_tools)
        );
        assert_eq!(
            git_unix_tools_directory(std::ffi::OsStr::new(""), None, Some(temp.path())),
            Some(user_tools)
        );
    }

    #[test]
    fn failure_diagnostics_include_exit_status_even_with_empty_output() {
        #[cfg(windows)]
        let status =
            <std::process::ExitStatus as std::os::windows::process::ExitStatusExt>::from_raw(255);
        #[cfg(unix)]
        let status =
            <std::process::ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(255 << 8);
        let output = Output {
            status,
            stdout: vec![],
            stderr: vec![],
        };
        let diagnostic = limactl_failure("dependency check", &output);
        assert!(diagnostic.contains("255"));
        assert!(diagnostic.contains("stdout=<empty> stderr=<empty>"));
        assert!(diagnostic.contains("SSH"));
        let output = Output {
            status,
            stdout: b"dependency missing".to_vec(),
            stderr: b"package failure".to_vec(),
        };
        let diagnostic = limactl_failure("dependency check", &output);
        assert!(diagnostic.contains("dependency missing"));
        assert!(diagnostic.contains("package failure"));
    }

    #[test]
    fn artifact_transport_checks_names_payloads_and_exit_status() {
        let expected = vec!["diagram.png".to_string()];
        let payload = serde_json::json!([{"filename": "diagram.png", "data": STANDARD.encode(b"\x89PNG\r\n\x1a\n") }]);
        let stdout = format!("log\nDISCORD_ARTIFACTS={payload}\n");
        let (log, artifacts) = decode_artifacts(stdout.as_bytes(), &expected, true).unwrap();
        assert_eq!(log, b"log");
        assert_eq!(artifacts.len(), 1);
        assert!(decode_artifacts(stdout.as_bytes(), &["other.png".into()], true).is_err());
        assert!(decode_artifacts(b"log", &expected, true).is_err());
        assert!(decode_artifacts(b"\nDISCORD_ARTIFACTS=[]", &expected, true).is_err());
        assert!(
            decode_artifacts(
                b"\nDISCORD_ARTIFACTS=[{\"filename\":\"diagram.png\",\"data\":\"dGV4dA==\"}]",
                &expected,
                true
            )
            .is_err()
        );
        assert!(
            decode_artifacts(stdout.as_bytes(), &expected, false)
                .unwrap()
                .1
                .is_empty()
        );
    }

    #[test]
    fn model_plans_cannot_request_artifact_extraction() {
        let plan: ExecutionPlan = serde_json::from_value(serde_json::json!({
            "execute": true, "explanation": "test", "commands": ["true"], "output_files": ["secret.png"]
        })).unwrap();
        assert!(plan.output_files.is_empty());
        let mut plan = valid_plan();
        plan.output_files = vec!["../diagram.png".into()];
        assert!(validate_plan(&plan).is_err());
        plan.output_files = vec!["sub/diagram.png".into()];
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn rejects_path_traversal() {
        let mut plan = valid_plan();
        for path in ["../host.txt", "", ".", "a/./b", "a//b", "a/"] {
            plan.files[0].path = path.into();
            assert!(validate_plan(&plan).is_err(), "{path}");
        }
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
