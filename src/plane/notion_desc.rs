use crate::error::{anyhow, bail, Context, Result};
use crate::store::{
    notion::{Node, ALL, ARCHIVED, PARENT, TIME, TITLE},
    Store,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;
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
    category_property: Option<String>,
    report_category: Option<String>,
    metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct Metadata {
    archived: String,
    parent: String,
    date: String,
    timestamp: String,
    basis: String,
    run_basis: String,
    creation_basis: String,
    utc_offset_minutes: i32,
}

pub(crate) struct Backend {
    config: Configuration,
    directory: PathBuf,
}

impl Backend {
    pub(crate) fn for_project(data_dir: &Path, project_id: &str) -> Result<Option<Self>> {
        let Ok(id) = Uuid::parse_str(project_id) else {
            return Ok(None);
        };
        let directory = data_dir.join("notion-sync").join(id.to_string());
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
        if let Some(name) = &config.category_property {
            if config
                .create_properties
                .get(name)
                .and_then(|v| v["select"]["name"].as_str())
                .is_none()
            {
                bail!(
                    "Set create_properties.{name}.select.name in {}",
                    path.display()
                );
            }
        }
        if config
            .metadata
            .as_ref()
            .is_some_and(|m| m.utc_offset_minutes.abs_diff(0) > 840)
        {
            bail!(
                "Set utc_offset_minutes between -840 and 840 in {}",
                path.display()
            );
        }
        Ok(Some(Self { config, directory }))
    }

    pub(crate) fn report_instructions(&self) -> Option<String> {
        let category = self.config.report_category.as_deref()?;
        let property = self.config.category_property.as_deref()?;
        Some(format!("Independent experiment reports go directly to Notion. Use `env -u NOTION_API_TOKEN NOTION_WORKSPACE_ID={} ntn` with data source `{}`. Match `{}` by the stable artifact-relative key and require `{property}` = `{category}`. Update the original page and its experiment time following `orx skill reports`. Read the project guide on the baseline branch for its fields.", self.config.workspace_id, self.config.data_source_id, self.config.experiment_id_property))
    }

    pub(super) async fn lock(&self) -> Result<File> {
        let path = self.directory.join("sync.lock");
        tokio::task::spawn_blocking(move || {
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
        })
        .await
        .context("Waiting for the Notion page lock")?
    }

    async fn api(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        crate::notion::api(
            &self.config.workspace_id,
            method,
            path,
            body,
            Duration::from_secs(180),
        )
        .await
    }

    async fn find_page(&self, key: &str) -> Result<Option<Value>> {
        let path = format!("v1/data_sources/{}/query", self.config.data_source_id);
        let response = self.api("POST", &path, Some(json!({
            "filter":{"property":self.config.experiment_id_property,"rich_text":{"equals":key}}, "page_size":2
        }))).await?;
        let rows = response["results"]
            .as_array()
            .context("Missing Notion query results")?;
        let more = response["has_more"]
            .as_bool()
            .context("Missing Notion query pagination")?;
        if more || rows.len() > 1 {
            let pages = rows
                .iter()
                .map(|row| row["id"].as_str().unwrap_or("(missing page ID)"))
                .collect::<Vec<_>>();
            bail!("Multiple pages in Notion data source {} share {}={key}, including {}. Open the pages and keep this source key only on the original experiment page before retrying.",
                self.config.data_source_id, self.config.experiment_id_property, pages.join(", "));
        }
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let id = row["id"].as_str().context("Missing Notion page ID")?;
        let category = self.config.category_property.as_deref().or_else(|| {
            self.config
                .create_properties
                .contains_key("类别")
                .then_some("类别")
        });
        if let Some(property) = category {
            let expected = self.config.create_properties[property]["select"]["name"]
                .as_str()
                .with_context(|| {
                    format!("Missing category value for {property} in the Notion binding")
                })?;
            if row["properties"][property]["select"]["name"].as_str() != Some(expected) {
                bail!("Notion page https://www.notion.so/{} matches {key} with the wrong {property}. Check its source. Correct the source key on an unrelated page or set {property} to {expected} on the original node, then retry.", id.replace('-', ""));
            }
        }
        Ok(Some(row.clone()))
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

    fn managed<'a>(&self, page: &str, markdown: &'a str) -> Result<(&'a str, &'a str)> {
        let Some(heading) = self.config.notes_heading.as_deref() else {
            let body = markdown.trim_end_matches(['\r', '\n']);
            return Ok((body, body));
        };
        let mut positions = heading_offsets(markdown, heading);
        let start = positions
            .next()
            .ok_or_else(|| anyhow!("Open https://www.notion.so/{} and add {heading:?} between the experiment description and team notes, then retry.", page.replace('-', "")))?;
        if positions.next().is_some() {
            bail!("Open https://www.notion.so/{} and keep one {heading:?} at the start of the team notes, then retry.", page.replace('-', ""));
        }
        Ok((
            markdown[..start].trim_end_matches(['\r', '\n']),
            &markdown[..start + heading.len()],
        ))
    }

    async fn properties(
        &self,
        node: &Node,
        current: Option<&Value>,
        fields: i64,
    ) -> Result<Map<String, Value>> {
        let exp = &node.experiment;
        let mut props = Map::new();
        if fields & TITLE != 0 {
            props.insert(
                self.config.title_property.clone(),
                json!({"title":[{"text":{"content":exp.title.as_deref().unwrap_or(&node.key)}}]}),
            );
        }
        if let Some(m) = &self.config.metadata {
            if fields & ARCHIVED != 0 {
                props.insert(m.archived.clone(), json!({"checkbox":exp.archived}));
            }
            if fields & PARENT != 0 {
                let mut relation = Vec::new();
                if let Some(parent) = &node.parent_key {
                    let row = self.find_page(parent).await?.ok_or_else(|| anyhow!(
                        "Parent {parent} has no page in data source {}. Save the parent description with orx exp desc {} --stdin, then retry node {}.",
                        self.config.data_source_id, exp.parent_experiment_id.as_deref().unwrap_or(parent),exp.id))?;
                    relation.push(json!({"id":row["id"]}));
                }
                props.insert(m.parent.clone(), json!({"relation":relation}));
            }
            let latest = node.latest_start.unwrap_or(exp.created_at);
            let newer_remote_run = current.is_some_and(|row| {
                row["properties"][&m.basis]["select"]["name"].as_str() == Some(&m.run_basis)
                    && (node.latest_start.is_none()
                        || row["properties"][&m.timestamp]["number"]
                            .as_f64()
                            .is_some_and(|previous| previous > latest as f64))
            });
            if fields & TIME != 0 && !newer_remote_run {
                let offset = m.utc_offset_minutes;
                let stamp = crate::telemetry::iso8601_utc(
                    latest.div_euclid(60_000) * 60_000 + i64::from(offset) * 60_000,
                );
                let date = format!(
                    "{}{}{:02}:{:02}",
                    stamp.trim_end_matches('Z'),
                    if offset < 0 { "-" } else { "+" },
                    offset.abs() / 60,
                    offset.abs() % 60
                );
                props.insert(m.date.clone(), json!({"date":{"start":date}}));
                props.insert(m.timestamp.clone(), json!({"number":latest}));
                props.insert(m.basis.clone(), json!({"select":{"name":if node.latest_start.is_some() { &m.run_basis } else { &m.creation_basis }}}));
            }
        }
        Ok(props)
    }

    pub(super) async fn read(&self, key: &str, id: &str) -> Result<String> {
        let row = self.find_page(key).await?.ok_or_else(|| anyhow!(
            "No Notion page for experiment {} in data source {}. Save its description with orx exp desc {} --stdin to create it.",
            key,self.config.data_source_id,id))?;
        self.markdown(row["id"].as_str().context("Missing Notion page ID")?, None)
            .await
    }

    pub(super) async fn publish(&self, node: &Node, fields: i64, body: Option<&str>) -> Result<()> {
        let replacement = body.map(|body| self.document(body)).transpose()?;
        if let Some(row) = self.find_page(&node.key).await? {
            let id = row["id"].as_str().context("Missing Notion page ID")?;
            let props = self.properties(node, Some(&row), fields).await?;
            if !props.is_empty() {
                self.api(
                    "PATCH",
                    &format!("v1/pages/{id}"),
                    Some(json!({"properties":props})),
                )
                .await?;
            }
            if replacement.is_some() {
                self.markdown(id, replacement).await?;
            }
        } else {
            let mut props = self.config.create_properties.clone();
            props.extend(self.properties(node, None, ALL).await?);
            props.insert(
                self.config.experiment_id_property.clone(),
                json!({"rich_text":[{"text":{"content":node.key}}]}),
            );
            let markdown = match replacement {
                Some(new) => new,
                None => self.document(node.experiment.description.as_deref().unwrap_or(""))?,
            };
            let created = self
                .api(
                    "POST",
                    "v1/pages",
                    Some(json!({
                        "parent":{"data_source_id":self.config.data_source_id},"properties":props,
                        "markdown_version":"v2","markdown":markdown
                    })),
                )
                .await?;
            created["id"]
                .as_str()
                .context("Missing new Notion page ID")?;
        }
        Ok(())
    }

    async fn markdown(&self, page: &str, replacement: Option<String>) -> Result<String> {
        let path = format!("v1/pages/{page}/markdown");
        let response = self.api("GET", &path, None).await?;
        let markdown = crate::notion::complete_markdown(&response, page)?;
        let (body, old) = self.managed(page, markdown)?;
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
        Ok(body.to_owned())
    }
}

pub(crate) async fn deliver(data_dir: PathBuf, id: &str, created: bool) -> Result<()> {
    let store = Store::open_at(data_dir.clone())?;
    let pending = store.notion_pending(id)?;
    let Some(exp) = store.get_local_experiment(id)? else {
        if let Some((_, revision)) = pending {
            store.finish_notion_change(id, revision)?;
        }
        return Ok(());
    };
    let enrolled = store.notion_bound(&exp.project_id)?;
    if pending.is_none() && (!created || enrolled) {
        return Ok(());
    }
    let backend = Backend::for_project(&data_dir, &exp.project_id);
    let backend = match (backend, pending) {
        (Ok(None), None) => return Ok(()),
        (Ok(Some(backend)), _) => backend,
        (result, Some((_, revision))) => {
            store.defer_notion_change(id, revision, false)?;
            return Err(result.err().unwrap_or_else(|| {
                anyhow!(
                    "Restore the Notion binding for project {} before retrying node {id}.",
                    exp.project_id
                )
            }));
        }
        (Err(error), None) => return Err(error),
    };
    let _lock = backend.lock().await?;
    let pending = store.notion_pending(id)?;
    if pending.is_none() && (!created || enrolled) {
        return Ok(());
    }
    let fields = pending.map_or(ALL, |p| p.0);
    let Some(exp) = store.get_local_experiment(id)? else {
        return Ok(());
    };
    let node = store.notion_node(exp)?;
    let result = backend.publish(&node, fields, None).await;
    if let Some((_, revision)) = pending {
        match &result {
            Ok(()) => store.finish_notion_change(id, revision)?,
            Err(error) => {
                let detail = format!("{error:#}").to_ascii_lowercase();
                let retryable = error
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some()
                    || [
                        "rate_limited",
                        "rate limited",
                        "429",
                        "502",
                        "503",
                        "504",
                        "service_unavailable",
                        "error sending request",
                        "connection refused",
                        "connection reset",
                        "timed out",
                        "has no page in data source",
                    ]
                    .iter()
                    .any(|message| detail.contains(message));
                store.defer_notion_change(id, revision, retryable)?;
            }
        }
    }
    result.with_context(|| format!("Node {id} is saved in ORX. Check the Notion error below. After correcting it, rerun the project binding to deliver pending properties."))
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
