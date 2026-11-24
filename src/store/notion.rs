use super::{LocalExperiment, Result, Store};
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) const TITLE: i64 = 1;
pub(crate) const PARENT: i64 = 2;
pub(crate) const ARCHIVED: i64 = 4;
pub(crate) const TIME: i64 = 8;
pub(crate) const ALL: i64 = TITLE | PARENT | ARCHIVED | TIME;

pub(crate) struct Node {
    pub experiment: LocalExperiment,
    pub key: String,
    pub parent_key: Option<String>,
    pub latest_start: Option<i64>,
}

pub(super) fn install(schema: &Connection) -> Result<()> {
    schema.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS notion_bound_projects (project_id TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS notion_node_changes (
            experiment_id TEXT PRIMARY KEY, fields INTEGER NOT NULL,
            revision INTEGER NOT NULL, retry_at INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0);
        CREATE TRIGGER IF NOT EXISTS notion_delete_node AFTER DELETE ON local_experiments
            BEGIN DELETE FROM notion_node_changes WHERE experiment_id = OLD.id; END;
        CREATE TRIGGER IF NOT EXISTS notion_delete_project AFTER DELETE ON local_projects
            BEGIN DELETE FROM notion_bound_projects WHERE project_id = OLD.id; END;
    ",
    )?;
    for (name, event, id, condition, fields) in [
        ("create", "INSERT ON local_experiments", "NEW.id", "1", ALL.to_string()),
        ("edit", "UPDATE OF title, parent_experiment_id, archived ON local_experiments", "NEW.id",
         "NEW.title IS NOT OLD.title OR NEW.parent_experiment_id IS NOT OLD.parent_experiment_id OR NEW.archived IS NOT OLD.archived",
         format!("({TITLE} * (NEW.title IS NOT OLD.title) | {PARENT} * (NEW.parent_experiment_id IS NOT OLD.parent_experiment_id) | {ARCHIVED} * (NEW.archived IS NOT OLD.archived))")),
        ("start", "UPDATE OF started_at ON runs", "NEW.experiment_id", "NEW.started_at IS NOT NULL AND NEW.started_at IS NOT OLD.started_at", TIME.to_string()),
        ("started_run", "INSERT ON runs", "NEW.experiment_id", "NEW.started_at IS NOT NULL", TIME.to_string()),
    ] {
        schema.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS notion_node_{name} AFTER {event}
            WHEN ({condition}) AND EXISTS (SELECT 1 FROM notion_bound_projects WHERE project_id = NEW.project_id)
            BEGIN INSERT INTO notion_node_changes (experiment_id, fields, revision) VALUES ({id}, {fields}, 1)
            ON CONFLICT(experiment_id) DO UPDATE SET fields = fields | excluded.fields, revision = revision + 1, retry_at = 0, attempts = 0; END;"))?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn notion_bound(&self, project: &str) -> Result<bool> {
        if self.conn.is_readonly(rusqlite::DatabaseName::Main)?
            && !self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='notion_bound_projects')",
                [],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Ok(false);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM notion_bound_projects WHERE project_id=?1)",
            [project],
            |r| r.get(0),
        )?)
    }

    pub(crate) fn notion_source_id(&self, id: &str) -> Result<Option<String>> {
        if self.conn.is_readonly(rusqlite::DatabaseName::Main)? && !self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('local_experiments') WHERE name='notion_source_id')", [], |row| row.get::<_,bool>(0))? {
            return Ok(None);
        }
        Ok(self.conn.query_row(
            "SELECT notion_source_id FROM local_experiments WHERE id=?1",
            [id],
            |r| r.get(0),
        )?)
    }

    pub(crate) fn notion_node(&self, experiment: LocalExperiment) -> Result<Node> {
        let key = self
            .notion_source_id(&experiment.id)?
            .unwrap_or_else(|| experiment.id.clone());
        let parent_key = experiment
            .parent_experiment_id
            .as_ref()
            .map(|id| {
                self.notion_source_id(id)
                    .map(|source| source.unwrap_or_else(|| id.clone()))
            })
            .transpose()?;
        let latest_start = self.conn.query_row(
            "SELECT MAX(started_at) FROM runs WHERE experiment_id=?1",
            [&experiment.id],
            |r| r.get(0),
        )?;
        Ok(Node {
            experiment,
            key,
            parent_key,
            latest_start,
        })
    }

    pub(crate) fn notion_pending(&self, id: &str) -> Result<Option<(i64, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT fields,revision FROM notion_node_changes WHERE experiment_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub(crate) fn next_notion_change(&self) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT experiment_id FROM notion_node_changes WHERE retry_at<=?1 ORDER BY retry_at,rowid LIMIT 1",
                               [super::now_ms()], |r| r.get(0)).optional()?)
    }

    pub(crate) fn finish_notion_change(&self, id: &str, revision: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notion_node_changes WHERE experiment_id=?1 AND revision=?2",
            params![id, revision],
        )?;
        Ok(())
    }

    pub(crate) fn defer_notion_change(
        &self,
        id: &str,
        revision: i64,
        retryable: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE notion_node_changes SET attempts=attempts+1, retry_at=CASE WHEN ?3 AND attempts<3
                THEN ?4 + 30000 * (1 << attempts) ELSE 9223372036854775807 END
             WHERE experiment_id=?1 AND revision=?2", params![id, revision, retryable, super::now_ms()])?;
        Ok(())
    }
}
