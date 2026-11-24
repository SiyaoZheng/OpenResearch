//! Portable global settings on a user-selected Notion memory page.
use super::Settings;
use crate::error::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

const HEADING: &str = "## ORX CLI";
const READ_TTL: Duration = Duration::from_secs(8);
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
struct Binding {
    workspace_id: String,
    page_id: String,
}

fn binding_path() -> PathBuf {
    crate::config::config_dir().join("notion-preferences.json")
}

pub(crate) fn is_bound() -> bool {
    !matches!(binding_path().try_exists(), Ok(false))
}

fn binding() -> Result<Option<Binding>> {
    let path = binding_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Reading {}", path.display())),
    };
    let mut binding: Binding = serde_json::from_slice(&bytes)
        .with_context(|| format!("Check workspace_id and page_id in {}", path.display()))?;
    binding.workspace_id = Uuid::parse_str(&binding.workspace_id)
        .with_context(|| format!("Invalid workspace_id in {}", path.display()))?
        .to_string();
    binding.page_id = Uuid::parse_str(&binding.page_id)
        .with_context(|| format!("Invalid page_id in {}", path.display()))?
        .to_string();
    Ok(Some(binding))
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Preferences {
    default_backend: Option<String>,
    default_flavor: Option<String>,
    default_ssh_host: Option<String>,
    disabled_lit_sources: Vec<String>,
    dashboard_locale: Option<String>,
    auto_update: Option<bool>,
    update_channel: Option<Value>,
}

impl Preferences {
    fn from_settings(s: &Settings) -> Self {
        Self {
            default_backend: s.default_backend.clone(),
            default_flavor: s.default_flavor.clone(),
            default_ssh_host: s.ssh.default_host.clone(),
            disabled_lit_sources: s.disabled_lit_sources.clone(),
            dashboard_locale: s.dashboard_locale.clone(),
            auto_update: s.auto_update,
            update_channel: s.update_channel.clone(),
        }
    }

    fn apply(self, s: &mut Settings) {
        s.default_backend = self.default_backend;
        s.default_flavor = self.default_flavor;
        s.ssh.default_host = self.default_ssh_host;
        s.disabled_lit_sources = self.disabled_lit_sources;
        s.dashboard_locale = self.dashboard_locale;
        s.auto_update = self.auto_update;
        s.update_channel = self.update_channel;
    }

    fn validate(&self) -> Result<()> {
        if let Some(backend) = &self.default_backend {
            crate::local::validate_compute_default(backend, self.default_flavor.as_deref())?;
        } else if self.default_flavor.is_some() {
            bail!("Set defaultBackend before defaultFlavor");
        }
        if self.default_ssh_host.as_deref().is_some_and(|host| {
            host.is_empty()
                || host.starts_with('-')
                || host.chars().any(|c| c.is_whitespace() || c.is_control())
        }) {
            bail!("Use a configured SSH host alias for defaultSshHost");
        }
        if self
            .dashboard_locale
            .as_deref()
            .is_some_and(|locale| !super::valid_dashboard_locale(locale))
        {
            bail!("Use a dashboard language code for dashboardLocale");
        }
        if self
            .update_channel
            .as_ref()
            .is_some_and(|channel| !channel.is_string())
        {
            bail!("Use a release channel name or null for updateChannel");
        }
        Ok(())
    }
}

struct Block<'a> {
    original: &'a str,
    json: &'a str,
}

