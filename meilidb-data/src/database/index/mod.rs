use std::collections::{HashSet, BTreeMap};
use std::convert::TryInto;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption, Guard};
use meilidb_core::criterion::Criteria;
use meilidb_core::{DocIndex, Store, DocumentId, QueryBuilder};
use meilidb_schema::Schema;
use sdset::SetBuf;
use serde::{de, Serialize, Deserialize};
use sled::Transactional;

use crate::ranked_map::RankedMap;
use crate::serde::{Deserializer, DeserializerError};

pub use self::custom_settings_index::CustomSettingsIndex;
use self::docs_words_index::DocsWordsIndex;
use self::documents_index::DocumentsIndex;
use self::main_index::MainIndex;
use self::synonyms_index::SynonymsIndex;
use self::words_index::WordsIndex;

use crate::database::{
    Error,
    DocumentsAddition, DocumentsDeletion,
    SynonymsAddition, SynonymsDeletion,
    apply_documents_addition, apply_documents_deletion,
    apply_synonyms_addition, apply_synonyms_deletion,
};

mod custom_settings_index;
mod docs_words_index;
mod documents_index;
mod main_index;
mod synonyms_index;
mod words_index;

fn event_is_set(event: &sled::Event) -> bool {
    match event {
        sled::Event::Set(_, _) => true,
        _ => false,
    }
}

#[derive(Deserialize)]
enum UpdateOwned {
    DocumentsAddition(Vec<rmpv::Value>),
    DocumentsDeletion(Vec<DocumentId>),
    SynonymsAddition(BTreeMap<String, Vec<String>>),
    SynonymsDeletion(BTreeMap<String, Option<Vec<String>>>),
}

#[derive(Serialize)]
enum Update {
    DocumentsAddition(Vec<rmpv::Value>),
    DocumentsDeletion(Vec<DocumentId>),
    SynonymsAddition(BTreeMap<String, Vec<String>>),
    SynonymsDeletion(BTreeMap<String, Option<Vec<String>>>),
}

#[derive(Clone, Serialize, Deserialize)]
pub enum UpdateType {
    DocumentsAddition { number: usize },
    DocumentsDeletion { number: usize },
    SynonymsAddition { number: usize },
    SynonymsDeletion { number: usize },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DetailedDuration {
    main: Duration,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub update_id: u64,
    pub update_type: UpdateType,
    pub result: Result<(), String>,
    pub detailed_duration: DetailedDuration,
}

fn spawn_update_system(index: Index) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        loop {
            let subscription = index.updates_index.watch_prefix(vec![]);
            while let Some(result) = index.updates_index.iter().next() {
                let (key, _) = result.unwrap();
                let update_id = key.as_ref().try_into().map(u64::from_be_bytes).unwrap();

                let updates = &index.updates_index;
                let results = &index.updates_results_index;

                (updates, results).transaction(|(updates, results)| {
                    let update = updates.remove(&key)?.unwrap();

                    let (update_type, result, duration) = match rmp_serde::from_read_ref(&update).unwrap() {
                        UpdateOwned::DocumentsAddition(documents) => {
                            let update_type = UpdateType::DocumentsAddition { number: documents.len() };
                            let ranked_map = index.cache.load().ranked_map.clone();
                            let start = Instant::now();
                            let result = apply_documents_addition(&index, ranked_map, documents);
                            (update_type, result, start.elapsed())
                        },
                        UpdateOwned::DocumentsDeletion(documents) => {
                            let update_type = UpdateType::DocumentsDeletion { number: documents.len() };
                            let ranked_map = index.cache.load().ranked_map.clone();
                            let start = Instant::now();
                            let result = apply_documents_deletion(&index, ranked_map, documents);
                            (update_type, result, start.elapsed())
                        },
                        UpdateOwned::SynonymsAddition(synonyms) => {
                            let update_type = UpdateType::SynonymsAddition { number: synonyms.len() };
                            let start = Instant::now();
                            let result = apply_synonyms_addition(&index, synonyms);
                            (update_type, result, start.elapsed())
                        },
                        UpdateOwned::SynonymsDeletion(synonyms) => {
                            let update_type = UpdateType::SynonymsDeletion { number: synonyms.len() };
                            let start = Instant::now();
                            let result = apply_synonyms_deletion(&index, synonyms);
                            (update_type, result, start.elapsed())
                        },
                    };

                    let detailed_duration = DetailedDuration { main: duration };
                    let status = UpdateStatus {
                        update_id,
                        update_type,
                        result: result.map_err(|e| e.to_string()),
                        detailed_duration,
                    };

                    if let Some(callback) = &*index.update_callback.load() {
                        (callback)(status.clone());
                    }

                    let value = bincode::serialize(&status).unwrap();
                    results.insert(&key, value)
                })
                .unwrap();
            }

            // this subscription is just used to block
            // the loop until a new update is inserted
            subscription.filter(event_is_set).next();
        }
    })
}

#[derive(Copy, Clone)]
pub struct IndexStats {
    pub number_of_words: usize,
    pub number_of_documents: usize,
    pub number_attrs_in_ranked_map: usize,
}

