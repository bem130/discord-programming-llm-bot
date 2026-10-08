use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::json;

use crate::runner::{CodeRunner, ExecutionFile, ExecutionPlan, ExecutionResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TikzArguments {
    pub source: String,
}

pub fn tool() -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": "render_tikz_diagram",
            "description": "TikZで図を描きPNGに変換する。成功した図はレビュー済み回答にDiscord添付として送られる。sourceには日本語を含められる完全なLaTeX文書を渡す。\\documentclass[tikz,border=6pt]{standalone}、\\usepackage{luatexja}、\\usepackage{tikz}、必要な\\usetikzlibraryとtikzpictureを含める。LuaLaTeXで処理する。外部ファイルやshell escapeは使わない。コンパイル失敗時は診断を読みsourceを修正して再試行する。",
            "parameters": {
                "type": "object",
                "properties": {"source": {"type": "string", "minLength": 1, "maxLength": 64000}},
                "required": ["source"],
                "additionalProperties": false
            }
        }
    })
}

pub async fn render(runner: &dyn CodeRunner, arguments: &str) -> Result<ExecutionResult> {
    let arguments: TikzArguments = serde_json::from_str(arguments)?;
    ensure!(!arguments.source.trim().is_empty(), "TikZ source is empty");
    ensure!(arguments.source.len() <= 64_000, "TikZ source is too large");
    let result = runner.execute(ExecutionPlan {
        execute: true,
        explanation: "render a TikZ diagram".into(),
        system_packages: vec![],
        files: vec![ExecutionFile { path: "diagram.tex".into(), content: arguments.source }],
        commands: vec![
            "command -v lualatex >/dev/null && command -v pdftoppm >/dev/null || { echo 'TikZ dependencies missing: provision texlive-luatex texlive-pictures texlive-latex-extra texlive-lang-japanese poppler-utils'; exit 1; }".into(),
            "env openin_any=r openout_any=p lualatex --no-shell-escape --interaction=nonstopmode --halt-on-error diagram.tex".into(),
            "pdftoppm -png -singlefile -f 1 -l 1 -scale-to 2400 diagram.pdf diagram".into(),
        ],
        output_files: vec!["diagram.png".into()],
    }).await?;
    if result.exit_code == Some(0) && !result.timed_out {
        ensure!(
            result.artifacts.len() == 1,
            "TikZ produced no diagram image"
        );
        ensure!(
            result.artifacts[0].bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "TikZ output is not PNG"
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::LimaRunner;
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "requires local Lima VM and installs TeX dependencies; no Discord credentials"]
    async fn renders_japanese_tikz_in_lima() {
        let runner = LimaRunner::new(
            "discord-code-runner".into(),
            "src/runner/lima.yaml".into(),
            Duration::from_secs(60),
        )
        .unwrap();
        runner.ensure_ready().await.unwrap();
        let source = r"\documentclass[tikz,border=6pt]{standalone}
\usepackage{luatexja}
\usepackage{tikz}
\begin{document}
\begin{tikzpicture}
\node[draw] (a) at (0,0) {入力};
\node[draw] (b) at (3,0) {出力};
\draw[->] (a) -- (b);
\end{tikzpicture}
\end{document}";
        let result = render(&runner, &json!({"source": source}).to_string())
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0), "{}", result.output);
        assert_eq!(result.artifacts.len(), 1);
        assert!(result.artifacts[0].bytes.len() > 100);
        let failed = render(&runner, &json!({"source": "invalid tex"}).to_string())
            .await
            .unwrap();
        assert_ne!(failed.exit_code, Some(0));
        assert!(failed.artifacts.is_empty());
        for command in [
            "ln -s /etc/hostname diagram.png",
            "printf '\\211PNG\\r\\n\\032\\n' > source.png; ln source.png diagram.png",
            "mkfifo diagram.png",
            "truncate -s 4194305 diagram.png",
        ] {
            let unsafe_file = runner
                .execute(ExecutionPlan {
                    execute: true,
                    explanation: "test artifact isolation".into(),
                    system_packages: vec![],
                    files: vec![],
                    commands: vec![command.into()],
                    output_files: vec!["diagram.png".into()],
                })
                .await
                .unwrap();
            assert_ne!(unsafe_file.exit_code, Some(0), "{command}");
            assert!(unsafe_file.artifacts.is_empty());
        }
    }
}