impl<'a> Block<'a> {
    fn parse(markdown: &'a str) -> Result<Self> {
        // Ignore headings quoted inside other code fences on the page.
        let mut heading = None;
        let mut fence = None;
        let mut offset = 0;
        for line in markdown.split_inclusive('\n') {
            let text = line.trim_end_matches(['\r', '\n']);
            let ticks = text.bytes().take_while(|b| *b == b'`').count();
            if let Some(width) = fence {
                if ticks >= width && text[ticks..].trim().is_empty() {
                    fence = None;
                }
            } else if ticks >= 3 {
                fence = Some(ticks);
            } else if text == HEADING && heading.replace(offset).is_some() {
                bail!("Keep a single '{HEADING}' heading on the Notion preference page");
            }
            offset += line.len();
        }
        let start =
            heading.with_context(|| format!("Add '{HEADING}' to the Notion preference page"))?;
        let section = &markdown[start..];
        let opening = section[HEADING.len()..].trim_start_matches(['\r', '\n']);
        let (language, body) = opening
            .split_once('\n')
            .context("Missing Notion preference JSON block")?;
        let opening_ticks = language.bytes().take_while(|b| *b == b'`').count();
        if opening_ticks < 3 || language[opening_ticks..].trim_end_matches('\r') != "json" {
            bail!("Put a JSON code block immediately below '{HEADING}'");
        }
        let mut end = 0;
        for line in body.split_inclusive('\n') {
            let closing = line.trim_end_matches(['\r', '\n']);
            if closing.len() >= opening_ticks && closing.bytes().all(|b| b == b'`') {
                let original_end = section.len() - body.len() + end + closing.len();
                return Ok(Self {
                    original: &section[..original_end],
                    json: &body[..end],
                });
            }
            end += line.len();
        }
        bail!("Close the JSON code block under '{HEADING}'")
    }

    fn preferences(&self) -> Result<Preferences> {
        if !self.json.trim_start().starts_with('{') {
            bail!("Use a JSON object under '{HEADING}'");
        }
        let prefs: Preferences =
            serde_json::from_str(self.json).context("Reading ORX preference JSON")?;
        prefs.validate()?;
        Ok(prefs)
    }
}

struct Cached {
    path: PathBuf,
    at: Instant,
    prefs: Preferences,
}

static RECENT: Mutex<Option<Cached>> = Mutex::new(None);

impl Binding {
    fn cache_path(&self) -> PathBuf {
        crate::config::config_dir().join(format!(
            "notion-preferences.{}.{}.cache.json",
            self.workspace_id, self.page_id
        ))
    }

    fn remember(&self, prefs: &Preferences) -> Result<()> {
        let path = self.cache_path();
        let bytes = serde_json::to_vec(prefs)?;
        if fs::read(&path).ok().as_deref() != Some(&bytes) {
            crate::local::git::atomic_write_with_mode(&path, &bytes, Some(0o600))?;
        }
        Ok(())
    }

    fn cached(&self) -> Result<Preferences> {
        let path = self.cache_path();
        let bytes = fs::read(&path).with_context(|| format!("Reading {}", path.display()))?;
        let prefs: Preferences =
            serde_json::from_slice(&bytes).context("Reading the offline preference cache")?;
        prefs.validate()?;
        Ok(prefs)
    }