#[derive(Clone)]
pub struct Index {
    pub(crate) cache: Arc<ArcSwap<Cache>>,

    // TODO this will be a snapshot in the future
    main_index: MainIndex,
    synonyms_index: SynonymsIndex,
    words_index: WordsIndex,
    docs_words_index: DocsWordsIndex,
    documents_index: DocumentsIndex,
    custom_settings_index: CustomSettingsIndex,

    // used by the update system
    db: sled::Db,
    updates_index: Arc<sled::Tree>,
    updates_results_index: Arc<sled::Tree>,
    update_callback: Arc<ArcSwapOption<Box<dyn Fn(UpdateStatus) + Send + Sync + 'static>>>,
}

pub(crate) struct Cache {
    pub words: Arc<fst::Set>,
    pub synonyms: Arc<fst::Set>,
    pub schema: Schema,
    pub ranked_map: RankedMap,
}

impl Index {
    pub fn new(db: sled::Db, name: &str) -> Result<Index, Error> {
        Index::new_raw(db, name, None)
    }

    pub fn with_schema(db: sled::Db, name: &str, schema: Schema) -> Result<Index, Error> {
        Index::new_raw(db, name, Some(schema))
    }

    fn new_raw(db: sled::Db, name: &str, schema: Option<Schema>) -> Result<Index, Error> {
        let main_index = db.open_tree(name).map(MainIndex)?;
        let synonyms_index = db.open_tree(format!("{}-synonyms", name)).map(SynonymsIndex)?;
        let words_index = db.open_tree(format!("{}-words", name)).map(WordsIndex)?;
        let docs_words_index = db.open_tree(format!("{}-docs-words", name)).map(DocsWordsIndex)?;
        let documents_index = db.open_tree(format!("{}-documents", name)).map(DocumentsIndex)?;
        let custom_settings_index = db.open_tree(format!("{}-custom", name)).map(CustomSettingsIndex)?;
        let updates_index = db.open_tree(format!("{}-updates", name))?;
        let updates_results_index = db.open_tree(format!("{}-updates-results", name))?;

        let words = match main_index.words_set()? {
            Some(words) => Arc::new(words),
            None => Arc::new(fst::Set::default()),
        };

        let synonyms = match main_index.synonyms_set()? {
            Some(synonyms) => Arc::new(synonyms),
            None => Arc::new(fst::Set::default()),
        };

        let schema = match (schema, main_index.schema()?) {
            (Some(ref expected), Some(ref current)) if current != expected => {
                return Err(Error::SchemaDiffer)
            },
            (Some(expected), Some(_)) => expected,
            (Some(expected), None) => {
                main_index.set_schema(&expected)?;
                expected
            },
            (None, Some(current)) => current,
            (None, None) => return Err(Error::SchemaMissing),
        };

        let ranked_map = match main_index.ranked_map()? {
            Some(map) => map,
            None => RankedMap::default(),
        };

        let cache = Cache { words, synonyms, schema, ranked_map };
        let cache = Arc::new(ArcSwap::from_pointee(cache));

        let index = Index {
            cache,
            main_index,
            synonyms_index,
            words_index,
            docs_words_index,
            documents_index,
            custom_settings_index,
            db,
            updates_index,
            updates_results_index,
            update_callback: Arc::new(ArcSwapOption::empty()),
        };

        let _handle = spawn_update_system(index.clone());

        Ok(index)
    }

    pub fn set_update_callback<F>(&self, callback: F)
    where F: Fn(UpdateStatus) + Send + Sync + 'static
    {
        self.update_callback.store(Some(Arc::new(Box::new(callback))));
    }

    pub fn unset_update_callback(&self) {
        self.update_callback.store(None);
    }

    pub fn stats(&self) -> sled::Result<IndexStats> {
        let cache = self.cache.load();
        Ok(IndexStats {
            number_of_words: cache.words.len(),
            number_of_documents: self.documents_index.len()?,
            number_attrs_in_ranked_map: cache.ranked_map.len(),
        })
    }

    pub fn query_builder(&self) -> QueryBuilder<RefIndex> {
        let ref_index = self.as_ref();
        QueryBuilder::new(ref_index)
    }

