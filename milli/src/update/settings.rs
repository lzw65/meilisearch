use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::TryInto;
use std::result::Result as StdResult;
use std::sync::Arc;

use charabia::{Normalize, Tokenizer, TokenizerBuilder};
use deserr::{DeserializeError, Deserr};
use itertools::{EitherOrBoth, Itertools};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;

use super::index_documents::{IndexDocumentsConfig, Transform};
use super::IndexerConfig;
use crate::criterion::Criterion;
use crate::error::UserError;
use crate::index::{DEFAULT_MIN_WORD_LEN_ONE_TYPO, DEFAULT_MIN_WORD_LEN_TWO_TYPOS};
use crate::proximity::ProximityPrecision;
use crate::update::index_documents::IndexDocumentsMethod;
use crate::update::{IndexDocuments, UpdateIndexingStep};
use crate::vector::settings::{check_set, check_unset, EmbedderSource, EmbeddingSettings};
use crate::vector::{Embedder, EmbeddingConfig, EmbeddingConfigs};
use crate::{FieldsIdsMap, Index, OrderBy, Result};

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum Setting<T> {
    Set(T),
    Reset,
    NotSet,
}

impl<T, E> Deserr<E> for Setting<T>
where
    T: Deserr<E>,
    E: DeserializeError,
{
    fn deserialize_from_value<V: deserr::IntoValue>(
        value: deserr::Value<V>,
        location: deserr::ValuePointerRef,
    ) -> std::result::Result<Self, E> {
        match value {
            deserr::Value::Null => Ok(Setting::Reset),
            _ => T::deserialize_from_value(value, location).map(Setting::Set),
        }
    }
}

impl<T> Default for Setting<T> {
    fn default() -> Self {
        Self::NotSet
    }
}

impl<T> Setting<T> {
    pub fn set(self) -> Option<T> {
        match self {
            Self::Set(value) => Some(value),
            _ => None,
        }
    }

    pub const fn as_ref(&self) -> Setting<&T> {
        match *self {
            Self::Set(ref value) => Setting::Set(value),
            Self::Reset => Setting::Reset,
            Self::NotSet => Setting::NotSet,
        }
    }

    pub const fn is_not_set(&self) -> bool {
        matches!(self, Self::NotSet)
    }

    /// If `Self` is `Reset`, then map self to `Set` with the provided `val`.
    pub fn or_reset(self, val: T) -> Self {
        match self {
            Self::Reset => Self::Set(val),
            otherwise => otherwise,
        }
    }

    /// Returns `true` if applying the new setting changed this setting
    pub fn apply(&mut self, new: Self) -> bool
    where
        T: PartialEq + Eq,
    {
        if let Setting::NotSet = new {
            return false;
        }
        if self == &new {
            return false;
        }
        *self = new;
        true
    }
}

impl<T: Serialize> Serialize for Setting<T> {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Set(value) => Some(value),
            // Usually not_set isn't serialized by setting skip_serializing_if field attribute
            Self::NotSet | Self::Reset => None,
        }
        .serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Setting<T> {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer).map(|x| match x {
            Some(x) => Self::Set(x),
            None => Self::Reset, // Reset is forced by sending null value
        })
    }
}

pub struct Settings<'a, 't, 'i> {
    wtxn: &'t mut heed::RwTxn<'i>,
    index: &'i Index,

    indexer_config: &'a IndexerConfig,

    searchable_fields: Setting<Vec<String>>,
    displayed_fields: Setting<Vec<String>>,
    filterable_fields: Setting<HashSet<String>>,
    sortable_fields: Setting<HashSet<String>>,
    criteria: Setting<Vec<Criterion>>,
    stop_words: Setting<BTreeSet<String>>,
    non_separator_tokens: Setting<BTreeSet<String>>,
    separator_tokens: Setting<BTreeSet<String>>,
    dictionary: Setting<BTreeSet<String>>,
    distinct_field: Setting<String>,
    synonyms: Setting<BTreeMap<String, Vec<String>>>,
    primary_key: Setting<String>,
    authorize_typos: Setting<bool>,
    min_word_len_two_typos: Setting<u8>,
    min_word_len_one_typo: Setting<u8>,
    exact_words: Setting<BTreeSet<String>>,
    /// Attributes on which typo tolerance is disabled.
    exact_attributes: Setting<HashSet<String>>,
    max_values_per_facet: Setting<usize>,
    sort_facet_values_by: Setting<HashMap<String, OrderBy>>,
    pagination_max_total_hits: Setting<usize>,
    proximity_precision: Setting<ProximityPrecision>,
    embedder_settings: Setting<BTreeMap<String, Setting<EmbeddingSettings>>>,
}

