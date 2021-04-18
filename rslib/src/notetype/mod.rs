// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

mod cardgen;
mod emptycards;
mod fields;
mod render;
mod schema11;
mod schemachange;
mod stock;
mod templates;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub(crate) use cardgen::{AlreadyGeneratedCardInfo, CardGenContext};
pub use fields::NoteField;
pub(crate) use render::RenderCardOutput;
pub use schema11::{CardTemplateSchema11, NoteFieldSchema11, NotetypeSchema11};
pub use stock::all_stock_notetypes;
pub use templates::CardTemplate;
use unicase::UniCase;

pub use crate::backend_proto::{
    notetype::{
        config::{
            card_requirement::Kind as CardRequirementKind, CardRequirement, Kind as NotetypeKind,
        },
        field::Config as NoteFieldConfig,
        template::Config as CardTemplateConfig,
        Config as NotetypeConfig, Field as NoteFieldProto, Template as CardTemplateProto,
    },
    Notetype as NotetypeProto,
};
use crate::{
    collection::Collection,
    decks::DeckId,
    define_newtype,
    error::{AnkiError, Result, TemplateSaveError},
    notes::Note,
    prelude::*,
    template::{FieldRequirements, ParsedTemplate},
    text::ensure_string_in_nfc,
    timestamp::TimestampSecs,
    types::Usn,
};

define_newtype!(NotetypeId, i64);

pub(crate) const DEFAULT_CSS: &str = include_str!("styling.css");
pub(crate) const DEFAULT_LATEX_HEADER: &str = include_str!("header.tex");
pub(crate) const DEFAULT_LATEX_FOOTER: &str = r"\end{document}";

#[derive(Debug, PartialEq)]
pub struct Notetype {
    pub id: NotetypeId,
    pub name: String,
    pub mtime_secs: TimestampSecs,
    pub usn: Usn,
    pub fields: Vec<NoteField>,
    pub templates: Vec<CardTemplate>,
    pub config: NotetypeConfig,
}

impl Default for Notetype {
    fn default() -> Self {
        Notetype {
            id: NotetypeId(0),
            name: "".into(),
            mtime_secs: TimestampSecs(0),
            usn: Usn(0),
            fields: vec![],
            templates: vec![],
            config: NotetypeConfig {
                css: DEFAULT_CSS.into(),
                latex_pre: DEFAULT_LATEX_HEADER.into(),
                latex_post: DEFAULT_LATEX_FOOTER.into(),
                ..Default::default()
            },
        }
    }
}

impl Notetype {
    pub(crate) fn ensure_names_unique(&mut self) {
        let mut names = HashSet::new();
        for t in &mut self.templates {
            loop {
                let name = UniCase::new(t.name.clone());
                if !names.contains(&name) {
                    names.insert(name);
                    break;
                }
                t.name.push('+');
            }
        }
        names.clear();
        for t in &mut self.fields {
            loop {
                let name = UniCase::new(t.name.clone());
                if !names.contains(&name) {
                    names.insert(name);
                    break;
                }
                t.name.push('+');
            }
        }
    }

    /// Return the template for the given card ordinal. Cloze notetypes
    /// always return the first and only template.
    pub fn get_template(&self, card_ord: u16) -> Result<&CardTemplate> {
        let template = if self.config.kind() == NotetypeKind::Cloze {
            self.templates.get(0)
        } else {
            self.templates.get(card_ord as usize)
        };

        template.ok_or(AnkiError::NotFound)
    }

    pub(crate) fn set_modified(&mut self, usn: Usn) {
        self.mtime_secs = TimestampSecs::now();
        self.usn = usn;
    }

