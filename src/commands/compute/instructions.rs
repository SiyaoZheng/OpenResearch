use super::{read_input, InstructionsCommand};
use crate::error::{anyhow, bail, Context, Result};
use crate::notion;
use clap::ValueEnum;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

const ARCHIVE: &str = "## ORX archive";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ComputeDocument {
    #[value(name = "CUSTOM.md")]
    Guidance,
    #[value(name = "metal-defaults.json")]
    Metal,
}

impl ComputeDocument {
    fn name(self) -> &'static str {
        match self {
            Self::Guidance => "CUSTOM.md",
            Self::Metal => "metal-defaults.json",
        }
    }

    fn validate(self, content: &str) -> Result<()> {
        if self == Self::Metal
            && !serde_json::from_str::<Value>(content)
                .context("Reading metal-defaults.json")?
                .is_object()
        {
            bail!("metal-defaults.json must contain a JSON object");
        }
        Ok(())
    }
}

struct MemoryPage {
    workspace: String,
    page: String,
    document: ComputeDocument,
}

struct Section {
    selection: String,
    content: String,
}

impl MemoryPage {
    fn url(&self) -> String {
        format!("https://app.notion.com/p/{}", self.page.replace('-', ""))
    }

    fn heading(&self) -> String {
        format!("## {}", self.document.name())
    }

    fn section(&self, response: &Value) -> Result<Section> {
        let md = notion::complete_markdown(response, &self.page)?;
        let heading_at = |heading: &str| -> Result<usize> {
            let mut offsets = notion::heading_offsets(md, heading);
            let at = offsets
                .next()
                .with_context(|| format!("Add '{heading}' to {}", self.url()))?;
            if offsets.next().is_some() {
                bail!("Keep a single '{heading}' on {}", self.url());
            }
            Ok(at)
        };
        let heading = self.heading();
        let start = heading_at(&heading)?;
        let end = heading_at(ARCHIVE)?;
        if end < start {
            bail!("Move '{ARCHIVE}' below '{heading}' on {}", self.url());
        }
        let body = md[start + heading.len()..end].trim_matches(['\r', '\n']);
        let content = match self.document {
            ComputeDocument::Guidance => body,
            ComputeDocument::Metal => {
                let err = || {
                    anyhow!(
                        "Keep one JSON code block between '{heading}' and '{ARCHIVE}' on {}",
                        self.url()
                    )
                };
                let (first, rest) = body.split_once('\n').ok_or_else(err)?;
                let fence = first
                    .trim_end_matches('\r')
                    .strip_suffix("json")
                    .ok_or_else(err)?;
                if fence.len() < 3 || !fence.bytes().all(|b| b == b'`') {
                    return Err(err());
                }
                let (json, closing) = rest.rsplit_once('\n').ok_or_else(err)?;
                if closing.trim_end_matches('\r') != fence {
                    return Err(err());
                }
                self.document
                    .validate(json)
                    .with_context(|| format!("Check the Metal defaults on {}", self.url()))?;
                json
            }
        };
        Ok(Section {
            selection: md[start..end + ARCHIVE.len()].to_string(),
            content: if content.is_empty() {
                String::new()
            } else {
                format!("{content}\n")
            },
        })
    }

    async fn request(&self, method: &str, body: Option<Value>) -> Result<Section> {
        let result = notion::api(
            &self.workspace,
            method,
            &format!("/v1/pages/{}/markdown", self.page),
            body,
            TIMEOUT,
        )
        .await
        .with_context(|| format!("{} ({})", self.document.name(), self.url()))?;
        self.section(&result)
    }

    async fn save(&self, content: &str, expected: &str) -> Result<String> {
        let heading = self.heading();
        let body = content.trim_end_matches(['\r', '\n']);
        let rendered = match self.document {
            ComputeDocument::Guidance => {
                for reserved in [heading.as_str(), ARCHIVE] {
                    if notion::heading_offsets(body, reserved).next().is_some() {
                        bail!("The heading '{reserved}' is reserved on {}", self.url());
                    }
                }
                body.to_owned()
            }
            ComputeDocument::Metal => {
                let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
                let fence = "`".repeat(3.max(longest + 1));
                format!("{fence}json\n{body}\n{fence}")
            }
        };
        let old = self.request("GET", None).await?;
        ensure_revision(&old.content, expected)?;
        if old.content.trim_end_matches(['\r', '\n']) == body {
            return Ok(old.content);
        }
        let new = if rendered.is_empty() {
            format!("{heading}\n\n{ARCHIVE}")
        } else {
            format!("{heading}\n\n{rendered}\n\n{ARCHIVE}")
        };
        let applied = self
            .request(
                "PATCH",
                Some(notion::replace_markdown(&old.selection, &new)),
            )
            .await?;
        let confirmed = self.request("GET", None).await.context(
            "Notion accepted the change. Read the compute document to confirm its saved revision",
        )?;
        if applied.content != confirmed.content {
            bail!(
                "The compute document changed again while saving. Read {} before retrying",
                self.url()
            );
        }
        if self.document == ComputeDocument::Metal
            && serde_json::from_str::<Value>(content)?
                != serde_json::from_str::<Value>(&confirmed.content)?
        {
            bail!(
                "The saved Metal JSON differs from the input. Check {}",
                self.url()
            );
        }
        Ok(confirmed.content)
    }
}

fn revision(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn ensure_revision(current: &str, expected: &str) -> Result<()> {
    if revision(current) != expected {
        bail!("Custom instructions changed. Read them again before saving.");
    }
    Ok(())
}

fn local_content(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("Reading {}", path.display())),
    }
}

async fn lock(path: &Path) -> Result<fs::File> {
    let path = path.with_extension("lock");
    tokio::task::spawn_blocking(move || -> Result<_> {
        fs::create_dir_all(path.parent().context("Missing compute directory")?)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.lock()
            .with_context(|| format!("Locking {}", path.display()))?;
        Ok(file)
    })
    .await?
}

pub(super) async fn run(document: ComputeDocument, command: InstructionsCommand) -> Result<Value> {
    let key = format!("compute/{}", document.name());
    let path = crate::config::config_dir().join(&key);
    let remote = crate::telemetry::notion_preferences::compute_document_page(&key)?.map(
        |(workspace, page)| MemoryPage {
            workspace,
            page,
            document,
        },
    );
    let mut result = json!({"path": path});
    if let Some(page) = &remote {
        result["notionUrl"] = json!(page.url());
    }
    let content = match command {
        InstructionsCommand::Show => match &remote {
            Some(page) => page.request("GET", None).await?.content,
            None => local_content(&path)?,
        },
        InstructionsCommand::Path => {
            let _guard = lock(&path).await?;
            if let Some(page) = &remote {
                let content = page.request("GET", None).await?.content;
                crate::local::git::atomic_write_with_mode(&path, content.as_bytes(), Some(0o600))?;
            } else {
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                match options.open(&path) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
            }
            return Ok(result);
        }
        InstructionsCommand::Set {
            file,
            expected_revision,
        } => {
            let content = read_input(&file)?;
            document.validate(&content)?;
            let _guard = lock(&path).await?;
            match &remote {
                Some(page) => page.save(&content, &expected_revision).await?,
                None => {
                    ensure_revision(&local_content(&path)?, &expected_revision)?;
                    crate::local::git::atomic_write_with_mode(
                        &path,
                        content.as_bytes(),
                        Some(0o600),
                    )?;
                    content
                }
            }
        }
    };
    result["revision"] = json!(revision(&content));
    result["content"] = json!(content);
    Ok(result)
}
