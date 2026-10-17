use super::*;
use zeron_proto::orchestrator_project::{
    MAX_NOTES, MemoryNote, OrchestratorProject, ProjectUpdate, note_text,
};

impl RegistryDoc {
    /// The project of the orchestrator `root_id`: its row, else an empty one.
    pub fn orchestrator_project(&self, root_id: &str) -> OrchestratorProject {
        self.overlay_row(KIND_ORCHESTRATOR_PROJECTS, root_id)
            .and_then(|row| {
                serde_json::from_value(Value::Object(row.fields.into_iter().collect())).ok()
            })
            .unwrap_or_else(|| OrchestratorProject {
                root_id: root_id.to_owned(),
                ..Default::default()
            })
    }

    /// Apply `update` to the project of `root_id` and return it.
    pub fn update_orchestrator_project(
        &mut self,
        root_id: &str,
        update: ProjectUpdate,
    ) -> Result<OrchestratorProject, DocError> {
        let update = update.validated().map_err(DocError::Schema)?;
        let mut project = self.orchestrator_project(root_id);
        update.apply(&mut project);
        project.root_id = root_id.to_owned();
        project.updated_at = Self::now_ms();
        let Value::Object(set) = serde_json::to_value(&project)? else {
            return Err(DocError::Schema("project is not an object".into()));
        };
        let set: BTreeMap<String, Value> = set.into_iter().collect();
        if !fits_op_budget(&RowOp {
            kind: KIND_ORCHESTRATOR_PROJECTS.into(),
            id: root_id.into(),
            op: OpKind::Upsert,
            set: Some(set.clone()),
            hlc: encode_hlc(0, 0, &self.device_id),
            clocks: None,
        }) {
            return Err(DocError::Schema(
                "the project's settings are too long to save".into(),
            ));
        }
        self.write(KIND_ORCHESTRATOR_PROJECTS, root_id, OpKind::Upsert, set);
        Ok(project)
    }

    /// The memory of `root_id`'s project, oldest first.
    pub fn orchestrator_memory(&self, root_id: &str) -> Vec<MemoryNote> {
        let mut notes: Vec<MemoryNote> = self
            .read_kind::<MemoryNote>(KIND_ORCHESTRATOR_MEMORY)
            .into_iter()
            .filter(|note| note.root_id == root_id)
            .collect();
        notes.sort_by(|a, b| a.id.cmp(&b.id));
        notes
    }

    /// Save a note to `root_id`'s project memory. The same text again is the
    /// note already there; past [`MAX_NOTES`] the oldest go.
    pub fn remember(&mut self, root_id: &str, text: &str) -> Result<MemoryNote, DocError> {
        let text = note_text(text).map_err(DocError::Schema)?;
        let notes = self.orchestrator_memory(root_id);
        if let Some(note) = notes.iter().find(|note| note.text == text) {
            return Ok(note.clone());
        }
        let note = MemoryNote {
            id: self.next_hlc(),
            root_id: root_id.to_owned(),
            text,
            at: Self::now_ms(),
        };
        let Value::Object(set) = serde_json::to_value(&note)? else {
            return Err(DocError::Schema("note is not an object".into()));
        };
        self.write(
            KIND_ORCHESTRATOR_MEMORY,
            &note.id,
            OpKind::Upsert,
            set.into_iter().collect(),
        );
        if notes.len() + 1 > MAX_NOTES {
            let old: Vec<(&str, &str)> = notes[..notes.len() + 1 - MAX_NOTES]
                .iter()
                .map(|note| (KIND_ORCHESTRATOR_MEMORY, note.id.as_str()))
                .collect();
            self.delete_row_ops(&old);
        }
        Ok(note)
    }

    /// Drop a note from `root_id`'s project memory; `false` when it is not
    /// one of its notes.
    pub fn forget(&mut self, root_id: &str, note_id: &str) -> bool {
        let known = self
            .orchestrator_memory(root_id)
            .iter()
            .any(|note| note.id == note_id);
        if known {
            self.delete_row_ops(&[(KIND_ORCHESTRATOR_MEMORY, note_id)]);
        }
        known
    }

    /// Drop a project and its memory with its root.
    pub(super) fn delete_orchestrator_project(&mut self, root_id: &str) {
        let notes = self.orchestrator_memory(root_id);
        let mut keys: Vec<(&str, &str)> = notes
            .iter()
            .map(|note| (KIND_ORCHESTRATOR_MEMORY, note.id.as_str()))
            .collect();
        if self.row_exists(KIND_ORCHESTRATOR_PROJECTS, root_id) {
            keys.push((KIND_ORCHESTRATOR_PROJECTS, root_id));
        }
        self.delete_row_ops(&keys);
    }
}
