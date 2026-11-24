//! Workspace-bound calls through the user's installed Notion CLI.
use crate::error::{bail, Context, Result};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub(crate) async fn api(
    workspace: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    timeout: Duration,
) -> Result<Value> {
    let mut command = Command::new("ntn");
    if let Some(path) = crate::local::shell_env::search_path() {
        command.env("PATH", path);
    }
    command
        .env_remove("NOTION_API_TOKEN")
        .env("NOTION_WORKSPACE_ID", workspace)
        .args(["api", path, "-X", method])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if body.is_some() {
        command.args(["-d", "@-"]).stdin(Stdio::piped());
    } else if path.ends_with("/markdown") {
        command.arg("markdown_version==v2");
    }
    let output = tokio::time::timeout(timeout, async {
        let mut child = command.spawn().context("Starting Notion CLI (ntn)")?;
        if let Some(data) = body {
            let bytes = serde_json::to_vec(&data)?;
            child
                .stdin
                .take()
                .context("Opening ntn input")?
                .write_all(&bytes)
                .await?;
        }
        child.wait_with_output().await.context("Waiting for ntn")
    })
    .await
    .with_context(|| format!("Notion CLI timed out on {method} {path}"))??;
    if !output.status.success() {
        bail!(
            "Notion CLI {method} {path}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let response: Value =
        serde_json::from_slice(&output.stdout).context("Invalid JSON from ntn")?;
    if response["object"] == "error" {
        bail!("Notion {method} {path}: {}", response["message"]);
    }
    Ok(response)
}

pub(crate) fn complete_markdown<'a>(response: &'a Value, page: &str) -> Result<&'a str> {
    if response["truncated"].as_bool() != Some(false)
        || response["unknown_block_ids"]
            .as_array()
            .is_none_or(|ids| !ids.is_empty())
    {
        bail!("Notion returned incomplete Markdown for page {page}");
    }
    response["markdown"]
        .as_str()
        .context("Missing Notion page Markdown")
}

pub(crate) fn heading_offsets<'a>(
    markdown: &'a str,
    heading: &'a str,
) -> impl Iterator<Item = usize> + 'a {
    let mut offset = 0;
    let mut fence: Option<(u8, usize)> = None;
    markdown.split_inclusive('\n').filter_map(move |line| {
        let start = offset;
        offset += line.len();
        let line = line.trim_end_matches(['\r', '\n']);
        let left = line.trim_start_matches(' ');
        if line.len() - left.len() <= 3 {
            if let Some(mark @ (b'`' | b'~')) = left.bytes().next() {
                let count = left.bytes().take_while(|&ch| ch == mark).count();
                if count >= 3 {
                    match fence {
                        None => fence = Some((mark, count)),
                        Some((ch, n))
                            if ch == mark && n <= count && left[count..].trim().is_empty() =>
                        {
                            fence = None
                        }
                        _ => {}
                    }
                    return None;
                }
            }
        }
        (fence.is_none() && line.trim_end() == heading).then_some(start)
    })
}

pub(crate) fn replace_markdown(old: &str, new: &str) -> Value {
    serde_json::json!({
        "markdown_version": "v2",
        "type": "update_content",
        "update_content": {"content_updates": [{"old_str": old, "new_str": new}]}
    })
}