    fn updated_requirements(
        &self,
        parsed: &[(Option<ParsedTemplate>, Option<ParsedTemplate>)],
    ) -> Vec<CardRequirement> {
        let field_map: HashMap<&str, u16> = self
            .fields
            .iter()
            .enumerate()
            .map(|(idx, field)| (field.name.as_str(), idx as u16))
            .collect();
        parsed
            .iter()
            .enumerate()
            .map(|(ord, (qtmpl, _atmpl))| {
                if let Some(tmpl) = qtmpl {
                    let mut req = match tmpl.requirements(&field_map) {
                        FieldRequirements::Any(ords) => CardRequirement {
                            card_ord: ord as u32,
                            kind: CardRequirementKind::Any as i32,
                            field_ords: ords.into_iter().map(|n| n as u32).collect(),
                        },
                        FieldRequirements::All(ords) => CardRequirement {
                            card_ord: ord as u32,
                            kind: CardRequirementKind::All as i32,
                            field_ords: ords.into_iter().map(|n| n as u32).collect(),
                        },
                        FieldRequirements::None => CardRequirement {
                            card_ord: ord as u32,
                            kind: CardRequirementKind::None as i32,
                            field_ords: vec![],
                        },
                    };
                    req.field_ords.sort_unstable();
                    req
                } else {
                    // template parsing failures make card unsatisfiable
                    CardRequirement {
                        card_ord: ord as u32,
                        kind: CardRequirementKind::None as i32,
                        field_ords: vec![],
                    }
                }
            })
            .collect()
    }

