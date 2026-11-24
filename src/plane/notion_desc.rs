use crate::error::{anyhow, bail, Context, Result};
use crate::local::model::LocalExperiment;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;

#[derive(Deserialize)]
struct Configuration {
    workspace_id: String,
    data_source_id: String,
    title_property: String,
    experiment_id_property: String,
    #[serde(default)]
    create_properties: Map<String, Value>,
    notes_heading: Option<String>,
}

pub(super) struct Backend {
    config: Configuration,
    directory: PathBuf,
}

impl Backend {
    pub(super) fn for_project(project_id: &str) -> Result<Option<Self>> {
        let Ok(id) = Uuid::parse_str(project_id) else {
            return Ok(None);
        };
        let directory = crate::store::data_dir()
            .join("notion-sync")
            .join(id.to_string());
        let path = directory.join("desc.json");
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("Reading {}", path.display())),
        };
        let config: Configuration = serde_json::from_reader(file)
            .with_context(|| format!("Invalid Notion description binding at {}", path.display()))?;
        Uuid::parse_str(&config.workspace_id).context("Invalid Notion workspace_id")?;
        Uuid::parse_str(&config.data_source_id).context("Invalid Notion data_source_id")?;
        if config.title_property.is_empty()
            || config.experiment_id_property.is_empty()
            || config.title_property == config.experiment_id_property
        {
            bail!(
                "Notion title_property and experiment_id_property must name two different columns"
            );
        }
        if let Some(h) = &config.notes_heading {
            if !h.starts_with("## ") || h.trim_end() != h || h.contains(['\n', '\r']) {
                bail!("Notion notes_heading must be a single level-two Markdown heading");
            }
        }
        Ok(Some(Self { config, directory }))
    }

    pub(super) fn lock(&self) -> Result<File> {
        let path = self.directory.join("sync.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("Locking {}", path.display()))?;
        Ok(file)
    }

    async fn api(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let mut command = Command::new("ntn");
        command
            .env_remove("NOTION_API_TOKEN")
            .env("NOTION_WORKSPACE_ID", &self.config.workspace_id)
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
        let output = tokio::time::timeout(Duration::from_secs(180), async {
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

    async fn find_page(&self, experiment_id: &str) -> Result<Option<String>> {
        let path = format!("v1/data_sources/{}/query", self.config.data_source_id);
        let response = self.api("POST", &path, Some(json!({
            "filter": {"property": self.config.experiment_id_property, "rich_text": {"equals": experiment_id}},
            "page_size": 2
        }))).await?;
        let rows = response["results"]
            .as_array()
            .context("Missing Notion query results")?;
        let more = response["has_more"]
            .as_bool()
            .context("Missing Notion query pagination")?;
        if more || rows.len() > 1 {
            bail!("Multiple Notion pages match experiment {experiment_id}");
        }
        rows.first()
            .map(|row| {
                row["id"]
                    .as_str()
                    .map(str::to_owned)
                    .context("Missing Notion page ID")
            })
            .transpose()
    }

    fn document(&self, body: &str) -> Result<String> {
        let body = body.trim_end_matches(['\r', '\n']);
        let Some(heading) = self.config.notes_heading.as_deref() else {
            return Ok(body.to_owned());
        };
        if heading_offsets(body, heading).next().is_some() {
            bail!("The heading {heading:?} is reserved for notes on the Notion page");
        }
        Ok(if body.is_empty() {
            heading.to_owned()
        } else {
            format!("{body}\n\n{heading}")
        })
    }

    fn managed<'a>(&self, markdown: &'a str) -> Result<(&'a str, &'a str)> {
        let Some(heading) = self.config.notes_heading.as_deref() else {
            let body = markdown.trim_end_matches(['\r', '\n']);
            return Ok((body, body));
        };
        let mut positions = heading_offsets(markdown, heading);
        let start = positions
            .next()
            .ok_or_else(|| anyhow!("Missing notes heading {heading:?} on the Notion page"))?;
        if positions.next().is_some() {
            bail!("Multiple notes headings {heading:?} on the Notion page");
        }
        Ok((
            markdown[..start].trim_end_matches(['\r', '\n']),
            &markdown[..start + heading.len()],
        ))
    }

    pub(super) async fn description(
        &self,
        exp: &LocalExperiment,
        set: Option<&str>,
    ) -> Result<String> {
        let replacement = set.map(|body| self.document(body)).transpose()?;
        let page = match self.find_page(&exp.id).await? {
            Some(page) => page,
            None => {
                let markdown = match &replacement {
                    Some(text) => text.clone(),
                    None => self.document(exp.description.as_deref().unwrap_or(""))?,
                };
                let mut props = self.config.create_properties.clone();
                props.insert(
                    self.config.title_property.clone(),
                    json!({"title":[{"text":{"content":exp.title.as_deref().unwrap_or(&exp.id)}}]}),
                );
                props.insert(
                    self.config.experiment_id_property.clone(),
                    json!({"rich_text":[{"text":{"content": exp.id}}]}),
                );
                let row = self
                    .api(
                        "POST",
                        "v1/pages",
                        Some(json!({
                            "parent": {"data_source_id":self.config.data_source_id},
                            "properties":props, "markdown_version":"v2", "markdown":markdown
                        })),
                    )
                    .await?;
                let id = row["id"].as_str().context("Missing new Notion page ID")?;
                if let Some(body) = set {
                    return Ok(body.to_owned());
                }
                id.to_owned()
            }
        };
        let path = format!("v1/pages/{page}/markdown");
        let response = self.api("GET", &path, None).await?;
        if response["truncated"].as_bool() != Some(false)
            || response["unknown_block_ids"]
                .as_array()
                .is_none_or(|ids| !ids.is_empty())
        {
            bail!("Notion returned incomplete Markdown for page {page}");
        }
        let markdown = response["markdown"]
            .as_str()
            .context("Missing Notion page Markdown")?;
        let (body, old) = self.managed(markdown)?;
        if let Some(new) = replacement {
            if new != old {
                let update = if old.is_empty() {
                    json!({"markdown_version":"v2","type":"insert_content","insert_content":{"content":new}})
                } else {
                    json!({"markdown_version":"v2","type":"update_content","update_content":{
                        "content_updates":[{"old_str":old,"new_str":new}]
                    }})
                };
                self.api("PATCH", &path, Some(update)).await?;
            }
        }
        Ok(set.unwrap_or(body).to_owned())
    }
}

fn heading_offsets<'a>(markdown: &'a str, heading: &'a str) -> impl Iterator<Item = usize> + 'a {
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