    fn api(&self, method: &str, body: Option<Value>) -> Result<Value> {
        // Settings are also read by synchronous callers already inside a Tokio runtime.
        std::thread::scope(|scope| {
            scope
                .spawn(|| -> Result<Value> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(crate::notion::api(
                        &self.workspace_id,
                        method,
                        &format!("/v1/pages/{}/markdown", self.page_id),
                        body,
                        TIMEOUT,
                    ))
                })
                .join()
                .map_err(|_| crate::error::anyhow!("Notion preference request interrupted"))?
        })
    }

    fn block<'a>(&self, response: &'a Value) -> Result<Block<'a>> {
        Block::parse(crate::notion::complete_markdown(response, &self.page_id)?)
            .with_context(|| format!("Check the ORX CLI section on Notion page {}", self.page_id))
    }

    fn lock(&self) -> Result<fs::File> {
        let path = crate::config::config_dir().join("notion-preferences.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.lock()
            .with_context(|| format!("Locking {}", path.display()))?;
        Ok(file)
    }

    fn record_read(&self, recent: &mut Option<Cached>, prefs: Preferences) {
        if let Err(error) = self.remember(&prefs) {
            eprintln!(
                "Read Notion page {}, but could not save the offline preference cache: {error:#}",
                self.page_id
            );
        }
        *recent = Some(Cached {
            path: self.cache_path(),
            at: Instant::now(),
            prefs,
        });
    }

    fn effective(&self, local: &Settings) -> Preferences {
        let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.cache_path();
        if let Some(cache) = recent
            .as_ref()
            .filter(|c| c.path == path && c.at.elapsed() < READ_TTL)
        {
            return cache.prefs.clone();
        }
        let refreshed = (|| -> Result<Preferences> {
            let _lock = self.lock()?;
            let response = self.api("GET", None)?;
            let prefs = self.block(&response)?.preferences()?;
            self.record_read(&mut recent, prefs.clone());
            Ok(prefs)
        })();
        match refreshed {
            Ok(prefs) => prefs,
            Err(error) => {
                let prefs = match self.cached() {
                    Ok(prefs) => {
                        eprintln!(
                            "Using the last verified preferences from Notion page {}: {error:#}",
                            self.page_id
                        );
                        prefs
                    }
                    Err(cache_error) => {
                        eprintln!("Using this installation's local defaults. Notion page {}: {error:#}. Offline cache: {cache_error:#}", self.page_id);
                        Preferences::from_settings(local)
                    }
                };
                *recent = Some(Cached {
                    path,
                    at: Instant::now(),
                    prefs: prefs.clone(),
                });
                prefs
            }
        }
    }

    fn update(&self, edit: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        let _lock = self.lock()?;
        *recent = None;
        let response = self.api("GET", None)?;
        let block = self.block(&response)?;
        let before = block.preferences()?;
        let mut selected = Settings::default();
        before.clone().apply(&mut selected);
        edit(&mut selected);
        let after = Preferences::from_settings(&selected);
        after.validate()?;
        if before == after {
            self.record_read(&mut recent, before);
            return Ok(());
        }
        // Edit only the changed keys, preserving future fields and surrounding notes.
        let mut raw: Map<String, Value> = serde_json::from_str(block.json)?;
        let old_fields = serde_json::to_value(&before)?;
        let new_fields = serde_json::to_value(&after)?;
        for (key, value) in new_fields.as_object().context("Encoding ORX preferences")? {
            if old_fields.get(key) != Some(value) {
                raw.insert(key.clone(), value.clone());
            }
        }
        let json = serde_json::to_string_pretty(&raw)?;
        let longest_ticks = json.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(3.max(longest_ticks + 1));
        let replacement = format!("{HEADING}\n\n{fence}json\n{json}\n{fence}");
        self.api("PATCH", Some(json!({
            "type": "update_content", "markdown_version": "v2",
            "update_content": { "content_updates": [{ "old_str": block.original, "new_str": replacement }] }
        })))?;
        let response = self.api("GET", None).context("Notion accepted the preference change but confirmation failed. Read the page before retrying")?;
        let confirmed = self.block(&response)?.preferences()?;
        let confirmed_fields = serde_json::to_value(&confirmed)?;
        let matches = new_fields
            .as_object()
            .context("Encoding ORX preferences")?
            .iter()
            .all(|(key, value)| {
                old_fields.get(key) == Some(value) || confirmed_fields.get(key) == Some(value)
            });
        self.record_read(&mut recent, confirmed);
        if !matches {
            bail!(
                "Notion page {} changed during the save. Read the current preferences and retry",
                self.page_id
            );
        }
        Ok(())
    }
}

pub(super) fn last_read_locale(local: Option<&Settings>) -> Option<String> {
    // Analytics must never wait for a network request.
    if let Some(cached) = binding().ok().flatten().and_then(|page| page.cached().ok()) {
        return cached.dashboard_locale;
    }
    local.and_then(|s| s.dashboard_locale.clone())
}

pub(super) fn effective(mut settings: Settings) -> Settings {
    match binding() {
        Ok(Some(page)) => page.effective(&settings).apply(&mut settings),
        Ok(None) => {}
        Err(error) => eprintln!("Using local ORX preference defaults: {error:#}"),
    }
    settings
}

pub(super) fn load() -> Settings {
    effective(super::load_settings().unwrap_or_default())
}

pub(super) fn mutate(edit: impl FnOnce(&mut Settings)) -> io::Result<()> {
    match binding().map_err(io::Error::other)? {
        None => super::mutate_settings(edit),
        Some(page) => page.update(edit).map_err(|error| {
            io::Error::other(format!(
                "Saving preferences to Notion page {}: {error:#}",
                page.page_id
            ))
        }),
    }
}