impl<'a, 't, 'i> Settings<'a, 't, 'i> {
    pub fn new(
        wtxn: &'t mut heed::RwTxn<'i>,
        index: &'i Index,
        indexer_config: &'a IndexerConfig,
    ) -> Settings<'a, 't, 'i> {
        Settings {
            wtxn,
            index,
            searchable_fields: Setting::NotSet,
            displayed_fields: Setting::NotSet,
            filterable_fields: Setting::NotSet,
            sortable_fields: Setting::NotSet,
            criteria: Setting::NotSet,
            stop_words: Setting::NotSet,
            non_separator_tokens: Setting::NotSet,
            separator_tokens: Setting::NotSet,
            dictionary: Setting::NotSet,
            distinct_field: Setting::NotSet,
            synonyms: Setting::NotSet,
            primary_key: Setting::NotSet,
            authorize_typos: Setting::NotSet,
            exact_words: Setting::NotSet,
            min_word_len_two_typos: Setting::NotSet,
            min_word_len_one_typo: Setting::NotSet,
            exact_attributes: Setting::NotSet,
            max_values_per_facet: Setting::NotSet,
            sort_facet_values_by: Setting::NotSet,
            pagination_max_total_hits: Setting::NotSet,
            proximity_precision: Setting::NotSet,
            embedder_settings: Setting::NotSet,
            indexer_config,
        }
    }

    pub fn reset_searchable_fields(&mut self) {
        self.searchable_fields = Setting::Reset;
    }

    pub fn set_searchable_fields(&mut self, names: Vec<String>) {
        self.searchable_fields = Setting::Set(names);
    }

    pub fn reset_displayed_fields(&mut self) {
        self.displayed_fields = Setting::Reset;
    }

    pub fn set_displayed_fields(&mut self, names: Vec<String>) {
        self.displayed_fields = Setting::Set(names);
    }

    pub fn reset_filterable_fields(&mut self) {
        self.filterable_fields = Setting::Reset;
    }

    pub fn set_filterable_fields(&mut self, names: HashSet<String>) {
        self.filterable_fields = Setting::Set(names);
    }

    pub fn set_sortable_fields(&mut self, names: HashSet<String>) {
        self.sortable_fields = Setting::Set(names);
    }

    pub fn reset_sortable_fields(&mut self) {
        self.sortable_fields = Setting::Reset;
    }

    pub fn reset_criteria(&mut self) {
        self.criteria = Setting::Reset;
    }

    pub fn set_criteria(&mut self, criteria: Vec<Criterion>) {
        self.criteria = Setting::Set(criteria);
    }

    pub fn reset_stop_words(&mut self) {
        self.stop_words = Setting::Reset;
    }

    pub fn set_stop_words(&mut self, stop_words: BTreeSet<String>) {
        self.stop_words =
            if stop_words.is_empty() { Setting::Reset } else { Setting::Set(stop_words) }
    }

    pub fn reset_non_separator_tokens(&mut self) {
        self.non_separator_tokens = Setting::Reset;
    }

    pub fn set_non_separator_tokens(&mut self, non_separator_tokens: BTreeSet<String>) {
        self.non_separator_tokens = if non_separator_tokens.is_empty() {
            Setting::Reset
        } else {
            Setting::Set(non_separator_tokens)
        }
    }

    pub fn reset_separator_tokens(&mut self) {
        self.separator_tokens = Setting::Reset;
    }

    pub fn set_separator_tokens(&mut self, separator_tokens: BTreeSet<String>) {
        self.separator_tokens = if separator_tokens.is_empty() {
            Setting::Reset
        } else {
            Setting::Set(separator_tokens)
        }
    }

    pub fn reset_dictionary(&mut self) {
        self.dictionary = Setting::Reset;
    }

    pub fn set_dictionary(&mut self, dictionary: BTreeSet<String>) {
        self.dictionary =
            if dictionary.is_empty() { Setting::Reset } else { Setting::Set(dictionary) }
    }

    pub fn reset_distinct_field(&mut self) {
        self.distinct_field = Setting::Reset;
    }

    pub fn set_distinct_field(&mut self, distinct_field: String) {
        self.distinct_field = Setting::Set(distinct_field);
    }

    pub fn reset_synonyms(&mut self) {
        self.synonyms = Setting::Reset;
    }

    pub fn set_synonyms(&mut self, synonyms: BTreeMap<String, Vec<String>>) {
        self.synonyms = if synonyms.is_empty() { Setting::Reset } else { Setting::Set(synonyms) }
    }

    pub fn reset_primary_key(&mut self) {
        self.primary_key = Setting::Reset;
    }

    pub fn set_primary_key(&mut self, primary_key: String) {
        self.primary_key = Setting::Set(primary_key);
    }

    pub fn set_autorize_typos(&mut self, val: bool) {
        self.authorize_typos = Setting::Set(val);
    }

    pub fn reset_authorize_typos(&mut self) {
        self.authorize_typos = Setting::Reset;
    }

    pub fn set_min_word_len_two_typos(&mut self, val: u8) {
        self.min_word_len_two_typos = Setting::Set(val);
    }

    pub fn reset_min_word_len_two_typos(&mut self) {
        self.min_word_len_two_typos = Setting::Reset;
    }

    pub fn set_min_word_len_one_typo(&mut self, val: u8) {
        self.min_word_len_one_typo = Setting::Set(val);
    }

    pub fn reset_min_word_len_one_typo(&mut self) {
        self.min_word_len_one_typo = Setting::Reset;
    }

    pub fn set_exact_words(&mut self, words: BTreeSet<String>) {
        self.exact_words = Setting::Set(words);
    }

    pub fn reset_exact_words(&mut self) {
        self.exact_words = Setting::Reset;
    }

    pub fn set_exact_attributes(&mut self, attrs: HashSet<String>) {
        self.exact_attributes = Setting::Set(attrs);
    }

    pub fn reset_exact_attributes(&mut self) {
        self.exact_attributes = Setting::Reset;
    }

    pub fn set_max_values_per_facet(&mut self, value: usize) {
        self.max_values_per_facet = Setting::Set(value);
    }

    pub fn reset_max_values_per_facet(&mut self) {
        self.max_values_per_facet = Setting::Reset;
    }

    pub fn set_sort_facet_values_by(&mut self, value: HashMap<String, OrderBy>) {
        self.sort_facet_values_by = Setting::Set(value);
    }

    pub fn reset_sort_facet_values_by(&mut self) {
        self.sort_facet_values_by = Setting::Reset;
    }

    pub fn set_pagination_max_total_hits(&mut self, value: usize) {
        self.pagination_max_total_hits = Setting::Set(value);
    }

    pub fn reset_pagination_max_total_hits(&mut self) {
        self.pagination_max_total_hits = Setting::Reset;
    }

    pub fn set_proximity_precision(&mut self, value: ProximityPrecision) {
        self.proximity_precision = Setting::Set(value);
    }

    pub fn reset_proximity_precision(&mut self) {
        self.proximity_precision = Setting::Reset;
    }

    pub fn set_embedder_settings(&mut self, value: BTreeMap<String, Setting<EmbeddingSettings>>) {
        self.embedder_settings = Setting::Set(value);
    }

    pub fn reset_embedder_settings(&mut self) {
        self.embedder_settings = Setting::Reset;
    }

    fn reindex<FP, FA>(
        &mut self,
        progress_callback: &FP,
        should_abort: &FA,
        old_fields_ids_map: FieldsIdsMap,
    ) -> Result<()>
    where
        FP: Fn(UpdateIndexingStep) + Sync,
        FA: Fn() -> bool + Sync,
    {
        puffin::profile_function!();

        let fields_ids_map = self.index.fields_ids_map(self.wtxn)?;
        // if the settings are set before any document update, we don't need to do anything, and
        // will set the primary key during the first document addition.
        if self.index.number_of_documents(self.wtxn)? == 0 {
            return Ok(());
        }

        let transform = Transform::new(
            self.wtxn,
            self.index,
            self.indexer_config,
            IndexDocumentsMethod::ReplaceDocuments,
            false,
        )?;

        // We clear the databases and remap the documents fields based on the new `FieldsIdsMap`.
        let output = transform.prepare_for_documents_reindexing(
            self.wtxn,
            old_fields_ids_map,
            fields_ids_map,
        )?;

        let embedder_configs = self.index.embedding_configs(self.wtxn)?;
        let embedders = self.embedders(embedder_configs)?;

        // We index the generated `TransformOutput` which must contain
        // all the documents with fields in the newly defined searchable order.
        let indexing_builder = IndexDocuments::new(
            self.wtxn,
            self.index,
            self.indexer_config,
            IndexDocumentsConfig::default(),
            &progress_callback,
            &should_abort,
        )?;

        let indexing_builder = indexing_builder.with_embedders(embedders);
        indexing_builder.execute_raw(output)?;

        Ok(())
    }

    fn embedders(
        &self,
        embedding_configs: Vec<(String, EmbeddingConfig)>,
    ) -> Result<EmbeddingConfigs> {
        let res: Result<_> = embedding_configs
            .into_iter()
            .map(|(name, EmbeddingConfig { embedder_options, prompt })| {
                let prompt = Arc::new(prompt.try_into().map_err(crate::Error::from)?);

                let embedder = Arc::new(
                    Embedder::new(embedder_options.clone())
                        .map_err(crate::vector::Error::from)
                        .map_err(crate::Error::from)?,
                );
                Ok((name, (embedder, prompt)))
            })
            .collect();
        res.map(EmbeddingConfigs::new)
    }

    fn update_displayed(&mut self) -> Result<bool> {
        match self.displayed_fields {
            Setting::Set(ref fields) => {
                // fields are deduplicated, only the first occurrence is taken into account
                let names: Vec<_> = fields.iter().unique().map(String::as_str).collect();
                self.index.put_displayed_fields(self.wtxn, &names)?;
            }
            Setting::Reset => {
                self.index.delete_displayed_fields(self.wtxn)?;
            }
            Setting::NotSet => return Ok(false),
        }
        Ok(true)
    }

    fn update_distinct_field(&mut self) -> Result<bool> {
        match self.distinct_field {
            Setting::Set(ref attr) => {
                self.index.put_distinct_field(self.wtxn, attr)?;
            }
            Setting::Reset => {
                self.index.delete_distinct_field(self.wtxn)?;
            }
            Setting::NotSet => return Ok(false),
        }
        Ok(true)
    }

    /// Updates the index's searchable attributes. This causes the field map to be recomputed to
    /// reflect the order of the searchable attributes.
    fn update_searchable(&mut self) -> Result<bool> {
        match self.searchable_fields {
            Setting::Set(ref fields) => {
                // Check to see if the searchable fields changed before doing anything else
                let old_fields = self.index.searchable_fields(self.wtxn)?;
                let did_change = match old_fields {
                    // If old_fields is Some, let's check to see if the fields actually changed
                    Some(old_fields) => {
                        let new_fields = fields.iter().map(String::as_str).collect::<Vec<_>>();
                        new_fields != old_fields
                    }
                    // If old_fields is None, the fields have changed (because they are being set)
                    None => true,
                };
                if !did_change {
                    return Ok(false);
                }

                // every time the searchable attributes are updated, we need to update the
                // ids for any settings that uses the facets. (distinct_fields, filterable_fields).
                let old_fields_ids_map = self.index.fields_ids_map(self.wtxn)?;

                let mut new_fields_ids_map = FieldsIdsMap::new();
                // fields are deduplicated, only the first occurrence is taken into account
                let names = fields.iter().unique().map(String::as_str).collect::<Vec<_>>();

                // Add all the searchable attributes to the field map, and then add the
                // remaining fields from the old field map to the new one
                for name in names.iter() {
                    new_fields_ids_map.insert(name).ok_or(UserError::AttributeLimitReached)?;
                }

                for (_, name) in old_fields_ids_map.iter() {
                    new_fields_ids_map.insert(name).ok_or(UserError::AttributeLimitReached)?;
                }

                self.index.put_all_searchable_fields_from_fields_ids_map(
                    self.wtxn,
                    &names,
                    &new_fields_ids_map,
                )?;
                self.index.put_fields_ids_map(self.wtxn, &new_fields_ids_map)?;
                Ok(true)
            }
            Setting::Reset => Ok(self.index.delete_all_searchable_fields(self.wtxn)?),
            Setting::NotSet => Ok(false),
        }
    }

    fn update_stop_words(&mut self) -> Result<bool> {
        match self.stop_words {
            Setting::Set(ref stop_words) => {
                let current = self.index.stop_words(self.wtxn)?;

                // Apply an unlossy normalization on stop_words
                let stop_words: BTreeSet<String> = stop_words
                    .iter()
                    .map(|w| w.as_str().normalize(&Default::default()).into_owned())
                    .collect();

                // since we can't compare a BTreeSet with an FST we are going to convert the
                // BTreeSet to an FST and then compare bytes per bytes the two FSTs.
                let fst = fst::Set::from_iter(stop_words.into_iter())?;

                // Does the new FST differ from the previous one?
                if current
                    .map_or(true, |current| current.as_fst().as_bytes() != fst.as_fst().as_bytes())
                {
                    // we want to re-create our FST.
                    self.index.put_stop_words(self.wtxn, &fst)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Setting::Reset => Ok(self.index.delete_stop_words(self.wtxn)?),
            Setting::NotSet => Ok(false),
        }
    }

    fn update_non_separator_tokens(&mut self) -> Result<bool> {
        let changes = match self.non_separator_tokens {
            Setting::Set(ref non_separator_tokens) => {
                let current = self.index.non_separator_tokens(self.wtxn)?;

                // Does the new list differ from the previous one?
                if current.map_or(true, |current| &current != non_separator_tokens) {
                    self.index.put_non_separator_tokens(self.wtxn, non_separator_tokens)?;
                    true
                } else {
                    false
                }
            }
            Setting::Reset => self.index.delete_non_separator_tokens(self.wtxn)?,
            Setting::NotSet => false,
        };

        // the synonyms must be updated if non separator tokens have been updated.
        if changes && self.synonyms == Setting::NotSet {
            self.synonyms = Setting::Set(self.index.user_defined_synonyms(self.wtxn)?);
        }

        Ok(changes)
    }

    fn update_separator_tokens(&mut self) -> Result<bool> {
        let changes = match self.separator_tokens {
            Setting::Set(ref separator_tokens) => {
                let current = self.index.separator_tokens(self.wtxn)?;

                // Does the new list differ from the previous one?
                if current.map_or(true, |current| &current != separator_tokens) {
                    self.index.put_separator_tokens(self.wtxn, separator_tokens)?;
                    true
                } else {
                    false
                }
            }
            Setting::Reset => self.index.delete_separator_tokens(self.wtxn)?,
            Setting::NotSet => false,
        };

        // the synonyms must be updated if separator tokens have been updated.
        if changes && self.synonyms == Setting::NotSet {
            self.synonyms = Setting::Set(self.index.user_defined_synonyms(self.wtxn)?);
        }

        Ok(changes)
    }

    fn update_dictionary(&mut self) -> Result<bool> {
        let changes = match self.dictionary {
            Setting::Set(ref dictionary) => {
                let current = self.index.dictionary(self.wtxn)?;

                // Does the new list differ from the previous one?
                if current.map_or(true, |current| &current != dictionary) {
                    self.index.put_dictionary(self.wtxn, dictionary)?;
                    true
                } else {
                    false
                }
            }
            Setting::Reset => self.index.delete_dictionary(self.wtxn)?,
            Setting::NotSet => false,
        };

        // the synonyms must be updated if dictionary has been updated.
        if changes && self.synonyms == Setting::NotSet {
            self.synonyms = Setting::Set(self.index.user_defined_synonyms(self.wtxn)?);
        }

        Ok(changes)
    }

    fn update_synonyms(&mut self) -> Result<bool> {
        match self.synonyms {
            Setting::Set(ref user_synonyms) => {
                fn normalize(tokenizer: &Tokenizer, text: &str) -> Vec<String> {
                    tokenizer
                        .tokenize(text)
                        .filter_map(|token| {
                            if token.is_word() && !token.lemma().is_empty() {
                                Some(token.lemma().to_string())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                }

                let mut builder = TokenizerBuilder::new();
                let stop_words = self.index.stop_words(self.wtxn)?;
                if let Some(ref stop_words) = stop_words {
                    builder.stop_words(stop_words);
                }

                let separators = self.index.allowed_separators(self.wtxn)?;
                let separators: Option<Vec<_>> =
                    separators.as_ref().map(|x| x.iter().map(String::as_str).collect());
                if let Some(ref separators) = separators {
                    builder.separators(separators);
                }

                let dictionary = self.index.dictionary(self.wtxn)?;
                let dictionary: Option<Vec<_>> =
                    dictionary.as_ref().map(|x| x.iter().map(String::as_str).collect());
                if let Some(ref dictionary) = dictionary {
                    builder.words_dict(dictionary);
                }

                let tokenizer = builder.build();

                let mut new_synonyms = HashMap::new();
                for (word, synonyms) in user_synonyms {
                    // Normalize both the word and associated synonyms.
                    let normalized_word = normalize(&tokenizer, word);
                    let normalized_synonyms: Vec<_> = synonyms
                        .iter()
                        .map(|synonym| normalize(&tokenizer, synonym))
                        .filter(|synonym| !synonym.is_empty())
                        .collect();

                    // Store the normalized synonyms under the normalized word,
                    // merging the possible duplicate words.
                    if !normalized_word.is_empty() && !normalized_synonyms.is_empty() {
                        let entry = new_synonyms.entry(normalized_word).or_insert_with(Vec::new);
                        entry.extend(normalized_synonyms.into_iter());
                    }
                }

                // Make sure that we don't have duplicate synonyms.
                new_synonyms.iter_mut().for_each(|(_, synonyms)| {
                    synonyms.sort_unstable();
                    synonyms.dedup();
                });

                let old_synonyms = self.index.synonyms(self.wtxn)?;

                if new_synonyms != old_synonyms {
                    self.index.put_synonyms(self.wtxn, &new_synonyms, user_synonyms)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Setting::Reset => Ok(self.index.delete_synonyms(self.wtxn)?),
            Setting::NotSet => Ok(false),
        }
    }

    fn update_exact_attributes(&mut self) -> Result<bool> {
        match self.exact_attributes {
            Setting::Set(ref attrs) => {
                let old_attrs = self.index.exact_attributes(self.wtxn)?;
                let old_attrs = old_attrs.into_iter().map(String::from).collect::<HashSet<_>>();

                if attrs != &old_attrs {
                    let attrs = attrs.iter().map(String::as_str).collect::<Vec<_>>();
                    self.index.put_exact_attributes(self.wtxn, &attrs)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Setting::Reset => Ok(self.index.delete_exact_attributes(self.wtxn)?),
            Setting::NotSet => Ok(false),
        }
    }

    fn update_filterable(&mut self) -> Result<()> {
        match self.filterable_fields {
            Setting::Set(ref fields) => {
                let mut new_facets = HashSet::new();
                for name in fields {
                    new_facets.insert(name.clone());
                }
                self.index.put_filterable_fields(self.wtxn, &new_facets)?;
            }
            Setting::Reset => {
                self.index.delete_filterable_fields(self.wtxn)?;
            }
            Setting::NotSet => (),
        }
        Ok(())
    }

    fn update_sortable(&mut self) -> Result<()> {
        match self.sortable_fields {
            Setting::Set(ref fields) => {
                let mut new_fields = HashSet::new();
                for name in fields {
                    new_fields.insert(name.clone());
                }
                self.index.put_sortable_fields(self.wtxn, &new_fields)?;
            }
            Setting::Reset => {
                self.index.delete_sortable_fields(self.wtxn)?;
            }
            Setting::NotSet => (),
        }
        Ok(())
    }

    fn update_criteria(&mut self) -> Result<()> {
        match &self.criteria {
            Setting::Set(criteria) => {
                self.index.put_criteria(self.wtxn, criteria)?;
            }
            Setting::Reset => {
                self.index.delete_criteria(self.wtxn)?;
            }
            Setting::NotSet => (),
        }
        Ok(())
    }

    fn update_primary_key(&mut self) -> Result<()> {
        match self.primary_key {
            Setting::Set(ref primary_key) => {
                if self.index.number_of_documents(self.wtxn)? == 0 {
                    let mut fields_ids_map = self.index.fields_ids_map(self.wtxn)?;
                    fields_ids_map.insert(primary_key).ok_or(UserError::AttributeLimitReached)?;
                    self.index.put_fields_ids_map(self.wtxn, &fields_ids_map)?;
                    self.index.put_primary_key(self.wtxn, primary_key)?;
                    Ok(())
                } else {
                    let curr_primary_key = self.index.primary_key(self.wtxn)?.unwrap().to_string();
                    if primary_key == &curr_primary_key {
                        Ok(())
                    } else {
                        Err(UserError::PrimaryKeyCannotBeChanged(curr_primary_key).into())
                    }
                }
            }
            Setting::Reset => {
                if self.index.number_of_documents(self.wtxn)? == 0 {
                    self.index.delete_primary_key(self.wtxn)?;
                    Ok(())
                } else {
                    let primary_key = self.index.primary_key(self.wtxn)?.unwrap();
                    Err(UserError::PrimaryKeyCannotBeChanged(primary_key.to_string()).into())
                }
            }
            Setting::NotSet => Ok(()),
        }
    }

    fn update_authorize_typos(&mut self) -> Result<()> {
        match self.authorize_typos {
            Setting::Set(flag) => {
                self.index.put_authorize_typos(self.wtxn, flag)?;
                Ok(())
            }
            Setting::Reset => {
                self.index.put_authorize_typos(self.wtxn, true)?;
                Ok(())
            }
            Setting::NotSet => Ok(()),
        }
    }

    fn update_min_typo_word_len(&mut self) -> Result<()> {
        let one = self.min_word_len_one_typo.or_reset(DEFAULT_MIN_WORD_LEN_ONE_TYPO);
        let two = self.min_word_len_two_typos.or_reset(DEFAULT_MIN_WORD_LEN_TWO_TYPOS);
        match (one, two) {
            (Setting::Set(one), Setting::Set(two)) => {
                if one > two {
                    return Err(UserError::InvalidMinTypoWordLenSetting(one, two).into());
                } else {
                    self.index.put_min_word_len_one_typo(self.wtxn, one)?;
                    self.index.put_min_word_len_two_typos(self.wtxn, two)?;
                }
            }
            (Setting::Set(one), _) => {
                let two = self.index.min_word_len_two_typos(self.wtxn)?;
                if one > two {
                    return Err(UserError::InvalidMinTypoWordLenSetting(one, two).into());
                } else {
                    self.index.put_min_word_len_one_typo(self.wtxn, one)?;
                }
            }
            (_, Setting::Set(two)) => {
                let one = self.index.min_word_len_one_typo(self.wtxn)?;
                if one > two {
                    return Err(UserError::InvalidMinTypoWordLenSetting(one, two).into());
                } else {
                    self.index.put_min_word_len_two_typos(self.wtxn, two)?;
                }
            }
            _ => (),
        }

        Ok(())
    }

    fn update_exact_words(&mut self) -> Result<()> {
        match self.exact_words {
            Setting::Set(ref mut words) => {
                fn normalize(tokenizer: &Tokenizer, text: &str) -> String {
                    tokenizer.tokenize(text).map(|token| token.lemma().to_string()).collect()
                }

                let mut builder = TokenizerBuilder::new();
                let stop_words = self.index.stop_words(self.wtxn)?;
                if let Some(ref stop_words) = stop_words {
                    builder.stop_words(stop_words);
                }
                let tokenizer = builder.build();

                let mut words: Vec<_> =
                    words.iter().map(|word| normalize(&tokenizer, word)).collect();

                // normalization could reorder words
                words.sort_unstable();

                let words = fst::Set::from_iter(words.iter())?;
                self.index.put_exact_words(self.wtxn, &words)?;
            }
            Setting::Reset => {
                self.index.put_exact_words(self.wtxn, &fst::Set::default())?;
            }
            Setting::NotSet => (),
        }

        Ok(())
    }

    fn update_max_values_per_facet(&mut self) -> Result<()> {
        match self.max_values_per_facet {
            Setting::Set(max) => {
                self.index.put_max_values_per_facet(self.wtxn, max as u64)?;
            }
            Setting::Reset => {
                self.index.delete_max_values_per_facet(self.wtxn)?;
            }
            Setting::NotSet => (),
        }

        Ok(())
    }

    fn update_sort_facet_values_by(&mut self) -> Result<()> {
        match self.sort_facet_values_by.as_ref() {
            Setting::Set(value) => {
                self.index.put_sort_facet_values_by(self.wtxn, value)?;
            }
            Setting::Reset => {
                self.index.delete_sort_facet_values_by(self.wtxn)?;
            }
            Setting::NotSet => (),
        }

        Ok(())
    }

    fn update_pagination_max_total_hits(&mut self) -> Result<()> {
        match self.pagination_max_total_hits {
            Setting::Set(max) => {
                self.index.put_pagination_max_total_hits(self.wtxn, max as u64)?;
            }
            Setting::Reset => {
                self.index.delete_pagination_max_total_hits(self.wtxn)?;
            }
            Setting::NotSet => (),
        }

        Ok(())
    }

    fn update_proximity_precision(&mut self) -> Result<bool> {
        let changed = match self.proximity_precision {
            Setting::Set(new) => {
                let old = self.index.proximity_precision(self.wtxn)?;
                if old == Some(new) {
                    false
                } else {
                    self.index.put_proximity_precision(self.wtxn, new)?;
                    true
                }
            }
            Setting::Reset => self.index.delete_proximity_precision(self.wtxn)?,
            Setting::NotSet => false,
        };

        Ok(changed)
    }

    fn update_embedding_configs(&mut self) -> Result<bool> {
        let update = match std::mem::take(&mut self.embedder_settings) {
            Setting::Set(configs) => {
                let mut changed = false;
                let old_configs = self.index.embedding_configs(self.wtxn)?;
                let old_configs: BTreeMap<String, Setting<EmbeddingSettings>> =
                    old_configs.into_iter().map(|(k, v)| (k, Setting::Set(v.into()))).collect();

                let mut new_configs = BTreeMap::new();
                for joined in old_configs
                    .into_iter()
                    .merge_join_by(configs.into_iter(), |(left, _), (right, _)| left.cmp(right))
                {
                    match joined {
                        // updated config
                        EitherOrBoth::Both((name, mut old), (_, new)) => {
                            changed |= old.apply(new);
                            let new = validate_embedding_settings(old, &name)?;
                            new_configs.insert(name, new);
                        }
                        // unchanged config
                        EitherOrBoth::Left((name, setting)) => {
                            new_configs.insert(name, setting);
                        }
                        // new config
                        EitherOrBoth::Right((name, mut setting)) => {
                            // apply the default source in case the source was not set so that it gets validated
                            crate::vector::settings::EmbeddingSettings::apply_default_source(
                                &mut setting,
                            );
                            let setting = validate_embedding_settings(setting, &name)?;
                            changed = true;
                            new_configs.insert(name, setting);
                        }
                    }
                }
                let new_configs: Vec<(String, EmbeddingConfig)> = new_configs
                    .into_iter()
                    .filter_map(|(name, setting)| match setting {
                        Setting::Set(value) => Some((name, value.into())),
                        Setting::Reset => None,
                        Setting::NotSet => Some((name, EmbeddingSettings::default().into())),
                    })
                    .collect();

                self.index.embedder_category_id.clear(self.wtxn)?;
                for (index, (embedder_name, _)) in new_configs.iter().enumerate() {
                    self.index.embedder_category_id.put_with_flags(
                        self.wtxn,
                        heed::PutFlags::APPEND,
                        embedder_name,
                        &index
                            .try_into()
                            .map_err(|_| UserError::TooManyEmbedders(new_configs.len()))?,
                    )?;
                }

                if new_configs.is_empty() {
                    self.index.delete_embedding_configs(self.wtxn)?;
                } else {
                    self.index.put_embedding_configs(self.wtxn, new_configs)?;
                }
                changed
            }
            Setting::Reset => {
                self.index.delete_embedding_configs(self.wtxn)?;
                true
            }
            Setting::NotSet => false,
        };
        Ok(update)
    }

    pub fn execute<FP, FA>(mut self, progress_callback: FP, should_abort: FA) -> Result<()>
    where
        FP: Fn(UpdateIndexingStep) + Sync,
        FA: Fn() -> bool + Sync,
    {
        self.index.set_updated_at(self.wtxn, &OffsetDateTime::now_utc())?;

        let old_faceted_fields = self.index.user_defined_faceted_fields(self.wtxn)?;
        let old_fields_ids_map = self.index.fields_ids_map(self.wtxn)?;

        self.update_displayed()?;
        self.update_filterable()?;
        self.update_sortable()?;
        self.update_distinct_field()?;
        self.update_criteria()?;
        self.update_primary_key()?;
        self.update_authorize_typos()?;
        self.update_min_typo_word_len()?;
        self.update_exact_words()?;
        self.update_max_values_per_facet()?;
        self.update_sort_facet_values_by()?;
        self.update_pagination_max_total_hits()?;

        // If there is new faceted fields we indicate that we must reindex as we must
        // index new fields as facets. It means that the distinct attribute,
        // an Asc/Desc criterion or a filtered attribute as be added or removed.
        let new_faceted_fields = self.index.user_defined_faceted_fields(self.wtxn)?;
        let faceted_updated = old_faceted_fields != new_faceted_fields;

        let stop_words_updated = self.update_stop_words()?;
        let non_separator_tokens_updated = self.update_non_separator_tokens()?;
        let separator_tokens_updated = self.update_separator_tokens()?;
        let dictionary_updated = self.update_dictionary()?;
        let synonyms_updated = self.update_synonyms()?;
        let searchable_updated = self.update_searchable()?;
        let exact_attributes_updated = self.update_exact_attributes()?;
        let proximity_precision = self.update_proximity_precision()?;
        // TODO: very rough approximation of the needs for reindexing where any change will result in
        // a full reindexing.
        // What can be done instead:
        // 1. Only change the distance on a distance change
        // 2. Only change the name -> embedder mapping on a name change
        // 3. Keep the old vectors but reattempt indexing on a prompt change: only actually changed prompt will need embedding + storage
        let embedding_configs_updated = self.update_embedding_configs()?;

        if stop_words_updated
            || non_separator_tokens_updated
            || separator_tokens_updated
            || dictionary_updated
            || faceted_updated
            || synonyms_updated
            || searchable_updated
            || exact_attributes_updated
            || proximity_precision
            || embedding_configs_updated
        {
            self.reindex(&progress_callback, &should_abort, old_fields_ids_map)?;
        }

        Ok(())
    }
}

fn validate_prompt(
    name: &str,
    new: Setting<EmbeddingSettings>,
) -> Result<Setting<EmbeddingSettings>> {
    match new {
        Setting::Set(EmbeddingSettings {
            source,
            model,
            revision,
            api_key,
            dimensions,
            document_template: Setting::Set(template),
        }) => {
            // validate
            let template = crate::prompt::Prompt::new(template)
                .map(|prompt| crate::prompt::PromptData::from(prompt).template)
                .map_err(|inner| UserError::InvalidPromptForEmbeddings(name.to_owned(), inner))?;

            Ok(Setting::Set(EmbeddingSettings {
                source,
                model,
                revision,
                api_key,
                dimensions,
                document_template: Setting::Set(template),
            }))
        }
        new => Ok(new),
    }
}

pub fn validate_embedding_settings(
    settings: Setting<EmbeddingSettings>,
    name: &str,
) -> Result<Setting<EmbeddingSettings>> {
    let settings = validate_prompt(name, settings)?;
    let Setting::Set(settings) = settings else { return Ok(settings) };
    let EmbeddingSettings { source, model, revision, api_key, dimensions, document_template } =
        settings;
    let Some(inferred_source) = source.set() else {
        return Ok(Setting::Set(EmbeddingSettings {
            source,
            model,
            revision,
            api_key,
            dimensions,
            document_template,
        }));
    };
    match inferred_source {
        EmbedderSource::OpenAi => {
            check_unset(&revision, "revision", inferred_source, name)?;
            check_unset(&dimensions, "dimensions", inferred_source, name)?;
            if let Setting::Set(model) = &model {
                crate::vector::openai::EmbeddingModel::from_name(model.as_str()).ok_or(
                    crate::error::UserError::InvalidOpenAiModel {
                        embedder_name: name.to_owned(),
                        model: model.clone(),
                    },
                )?;
            }
        }
        EmbedderSource::HuggingFace => {
            check_unset(&api_key, "apiKey", inferred_source, name)?;
            check_unset(&dimensions, "dimensions", inferred_source, name)?;
        }
        EmbedderSource::UserProvided => {
            check_unset(&model, "model", inferred_source, name)?;
            check_unset(&revision, "revision", inferred_source, name)?;
            check_unset(&api_key, "apiKey", inferred_source, name)?;
            check_unset(&document_template, "documentTemplate", inferred_source, name)?;
            check_set(&dimensions, "dimensions", inferred_source, name)?;
        }
    }
    Ok(Setting::Set(EmbeddingSettings {
        source,
        model,
        revision,
        api_key,
        dimensions,
        document_template,
    }))
}

#[cfg(test)]
mod tests {
    use big_s::S;
    use heed::types::Bytes;
    use maplit::{btreemap, btreeset, hashset};

    use super::*;
    use crate::error::Error;
    use crate::index::tests::TempIndex;
    use crate::update::ClearDocuments;
    use crate::{Criterion, Filter, SearchResult};

    #[test]
    fn set_and_reset_searchable_fields() {
        let index = TempIndex::new();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();

        index
            .add_documents_using_wtxn(
                &mut wtxn,
                documents!([
                    { "id": 1, "name": "kevin", "age": 23 },
                    { "id": 2, "name": "kevina", "age": 21},
                    { "id": 3, "name": "benoit", "age": 34 }
                ]),
            )
            .unwrap();

        // We change the searchable fields to be the "name" field only.
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_searchable_fields(vec!["name".into()]);
            })
            .unwrap();

        wtxn.commit().unwrap();

        // Check that the searchable field is correctly set to "name" only.
        let rtxn = index.read_txn().unwrap();
        // When we search for something that is not in
        // the searchable fields it must not return any document.
        let result = index.search(&rtxn).query("23").execute().unwrap();
        assert!(result.documents_ids.is_empty());

        // When we search for something that is in the searchable fields
        // we must find the appropriate document.
        let result = index.search(&rtxn).query(r#""kevin""#).execute().unwrap();
        let documents = index.documents(&rtxn, result.documents_ids).unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].1.get(0), Some(&br#""kevin""#[..]));
        drop(rtxn);

        // We change the searchable fields to be the "name" field only.
        index
            .update_settings(|settings| {
                settings.reset_searchable_fields();
            })
            .unwrap();

        // Check that the searchable field have been reset and documents are found now.
        let rtxn = index.read_txn().unwrap();
        let searchable_fields = index.searchable_fields(&rtxn).unwrap();
        assert_eq!(searchable_fields, None);
        let result = index.search(&rtxn).query("23").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 1);
        let documents = index.documents(&rtxn, result.documents_ids).unwrap();
        assert_eq!(documents[0].1.get(0), Some(&br#""kevin""#[..]));
    }

    #[test]
    fn mixup_searchable_with_displayed_fields() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        let mut wtxn = index.write_txn().unwrap();
        // First we send 3 documents with ids from 1 to 3.
        index
            .add_documents_using_wtxn(
                &mut wtxn,
                documents!([
                    { "name": "kevin", "age": 23},
                    { "name": "kevina", "age": 21 },
                    { "name": "benoit", "age": 34 }
                ]),
            )
            .unwrap();

        // In the same transaction we change the displayed fields to be only the "age".
        // We also change the searchable fields to be the "name" field only.
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_displayed_fields(vec!["age".into()]);
                settings.set_searchable_fields(vec!["name".into()]);
            })
            .unwrap();
        wtxn.commit().unwrap();

        // Check that the displayed fields are correctly set to `None` (default value).
        let rtxn = index.read_txn().unwrap();
        let fields_ids = index.displayed_fields(&rtxn).unwrap();
        assert_eq!(fields_ids.unwrap(), (&["age"][..]));
        drop(rtxn);

        // We change the searchable fields to be the "name" field only.
        index
            .update_settings(|settings| {
                settings.reset_searchable_fields();
            })
            .unwrap();

        // Check that the displayed fields always contains only the "age" field.
        let rtxn = index.read_txn().unwrap();
        let fields_ids = index.displayed_fields(&rtxn).unwrap();
        assert_eq!(fields_ids.unwrap(), &["age"][..]);
    }

    #[test]
    fn default_displayed_fields() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        // First we send 3 documents with ids from 1 to 3.
        index
            .add_documents(documents!([
                { "name": "kevin", "age": 23},
                { "name": "kevina", "age": 21 },
                { "name": "benoit", "age": 34 }
            ]))
            .unwrap();

        // Check that the displayed fields are correctly set to `None` (default value).
        let rtxn = index.read_txn().unwrap();
        let fields_ids = index.displayed_fields(&rtxn).unwrap();
        assert_eq!(fields_ids, None);
    }

    #[test]
    fn set_and_reset_displayed_field() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        let mut wtxn = index.write_txn().unwrap();
        index
            .add_documents_using_wtxn(
                &mut wtxn,
                documents!([
                    { "name": "kevin", "age": 23},
                    { "name": "kevina", "age": 21 },
                    { "name": "benoit", "age": 34 }
                ]),
            )
            .unwrap();
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_displayed_fields(vec!["age".into()]);
            })
            .unwrap();
        wtxn.commit().unwrap();

        // Check that the displayed fields are correctly set to only the "age" field.
        let rtxn = index.read_txn().unwrap();
        let fields_ids = index.displayed_fields(&rtxn).unwrap();
        assert_eq!(fields_ids.unwrap(), &["age"][..]);
        drop(rtxn);

        // We reset the fields ids to become `None`, the default value.
        index
            .update_settings(|settings| {
                settings.reset_displayed_fields();
            })
            .unwrap();

        // Check that the displayed fields are correctly set to `None` (default value).
        let rtxn = index.read_txn().unwrap();
        let fields_ids = index.displayed_fields(&rtxn).unwrap();
        assert_eq!(fields_ids, None);
    }

    #[test]
    fn set_filterable_fields() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        // Set the filterable fields to be the age.
        index
            .update_settings(|settings| {
                settings.set_filterable_fields(hashset! { S("age") });
            })
            .unwrap();

        // Then index some documents.
        index
            .add_documents(documents!([
                { "name": "kevin", "age": 23},
                { "name": "kevina", "age": 21 },
                { "name": "benoit", "age": 34 }
            ]))
            .unwrap();

        // Check that the displayed fields are correctly set.
        let rtxn = index.read_txn().unwrap();
        let fields_ids = index.filterable_fields(&rtxn).unwrap();
        assert_eq!(fields_ids, hashset! { S("age") });
        // Only count the field_id 0 and level 0 facet values.
        // TODO we must support typed CSVs for numbers to be understood.
        let fidmap = index.fields_ids_map(&rtxn).unwrap();
        for document in index.all_documents(&rtxn).unwrap() {
            let document = document.unwrap();
            let json = crate::obkv_to_json(&fidmap.ids().collect::<Vec<_>>(), &fidmap, document.1)
                .unwrap();
            println!("json: {:?}", json);
        }
        let count = index
            .facet_id_f64_docids
            .remap_key_type::<Bytes>()
            // The faceted field id is 1u16
            .prefix_iter(&rtxn, &[0, 1, 0])
            .unwrap()
            .count();
        assert_eq!(count, 3);
        drop(rtxn);

        // Index a little more documents with new and current facets values.
        index
            .add_documents(documents!([
                { "name": "kevin2", "age": 23},
                { "name": "kevina2", "age": 21 },
                { "name": "benoit", "age": 35 }
            ]))
            .unwrap();

        let rtxn = index.read_txn().unwrap();
        // Only count the field_id 0 and level 0 facet values.
        let count = index
            .facet_id_f64_docids
            .remap_key_type::<Bytes>()
            .prefix_iter(&rtxn, &[0, 1, 0])
            .unwrap()
            .count();
        assert_eq!(count, 4);
    }

    #[test]
    fn set_asc_desc_field() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        // Set the filterable fields to be the age.
        index
            .update_settings(|settings| {
                settings.set_displayed_fields(vec![S("name")]);
                settings.set_criteria(vec![Criterion::Asc("age".to_owned())]);
            })
            .unwrap();

        // Then index some documents.
        index
            .add_documents(documents!([
                { "name": "kevin", "age": 23},
                { "name": "kevina", "age": 21 },
                { "name": "benoit", "age": 34 }
            ]))
            .unwrap();

        // Run an empty query just to ensure that the search results are ordered.
        let rtxn = index.read_txn().unwrap();
        let SearchResult { documents_ids, .. } = index.search(&rtxn).execute().unwrap();
        let documents = index.documents(&rtxn, documents_ids).unwrap();

        // Fetch the documents "age" field in the ordre in which the documents appear.
        let age_field_id = index.fields_ids_map(&rtxn).unwrap().id("age").unwrap();
        let iter = documents.into_iter().map(|(_, doc)| {
            let bytes = doc.get(age_field_id).unwrap();
            let string = std::str::from_utf8(bytes).unwrap();
            string.parse::<u32>().unwrap()
        });

        assert_eq!(iter.collect::<Vec<_>>(), vec![21, 23, 34]);
    }

    #[test]
    fn set_distinct_field() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        // Set the filterable fields to be the age.
        index
            .update_settings(|settings| {
                // Don't display the generated `id` field.
                settings.set_displayed_fields(vec![S("name"), S("age")]);
                settings.set_distinct_field(S("age"));
            })
            .unwrap();

        // Then index some documents.
        index
            .add_documents(documents!([
                { "name": "kevin",  "age": 23 },
                { "name": "kevina", "age": 21 },
                { "name": "benoit", "age": 34 },
                { "name": "bernard", "age": 34 },
                { "name": "bertrand", "age": 34 },
                { "name": "bernie", "age": 34 },
                { "name": "ben", "age": 34 }
            ]))
            .unwrap();

        // Run an empty query just to ensure that the search results are ordered.
        let rtxn = index.read_txn().unwrap();
        let SearchResult { documents_ids, .. } = index.search(&rtxn).execute().unwrap();

        // There must be at least one document with a 34 as the age.
        assert_eq!(documents_ids.len(), 3);
    }

    #[test]
    fn set_nested_distinct_field() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        // Set the filterable fields to be the age.
        index
            .update_settings(|settings| {
                // Don't display the generated `id` field.
                settings.set_displayed_fields(vec![S("person")]);
                settings.set_distinct_field(S("person.age"));
            })
            .unwrap();

        // Then index some documents.
        index
            .add_documents(documents!([
                { "person": { "name": "kevin", "age": 23 }},
                { "person": { "name": "kevina", "age": 21 }},
                { "person": { "name": "benoit", "age": 34 }},
                { "person": { "name": "bernard", "age": 34 }},
                { "person": { "name": "bertrand", "age": 34 }},
                { "person": { "name": "bernie", "age": 34 }},
                { "person": { "name": "ben", "age": 34 }}
            ]))
            .unwrap();

        // Run an empty query just to ensure that the search results are ordered.
        let rtxn = index.read_txn().unwrap();
        let SearchResult { documents_ids, .. } = index.search(&rtxn).execute().unwrap();

        // There must be at least one document with a 34 as the age.
        assert_eq!(documents_ids.len(), 3);
    }

    #[test]
    fn default_stop_words() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        // First we send 3 documents with ids from 1 to 3.
        index
            .add_documents(documents!([
                { "name": "kevin", "age": 23},
                { "name": "kevina", "age": 21 },
                { "name": "benoit", "age": 34 }
            ]))
            .unwrap();

        // Ensure there is no stop_words by default
        let rtxn = index.read_txn().unwrap();
        let stop_words = index.stop_words(&rtxn).unwrap();
        assert!(stop_words.is_none());
    }

    #[test]
    fn set_and_reset_stop_words() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        let mut wtxn = index.write_txn().unwrap();
        // First we send 3 documents with ids from 1 to 3.
        index
            .add_documents_using_wtxn(
                &mut wtxn,
                documents!([
                    { "name": "kevin", "age": 23, "maxim": "I love dogs" },
                    { "name": "kevina", "age": 21, "maxim": "Doggos are the best" },
                    { "name": "benoit", "age": 34, "maxim": "The crepes are really good" },
                ]),
            )
            .unwrap();

        // In the same transaction we provide some stop_words
        let set = btreeset! { "i".to_string(), "the".to_string(), "are".to_string() };
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_stop_words(set.clone());
            })
            .unwrap();

        wtxn.commit().unwrap();

        // Ensure stop_words are effectively stored
        let rtxn = index.read_txn().unwrap();
        let stop_words = index.stop_words(&rtxn).unwrap();
        assert!(stop_words.is_some()); // at this point the index should return something

        let stop_words = stop_words.unwrap();
        let expected = fst::Set::from_iter(&set).unwrap();
        assert_eq!(stop_words.as_fst().as_bytes(), expected.as_fst().as_bytes());

        // when we search for something that is a non prefix stop_words it should be ignored
        // thus we should get a placeholder search (all the results = 3)
        let result = index.search(&rtxn).query("the ").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 3);
        let result = index.search(&rtxn).query("i ").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 3);
        let result = index.search(&rtxn).query("are ").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 3);

        let result = index.search(&rtxn).query("dog").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 2); // we have two maxims talking about doggos
        let result = index.search(&rtxn).query("benoît").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 1); // there is one benoit in our data

        // now we'll reset the stop_words and ensure it's None
        index
            .update_settings(|settings| {
                settings.reset_stop_words();
            })
            .unwrap();

        let rtxn = index.read_txn().unwrap();
        let stop_words = index.stop_words(&rtxn).unwrap();
        assert!(stop_words.is_none());

        // now we can search for the stop words
        let result = index.search(&rtxn).query("the").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 2);
        let result = index.search(&rtxn).query("i").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 1);
        let result = index.search(&rtxn).query("are").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 2);

        // the rest of the search is still not impacted
        let result = index.search(&rtxn).query("dog").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 2); // we have two maxims talking about doggos
        let result = index.search(&rtxn).query("benoît").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 1); // there is one benoit in our data
    }

    #[test]
    fn set_and_reset_synonyms() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        let mut wtxn = index.write_txn().unwrap();
        // Send 3 documents with ids from 1 to 3.
        index
            .add_documents_using_wtxn(
                &mut wtxn,
                documents!([
                    { "name": "kevin", "age": 23, "maxim": "I love dogs"},
                    { "name": "kevina", "age": 21, "maxim": "Doggos are the best"},
                    { "name": "benoit", "age": 34, "maxim": "The crepes are really good"},
                ]),
            )
            .unwrap();

        // In the same transaction provide some synonyms
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_synonyms(btreemap! {
                    "blini".to_string() => vec!["crepes".to_string()],
                    "super like".to_string() => vec!["love".to_string()],
                    "puppies".to_string() => vec!["dogs".to_string(), "doggos".to_string()]
                });
            })
            .unwrap();
        wtxn.commit().unwrap();

        // Ensure synonyms are effectively stored
        let rtxn = index.read_txn().unwrap();
        let synonyms = index.synonyms(&rtxn).unwrap();
        assert!(!synonyms.is_empty()); // at this point the index should return something

        // Check that we can use synonyms
        let result = index.search(&rtxn).query("blini").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 1);
        let result = index.search(&rtxn).query("super like").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 1);
        let result = index.search(&rtxn).query("puppies").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 2);

        // Reset the synonyms
        index
            .update_settings(|settings| {
                settings.reset_synonyms();
            })
            .unwrap();

        // Ensure synonyms are reset
        let rtxn = index.read_txn().unwrap();
        let synonyms = index.synonyms(&rtxn).unwrap();
        assert!(synonyms.is_empty());

        // Check that synonyms are no longer work
        let result = index.search(&rtxn).query("blini").execute().unwrap();
        assert!(result.documents_ids.is_empty());
        let result = index.search(&rtxn).query("super like").execute().unwrap();
        assert!(result.documents_ids.is_empty());
        let result = index.search(&rtxn).query("puppies").execute().unwrap();
        assert!(result.documents_ids.is_empty());
    }

    #[test]
    fn thai_synonyms() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        let mut wtxn = index.write_txn().unwrap();
        // Send 3 documents with ids from 1 to 3.
        index
            .add_documents_using_wtxn(
                &mut wtxn,
                documents!([
                    { "name": "ยี่ปุ่น" },
                    { "name": "ญี่ปุ่น" },
                ]),
            )
            .unwrap();

        // In the same transaction provide some synonyms
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_synonyms(btreemap! {
                    "japanese".to_string() => vec![S("ญี่ปุ่น"), S("ยี่ปุ่น")],
                });
            })
            .unwrap();
        wtxn.commit().unwrap();

        // Ensure synonyms are effectively stored
        let rtxn = index.read_txn().unwrap();
        let synonyms = index.synonyms(&rtxn).unwrap();
        assert!(!synonyms.is_empty()); // at this point the index should return something

        // Check that we can use synonyms
        let result = index.search(&rtxn).query("japanese").execute().unwrap();
        assert_eq!(result.documents_ids.len(), 2);
    }

    #[test]
    fn setting_searchable_recomputes_other_settings() {
        let index = TempIndex::new();

        // Set all the settings except searchable
        index
            .update_settings(|settings| {
                settings.set_displayed_fields(vec!["hello".to_string()]);
                settings.set_filterable_fields(hashset! { S("age"), S("toto") });
                settings.set_criteria(vec![Criterion::Asc(S("toto"))]);
            })
            .unwrap();

        // check the output
        let rtxn = index.read_txn().unwrap();
        assert_eq!(&["hello"][..], index.displayed_fields(&rtxn).unwrap().unwrap());
        // since no documents have been pushed the primary key is still unset
        assert!(index.primary_key(&rtxn).unwrap().is_none());
        assert_eq!(vec![Criterion::Asc("toto".to_string())], index.criteria(&rtxn).unwrap());
        drop(rtxn);

        // We set toto and age as searchable to force reordering of the fields
        index
            .update_settings(|settings| {
                settings.set_searchable_fields(vec!["toto".to_string(), "age".to_string()]);
            })
            .unwrap();

        let rtxn = index.read_txn().unwrap();
        assert_eq!(&["hello"][..], index.displayed_fields(&rtxn).unwrap().unwrap());
        assert!(index.primary_key(&rtxn).unwrap().is_none());
        assert_eq!(vec![Criterion::Asc("toto".to_string())], index.criteria(&rtxn).unwrap());
    }

    #[test]
    fn setting_not_filterable_cant_filter() {
        let index = TempIndex::new();

        // Set all the settings except searchable
        index
            .update_settings(|settings| {
                settings.set_displayed_fields(vec!["hello".to_string()]);
                // It is only Asc(toto), there is a facet database but it is denied to filter with toto.
                settings.set_criteria(vec![Criterion::Asc(S("toto"))]);
            })
            .unwrap();

        let rtxn = index.read_txn().unwrap();
        let filter = Filter::from_str("toto = 32").unwrap().unwrap();
        let _ = filter.evaluate(&rtxn, &index).unwrap_err();
    }

    #[test]
    fn setting_primary_key() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        let mut wtxn = index.write_txn().unwrap();
        // Set the primary key settings
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_primary_key(S("mykey"));
            })
            .unwrap();
        assert_eq!(index.primary_key(&wtxn).unwrap(), Some("mykey"));

        // Then index some documents with the "mykey" primary key.
        index
            .add_documents_using_wtxn(
                &mut wtxn,
                documents!([
                    { "mykey": 1, "name": "kevin",  "age": 23 },
                    { "mykey": 2, "name": "kevina", "age": 21 },
                    { "mykey": 3, "name": "benoit", "age": 34 },
                    { "mykey": 4, "name": "bernard", "age": 34 },
                    { "mykey": 5, "name": "bertrand", "age": 34 },
                    { "mykey": 6, "name": "bernie", "age": 34 },
                    { "mykey": 7, "name": "ben", "age": 34 }
                ]),
            )
            .unwrap();
        wtxn.commit().unwrap();

        // Updating settings with the same primary key should do nothing
        let mut wtxn = index.write_txn().unwrap();
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_primary_key(S("mykey"));
            })
            .unwrap();
        assert_eq!(index.primary_key(&wtxn).unwrap(), Some("mykey"));
        wtxn.commit().unwrap();

        // Updating the settings with a different (or no) primary key causes an error
        let mut wtxn = index.write_txn().unwrap();
        let error = index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.reset_primary_key();
            })
            .unwrap_err();
        assert!(matches!(error, Error::UserError(UserError::PrimaryKeyCannotBeChanged(_))));
        wtxn.abort();

        // But if we clear the database...
        let mut wtxn = index.write_txn().unwrap();
        let builder = ClearDocuments::new(&mut wtxn, &index);
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // ...we can change the primary key
        index
            .update_settings(|settings| {
                settings.set_primary_key(S("myid"));
            })
            .unwrap();
    }

    #[test]
    fn setting_impact_relevancy() {
        let mut index = TempIndex::new();
        index.index_documents_config.autogenerate_docids = true;

        // Set the genres setting
        index
            .update_settings(|settings| {
                settings.set_filterable_fields(hashset! { S("genres") });
            })
            .unwrap();

        index.add_documents(documents!([
          {
            "id": 11,
            "title": "Star Wars",
            "overview":
              "Princess Leia is captured and held hostage by the evil Imperial forces in their effort to take over the galactic Empire. Venturesome Luke Skywalker and dashing captain Han Solo team together with the loveable robot duo R2-D2 and C-3PO to rescue the beautiful princess and restore peace and justice in the Empire.",
            "genres": ["Adventure", "Action", "Science Fiction"],
            "poster": "https://image.tmdb.org/t/p/w500/6FfCtAuVAW8XJjZ7eWeLibRLWTw.jpg",
            "release_date": 233366400
          },
          {
            "id": 30,
            "title": "Magnetic Rose",
            "overview": "",
            "genres": ["Animation", "Science Fiction"],
            "poster": "https://image.tmdb.org/t/p/w500/gSuHDeWemA1menrwfMRChnSmMVN.jpg",
            "release_date": 819676800
          }
        ])).unwrap();

        let rtxn = index.read_txn().unwrap();
        let SearchResult { documents_ids, .. } = index.search(&rtxn).query("S").execute().unwrap();
        let first_id = documents_ids[0];
        let documents = index.documents(&rtxn, documents_ids).unwrap();
        let (_, content) = documents.iter().find(|(id, _)| *id == first_id).unwrap();

        let fid = index.fields_ids_map(&rtxn).unwrap().id("title").unwrap();
        let line = std::str::from_utf8(content.get(fid).unwrap()).unwrap();
        assert_eq!(line, r#""Star Wars""#);
    }

    #[test]
    fn test_disable_typo() {
        let index = TempIndex::new();

        let mut txn = index.write_txn().unwrap();
        assert!(index.authorize_typos(&txn).unwrap());

        index
            .update_settings_using_wtxn(&mut txn, |settings| {
                settings.set_autorize_typos(false);
            })
            .unwrap();

        assert!(!index.authorize_typos(&txn).unwrap());
    }

    #[test]
    fn update_min_word_len_for_typo() {
        let index = TempIndex::new();

        // Set the genres setting
        index
            .update_settings(|settings| {
                settings.set_min_word_len_one_typo(8);
                settings.set_min_word_len_two_typos(8);
            })
            .unwrap();

        let txn = index.read_txn().unwrap();
        assert_eq!(index.min_word_len_one_typo(&txn).unwrap(), 8);
        assert_eq!(index.min_word_len_two_typos(&txn).unwrap(), 8);

        index
            .update_settings(|settings| {
                settings.reset_min_word_len_one_typo();
                settings.reset_min_word_len_two_typos();
            })
            .unwrap();

        let txn = index.read_txn().unwrap();
        assert_eq!(index.min_word_len_one_typo(&txn).unwrap(), DEFAULT_MIN_WORD_LEN_ONE_TYPO);
        assert_eq!(index.min_word_len_two_typos(&txn).unwrap(), DEFAULT_MIN_WORD_LEN_TWO_TYPOS);
    }

    #[test]
    fn update_invalid_min_word_len_for_typo() {
        let index = TempIndex::new();

        // Set the genres setting
        index
            .update_settings(|settings| {
                settings.set_min_word_len_one_typo(10);
                settings.set_min_word_len_two_typos(7);
            })
            .unwrap_err();
    }

    #[test]
    fn update_exact_words_normalization() {
        let index = TempIndex::new();

        let mut txn = index.write_txn().unwrap();
        // Set the genres setting
        index
            .update_settings_using_wtxn(&mut txn, |settings| {
                let words = btreeset! { S("Ab"), S("ac") };
                settings.set_exact_words(words);
            })
            .unwrap();

        let exact_words = index.exact_words(&txn).unwrap().unwrap();
        for word in exact_words.into_fst().stream().into_str_vec().unwrap() {
            assert!(word.0 == "ac" || word.0 == "ab");
        }
    }

    #[test]
    fn test_correct_settings_init() {
        let index = TempIndex::new();

        index
            .update_settings(|settings| {
                // we don't actually update the settings, just check their content
                let Settings {
                    wtxn: _,
                    index: _,
                    indexer_config: _,
                    searchable_fields,
                    displayed_fields,
                    filterable_fields,
                    sortable_fields,
                    criteria,
                    stop_words,
                    non_separator_tokens,
                    separator_tokens,
                    dictionary,
                    distinct_field,
                    synonyms,
                    primary_key,
                    authorize_typos,
                    min_word_len_two_typos,
                    min_word_len_one_typo,
                    exact_words,
                    exact_attributes,
                    max_values_per_facet,
                    sort_facet_values_by,
                    pagination_max_total_hits,
                    proximity_precision,
                    embedder_settings,
                } = settings;
                assert!(matches!(searchable_fields, Setting::NotSet));
                assert!(matches!(displayed_fields, Setting::NotSet));
                assert!(matches!(filterable_fields, Setting::NotSet));
                assert!(matches!(sortable_fields, Setting::NotSet));
                assert!(matches!(criteria, Setting::NotSet));
                assert!(matches!(stop_words, Setting::NotSet));
                assert!(matches!(non_separator_tokens, Setting::NotSet));
                assert!(matches!(separator_tokens, Setting::NotSet));
                assert!(matches!(dictionary, Setting::NotSet));
                assert!(matches!(distinct_field, Setting::NotSet));
                assert!(matches!(synonyms, Setting::NotSet));
                assert!(matches!(primary_key, Setting::NotSet));
                assert!(matches!(authorize_typos, Setting::NotSet));
                assert!(matches!(min_word_len_two_typos, Setting::NotSet));
                assert!(matches!(min_word_len_one_typo, Setting::NotSet));
                assert!(matches!(exact_words, Setting::NotSet));
                assert!(matches!(exact_attributes, Setting::NotSet));
                assert!(matches!(max_values_per_facet, Setting::NotSet));
                assert!(matches!(sort_facet_values_by, Setting::NotSet));
                assert!(matches!(pagination_max_total_hits, Setting::NotSet));
                assert!(matches!(proximity_precision, Setting::NotSet));
                assert!(matches!(embedder_settings, Setting::NotSet));
            })
            .unwrap();
    }

    #[test]
    fn settings_must_ignore_soft_deleted() {
        use serde_json::json;

        let index = TempIndex::new();

        let mut docs = vec![];
        for i in 0..10 {
            docs.push(json!({ "id": i, "title": format!("{:x}", i) }));
        }
        index.add_documents(documents! { docs }).unwrap();

        index.delete_documents((0..5).map(|id| id.to_string()).collect());

        let mut wtxn = index.write_txn().unwrap();
        index
            .update_settings_using_wtxn(&mut wtxn, |settings| {
                settings.set_searchable_fields(vec!["id".to_string()]);
            })
            .unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.write_txn().unwrap();
        let docs: StdResult<Vec<_>, _> = index.all_documents(&rtxn).unwrap().collect();
        let docs = docs.unwrap();
        assert_eq!(docs.len(), 5);
    }
}