    /// Adjust sort index to match repositioned fields.
    fn reposition_sort_idx(&mut self) {
        self.config.sort_field_idx = self
            .fields
            .iter()
            .enumerate()
            .find_map(|(idx, f)| {
                if f.ord == Some(self.config.sort_field_idx) {
                    Some(idx as u32)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                // provided ordinal not on any existing field; cap to bounds
                self.config
                    .sort_field_idx
                    .max(0)
                    .min((self.fields.len() - 1) as u32)
            });
    }

    pub(crate) fn normalize_names(&mut self) {
        ensure_string_in_nfc(&mut self.name);
        for f in &mut self.fields {
            ensure_string_in_nfc(&mut f.name);
        }
        for t in &mut self.templates {
            ensure_string_in_nfc(&mut t.name);
        }
    }

    pub(crate) fn add_field<S: Into<String>>(&mut self, name: S) {
        self.fields.push(NoteField::new(name));
    }

    pub(crate) fn add_template<S1, S2, S3>(&mut self, name: S1, qfmt: S2, afmt: S3)
    where
        S1: Into<String>,
        S2: Into<String>,
        S3: Into<String>,
    {
        self.templates.push(CardTemplate::new(name, qfmt, afmt));
    }

    pub(crate) fn prepare_for_adding(&mut self) -> Result<()> {
        // defaults to 0
        if self.config.target_deck_id == 0 {
            self.config.target_deck_id = 1;
        }
        self.prepare_for_update(None)
    }

    pub(crate) fn prepare_for_update(&mut self, existing: Option<&Notetype>) -> Result<()> {
        if self.fields.is_empty() {
            return Err(AnkiError::invalid_input("1 field required"));
        }
        if self.templates.is_empty() {
            return Err(AnkiError::invalid_input("1 template required"));
        }
        let bad_chars = |c| c == '"';
        if self.name.contains(bad_chars) {
            self.name = self.name.replace(bad_chars, "");
        }
        if self.name.is_empty() {
            return Err(AnkiError::invalid_input("Empty note type name"));
        }
        self.normalize_names();
        self.fix_field_names()?;
        self.fix_template_names()?;
        self.ensure_names_unique();
        self.reposition_sort_idx();

        let parsed_templates = self.parsed_templates();
        let invalid_card_idx = parsed_templates
            .iter()
            .enumerate()
            .find_map(|(idx, (q, a))| {
                if q.is_none() || a.is_none() {
                    Some(idx)
                } else {
                    None
                }
            });
        if let Some(idx) = invalid_card_idx {
            return Err(AnkiError::TemplateSaveError(TemplateSaveError {
                notetype: self.name.clone(),
                ordinal: idx,
            }));
        }
        let reqs = self.updated_requirements(&parsed_templates);

        // handle renamed+deleted fields
        if let Some(existing) = existing {
            let fields = self.renamed_and_removed_fields(existing);
            if !fields.is_empty() {
                self.update_templates_for_renamed_and_removed_fields(fields, parsed_templates);
            }
        }
        self.config.reqs = reqs;

        Ok(())
    }

    fn renamed_and_removed_fields(&self, current: &Notetype) -> HashMap<String, Option<String>> {
        let mut remaining_ords = HashSet::new();
        // gather renames
        let mut map: HashMap<String, Option<String>> = self
            .fields
            .iter()
            .filter_map(|field| {
                if let Some(existing_ord) = field.ord {
                    remaining_ords.insert(existing_ord);
                    if let Some(existing_field) = current.fields.get(existing_ord as usize) {
                        if existing_field.name != field.name {
                            return Some((existing_field.name.clone(), Some(field.name.clone())));
                        }
                    }
                }
                None
            })
            .collect();
        // and add any fields that have been removed
        for (idx, field) in current.fields.iter().enumerate() {
            if !remaining_ords.contains(&(idx as u32)) {
                map.insert(field.name.clone(), None);
            }
        }

        map
    }

    /// Update templates to reflect field deletions and renames.
    /// Any templates that failed to parse will be ignored.
    fn update_templates_for_renamed_and_removed_fields(
        &mut self,
        fields: HashMap<String, Option<String>>,
        parsed: Vec<(Option<ParsedTemplate>, Option<ParsedTemplate>)>,
    ) {
        for (idx, (q, a)) in parsed.into_iter().enumerate() {
            if let Some(q) = q {
                let updated = q.rename_and_remove_fields(&fields);
                self.templates[idx].config.q_format = updated.template_to_string();
            }
            if let Some(a) = a {
                let updated = a.rename_and_remove_fields(&fields);
                self.templates[idx].config.a_format = updated.template_to_string();
            }
        }
    }

    fn parsed_templates(&self) -> Vec<(Option<ParsedTemplate>, Option<ParsedTemplate>)> {
        self.templates
            .iter()
            .map(|t| (t.parsed_question(), t.parsed_answer()))
            .collect()
    }

    pub fn new_note(&self) -> Note {
        Note::new(&self)
    }

    pub fn target_deck_id(&self) -> DeckId {
        DeckId(self.config.target_deck_id)
    }

    fn fix_field_names(&mut self) -> Result<()> {
        self.fields.iter_mut().try_for_each(NoteField::fix_name)
    }

    fn fix_template_names(&mut self) -> Result<()> {
        self.templates
            .iter_mut()
            .try_for_each(CardTemplate::fix_name)
    }

    /// Find the field index of the provided field name.
    pub(crate) fn get_field_ord(&self, field_name: &str) -> Option<usize> {
        let field_name = UniCase::new(field_name);
        self.fields
            .iter()
            .enumerate()
            .filter_map(|(idx, f)| {
                if UniCase::new(&f.name) == field_name {
                    Some(idx)
                } else {
                    None
                }
            })
            .next()
    }

    pub(crate) fn is_cloze(&self) -> bool {
        matches!(self.config.kind(), NotetypeKind::Cloze)
    }
}

impl From<Notetype> for NotetypeProto {
    fn from(nt: Notetype) -> Self {
        NotetypeProto {
            id: nt.id.0,
            name: nt.name,
            mtime_secs: nt.mtime_secs.0 as u32,
            usn: nt.usn.0,
            config: Some(nt.config),
            fields: nt.fields.into_iter().map(Into::into).collect(),
            templates: nt.templates.into_iter().map(Into::into).collect(),
        }
    }
}

impl Collection {
    /// Add a new notetype, and allocate it an ID.
    pub fn add_notetype(&mut self, nt: &mut Notetype) -> Result<()> {
        self.transact_no_undo(|col| {
            let usn = col.usn()?;
            nt.set_modified(usn);
            col.add_notetype_inner(nt, usn)
        })
    }