    pub fn query_builder_with_criteria<'c>(
        &self,
        criteria: Criteria<'c>,
    ) -> QueryBuilder<'c, RefIndex>
    {
        let ref_index = self.as_ref();
        QueryBuilder::with_criteria(ref_index, criteria)
    }

    pub fn as_ref(&self) -> RefIndex {
        RefIndex {
            cache: self.cache.load(),
            main_index: &self.main_index,
            synonyms_index: &self.synonyms_index,
            words_index: &self.words_index,
            docs_words_index: &self.docs_words_index,
            documents_index: &self.documents_index,
            custom_settings_index: &self.custom_settings_index,
        }
    }

    pub fn schema(&self) -> Schema {
        self.cache.load().schema.clone()
    }

    pub fn custom_settings(&self) -> CustomSettingsIndex {
        self.custom_settings_index.clone()
    }

    pub fn documents_addition<D>(&self) -> DocumentsAddition<D> {
        DocumentsAddition::new(self)
    }

    pub fn documents_deletion(&self) -> DocumentsDeletion {
        DocumentsDeletion::new(self)
    }

    pub fn synonyms_addition(&self) -> SynonymsAddition {
        SynonymsAddition::new(self)
    }

    pub fn synonyms_deletion(&self) -> SynonymsDeletion {
        SynonymsDeletion::new(self)
    }

    pub fn update_status(
        &self,
        update_id: u64,
    ) -> Result<Option<UpdateStatus>, Error>
    {
        let update_id = update_id.to_be_bytes();
        match self.updates_results_index.get(update_id)? {
            Some(value) => {
                let value = bincode::deserialize(&value)?;
                Ok(Some(value))
            },
            None => Ok(None),
        }
    }

    pub fn update_status_blocking(
        &self,
        update_id: u64,
    ) -> Result<UpdateStatus, Error>
    {
        let update_id_bytes = update_id.to_be_bytes().to_vec();
        let mut subscription = self.updates_results_index.watch_prefix(update_id_bytes);

        // if we find the update result return it now
        if let Some(result) = self.update_status(update_id)? {
            return Ok(result)
        }

        // this subscription is used to block the thread
        // until the update_id is inserted in the tree
        subscription.next();

        // the thread has been unblocked, it means that the update result
        // has been inserted in the tree, retrieve it
        Ok(self.update_status(update_id)?.unwrap())
    }

    pub fn document<T>(
        &self,
        fields: Option<&HashSet<&str>>,
        id: DocumentId,
    ) -> Result<Option<T>, DeserializerError>
    where T: de::DeserializeOwned,
    {
        let schema = self.schema();
        let fields = match fields {
            Some(fields) => fields.into_iter().map(|name| schema.attribute(name)).collect(),
            None => None,
        };

        let mut deserializer = Deserializer {
            document_id: id,
            index: &self,
            fields: fields.as_ref(),
        };

        // TODO: currently we return an error if all document fields are missing,
        //       returning None would have been better
        T::deserialize(&mut deserializer).map(Some)
    }
}

impl Index {
    pub(crate) fn push_documents_addition<D>(&self, addition: Vec<D>) -> Result<u64, Error>
    where D: serde::Serialize
    {
        let mut values = Vec::with_capacity(addition.len());
        for add in addition {
            let vec = rmp_serde::to_vec_named(&add)?;
            let add = rmp_serde::from_read(&vec[..])?;
            values.push(add);
        }

        let addition = Update::DocumentsAddition(values);
        let update = rmp_serde::to_vec_named(&addition)?;
        self.raw_push_update(update)
    }

    pub(crate) fn push_documents_deletion(
        &self,
        deletion: Vec<DocumentId>,
    ) -> Result<u64, Error>
    {
        let deletion = Update::DocumentsDeletion(deletion);
        let update = rmp_serde::to_vec_named(&deletion)?;
        self.raw_push_update(update)
    }

    pub(crate) fn push_synonyms_addition(
        &self,
        addition: BTreeMap<String, Vec<String>>,
    ) -> Result<u64, Error>
    {
        let addition = Update::SynonymsAddition(addition);
        let update = rmp_serde::to_vec_named(&addition)?;
        self.raw_push_update(update)
    }

    pub(crate) fn push_synonyms_deletion(
        &self,
        deletion: BTreeMap<String, Option<Vec<String>>>,
    ) -> Result<u64, Error>
    {
        let deletion = Update::SynonymsDeletion(deletion);
        let update = rmp_serde::to_vec_named(&deletion)?;
        self.raw_push_update(update)
    }

    fn raw_push_update(&self, raw_update: Vec<u8>) -> Result<u64, Error> {
        let update_id = self.db.generate_id()?;
        let update_id_array = update_id.to_be_bytes();

        self.updates_index.insert(update_id_array, raw_update)?;

        Ok(update_id)
    }
}

pub struct RefIndex<'a> {
    pub(crate) cache: Guard<'static, Arc<Cache>>,
    pub main_index: &'a MainIndex,
    pub synonyms_index: &'a SynonymsIndex,
    pub words_index: &'a WordsIndex,
    pub docs_words_index: &'a DocsWordsIndex,
    pub documents_index: &'a DocumentsIndex,
    pub custom_settings_index: &'a CustomSettingsIndex,
}

impl Store for RefIndex<'_> {
    type Error = Error;

    fn words(&self) -> Result<&fst::Set, Self::Error> {
        Ok(&self.cache.words)
    }

    fn word_indexes(&self, word: &[u8]) -> Result<Option<SetBuf<DocIndex>>, Self::Error> {
        Ok(self.words_index.doc_indexes(word)?)
    }

    fn synonyms(&self) -> Result<&fst::Set, Self::Error> {
        Ok(&self.cache.synonyms)
    }

    fn alternatives_to(&self, word: &[u8]) -> Result<Option<fst::Set>, Self::Error> {
        Ok(self.synonyms_index.alternatives_to(word)?)
    }
}