    pub(crate) fn add_notetype_inner(&mut self, nt: &mut Notetype, usn: Usn) -> Result<()> {
        nt.prepare_for_adding()?;
        self.ensure_notetype_name_unique(nt, usn)?;
        self.storage.add_new_notetype(nt)
    }

    pub(crate) fn ensure_notetype_name_unique(
        &self,
        notetype: &mut Notetype,
        usn: Usn,
    ) -> Result<()> {
        loop {
            match self.storage.get_notetype_id(&notetype.name)? {
                Some(id) if id == notetype.id => {
                    break;
                }
                None => break,
                _ => (),
            }
            notetype.name += "+";
            notetype.set_modified(usn);
        }

        Ok(())
    }

    /// Saves changes to a note type. This will force a full sync if templates
    /// or fields have been added/removed/reordered.
    pub fn update_notetype(&mut self, nt: &mut Notetype, preserve_usn: bool) -> Result<()> {
        let existing = self.get_notetype(nt.id)?;
        let norm = self.get_bool(BoolKey::NormalizeNoteText);
        nt.prepare_for_update(existing.as_ref().map(AsRef::as_ref))?;
        self.transact_no_undo(|col| {
            if let Some(existing_notetype) = existing {
                if existing_notetype.mtime_secs > nt.mtime_secs {
                    return Err(AnkiError::invalid_input("attempt to save stale notetype"));
                }
                col.update_notes_for_changed_fields(
                    nt,
                    existing_notetype.fields.len(),
                    existing_notetype.config.sort_field_idx,
                    norm,
                )?;
                col.update_cards_for_changed_templates(nt, existing_notetype.templates.len())?;
            }

            let usn = col.usn()?;
            if !preserve_usn {
                nt.set_modified(usn);
            }
            col.ensure_notetype_name_unique(nt, usn)?;

            col.storage.update_notetype_config(&nt)?;
            col.storage.update_notetype_fields(nt.id, &nt.fields)?;
            col.storage
                .update_notetype_templates(nt.id, &nt.templates)?;

            // fixme: update cache instead of clearing
            col.state.notetype_cache.remove(&nt.id);

            Ok(())
        })
    }

    pub fn get_notetype_by_name(&mut self, name: &str) -> Result<Option<Arc<Notetype>>> {
        if let Some(ntid) = self.storage.get_notetype_id(name)? {
            self.get_notetype(ntid)
        } else {
            Ok(None)
        }
    }

    pub fn get_notetype(&mut self, ntid: NotetypeId) -> Result<Option<Arc<Notetype>>> {
        if let Some(nt) = self.state.notetype_cache.get(&ntid) {
            return Ok(Some(nt.clone()));
        }
        if let Some(nt) = self.storage.get_notetype(ntid)? {
            let nt = Arc::new(nt);
            self.state.notetype_cache.insert(ntid, nt.clone());
            Ok(Some(nt))
        } else {
            Ok(None)
        }
    }

    pub fn get_all_notetypes(&mut self) -> Result<HashMap<NotetypeId, Arc<Notetype>>> {
        self.storage
            .get_all_notetype_names()?
            .into_iter()
            .map(|(ntid, _)| {
                self.get_notetype(ntid)
                    .transpose()
                    .unwrap()
                    .map(|nt| (ntid, nt))
            })
            .collect()
    }

    pub fn remove_notetype(&mut self, ntid: NotetypeId) -> Result<()> {
        // fixme: currently the storage layer is taking care of removing the notes and cards,
        // but we need to do it in this layer in the future for undo handling
        self.transact_no_undo(|col| {
            col.set_schema_modified()?;
            col.state.notetype_cache.remove(&ntid);
            col.clear_aux_config_for_notetype(ntid)?;
            col.storage.remove_notetype(ntid)?;
            let all = col.storage.get_all_notetype_names()?;
            if all.is_empty() {
                let mut nt = all_stock_notetypes(&col.tr).remove(0);
                col.add_notetype_inner(&mut nt, col.usn()?)?;
                col.set_current_notetype_id(nt.id)
            } else {
                col.set_current_notetype_id(all[0].0)
            }
        })
    }
}
