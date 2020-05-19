use std::collections::{HashMap, BTreeMap};

use fst::{set::OpBuilder, SetBuilder};
use indexmap::IndexMap;
use meilisearch_schema::{Schema, FieldId};
use meilisearch_types::DocumentId;
use sdset::{duo::Union, SetOperation};
use serde::Deserialize;
use serde_json::Value;

use crate::database::{MainT, UpdateT};
use crate::database::{UpdateEvent, UpdateEventsEmitter};
use crate::facets;
use crate::raw_indexer::RawIndexer;
use crate::serde::Deserializer;
use crate::store::{self, DocumentsFields, DocumentsFieldsCounts, DiscoverIds};
use crate::update::helpers::{index_value, value_to_number, extract_document_id};
use crate::update::{apply_documents_deletion, compute_short_prefixes, next_update_id, Update};
use crate::{Error, MResult, RankedMap};

pub struct DocumentsAddition<D> {
    updates_store: store::Updates,
    updates_results_store: store::UpdatesResults,
    updates_notifier: UpdateEventsEmitter,
    documents: Vec<D>,
    is_partial: bool,
}

impl<D> DocumentsAddition<D> {
    pub fn new(
        updates_store: store::Updates,
        updates_results_store: store::UpdatesResults,
        updates_notifier: UpdateEventsEmitter,
    ) -> DocumentsAddition<D> {
        DocumentsAddition {
            updates_store,
            updates_results_store,
            updates_notifier,
            documents: Vec::new(),
            is_partial: false,
        }
    }

    pub fn new_partial(
        updates_store: store::Updates,
        updates_results_store: store::UpdatesResults,
        updates_notifier: UpdateEventsEmitter,
    ) -> DocumentsAddition<D> {
        DocumentsAddition {
            updates_store,
            updates_results_store,
            updates_notifier,
            documents: Vec::new(),
            is_partial: true,
        }
    }

    pub fn update_document(&mut self, document: D) {
        self.documents.push(document);
    }

    pub fn finalize(self, writer: &mut heed::RwTxn<UpdateT>) -> MResult<u64>
    where
        D: serde::Serialize,
    {
        let _ = self.updates_notifier.send(UpdateEvent::NewUpdate);
        let update_id = push_documents_addition(
            writer,
            self.updates_store,
            self.updates_results_store,
            self.documents,
            self.is_partial,
        )?;
        Ok(update_id)
    }
}

impl<D> Extend<D> for DocumentsAddition<D> {
    fn extend<T: IntoIterator<Item = D>>(&mut self, iter: T) {
        self.documents.extend(iter)
    }
}

pub fn push_documents_addition<D: serde::Serialize>(
    writer: &mut heed::RwTxn<UpdateT>,
    updates_store: store::Updates,
    updates_results_store: store::UpdatesResults,
    addition: Vec<D>,
    is_partial: bool,
) -> MResult<u64> {
    let mut values = Vec::with_capacity(addition.len());
    for add in addition {
        let vec = serde_json::to_vec(&add)?;
        let add = serde_json::from_slice(&vec)?;
        values.push(add);
    }

    let last_update_id = next_update_id(writer, updates_store, updates_results_store)?;

    let update = if is_partial {
        Update::documents_partial(values)
    } else {
        Update::documents_addition(values)
    };

    updates_store.put_update(writer, last_update_id, &update)?;

    Ok(last_update_id)
}

fn index_document(
    writer: &mut heed::RwTxn<MainT>,
    documents_fields: DocumentsFields,
    documents_fields_counts: DocumentsFieldsCounts,
    ranked_map: &mut RankedMap,
    indexer: &mut RawIndexer,
    schema: &Schema,
    field_id: FieldId,
    document_id: DocumentId,
    value: &Value,
) -> MResult<()>
{
    let serialized = serde_json::to_vec(value)?;
    documents_fields.put_document_field(writer, document_id, field_id, &serialized)?;

    if let Some(indexed_pos) = schema.is_indexed(field_id) {
        let number_of_words = index_value(indexer, document_id, *indexed_pos, value);
        if let Some(number_of_words) = number_of_words {
            documents_fields_counts.put_document_field_count(
                writer,
                document_id,
                *indexed_pos,
                number_of_words as u16,
            )?;
        }
    }

    if schema.is_ranked(field_id) {
        let number = value_to_number(value).unwrap_or_default();
        ranked_map.insert(document_id, field_id, number);
    }

    Ok(())
}

pub fn apply_addition<'a, 'b>(
    writer: &'a mut heed::RwTxn<'b, MainT>,
    index: &store::Index,
    new_documents: Vec<IndexMap<String, Value>>,
    partial: bool
) -> MResult<()> {
    let mut documents_additions = HashMap::new();
    let mut new_user_ids = BTreeMap::new();
    let mut new_internal_ids = Vec::with_capacity(new_documents.len());

    let mut schema = match index.main.schema(writer)? {
        Some(schema) => schema,
        None => return Err(Error::SchemaMissing),
    };

    // Retrieve the documents ids related structures
    let user_ids = index.main.user_ids(writer)?;
    let internal_ids = index.main.internal_ids(writer)?;
    let mut available_ids = DiscoverIds::new(&internal_ids);

    let primary_key = schema.primary_key().ok_or(Error::MissingPrimaryKey)?;

    // 1. store documents ids for future deletion
    for mut document in new_documents {
        let (document_id, userid) = extract_document_id(&primary_key, &document, &user_ids, &mut available_ids)?;
        new_user_ids.insert(userid, document_id.0);
        new_internal_ids.push(document_id);

        if partial {
            let mut deserializer = Deserializer {
                document_id,
                reader: writer,
                documents_fields: index.documents_fields,
                schema: &schema,
                fields: None,
            };

            let old_document = Option::<HashMap<String, Value>>::deserialize(&mut deserializer)?;
            if let Some(old_document) = old_document {
                for (key, value) in old_document {
                    document.entry(key).or_insert(value);
                }
            }
        }
        documents_additions.insert(document_id, document);
    }

    // 2. remove the documents posting lists
    let number_of_inserted_documents = documents_additions.len();
    let documents_ids = documents_additions.iter().map(|(id, _)| *id).collect();
    apply_documents_deletion(writer, index, documents_ids)?;

    let mut ranked_map = match index.main.ranked_map(writer)? {
        Some(ranked_map) => ranked_map,
        None => RankedMap::default(),
    };

    let stop_words = match index.main.stop_words_fst(writer)? {
        Some(stop_words) => stop_words,
        None => fst::Set::default(),
    };

    // 3. index the documents fields in the stores
    if let Some(attributes_for_facetting) = index.main.attributes_for_faceting(writer)? {
        let facet_map = facets::facet_map_from_docs(&schema, &documents_additions, attributes_for_facetting.as_ref())?;
        index.facets.add(writer, facet_map)?;
    }

    let mut indexer = RawIndexer::new(stop_words);

    // For each document in this update
    for (document_id, document) in documents_additions {
        // For each key-value pair in the document.
        for (attribute, value) in document {
            let field_id = schema.insert_and_index(&attribute)?;
            index_document(
                writer,
                index.documents_fields,
                index.documents_fields_counts,
                &mut ranked_map,
                &mut indexer,
                &schema,
                field_id,
                document_id,
                &value,
            )?;
        }
    }

    write_documents_addition_index(
        writer,
        index,
        &ranked_map,
        number_of_inserted_documents,
        indexer,
    )?;

    index.main.put_schema(writer, &schema)?;

    let new_user_ids = fst::Map::from_iter(new_user_ids)?;
    let new_internal_ids = sdset::SetBuf::from_dirty(new_internal_ids);
    index.main.merge_user_ids(writer, &new_user_ids)?;
    index.main.merge_internal_ids(writer, &new_internal_ids)?;

    Ok(())
}

pub fn apply_documents_partial_addition<'a, 'b>(
    writer: &'a mut heed::RwTxn<'b, MainT>,
    index: &store::Index,
    new_documents: Vec<IndexMap<String, Value>>,
) -> MResult<()> {
    apply_addition(writer, index, new_documents, true)
}

pub fn apply_documents_addition<'a, 'b>(
    writer: &'a mut heed::RwTxn<'b, MainT>,
    index: &store::Index,
    new_documents: Vec<IndexMap<String, Value>>,
) -> MResult<()> {
    apply_addition(writer, index, new_documents, false)
}

pub fn reindex_all_documents(writer: &mut heed::RwTxn<MainT>, index: &store::Index) -> MResult<()> {
    let schema = match index.main.schema(writer)? {
        Some(schema) => schema,
        None => return Err(Error::SchemaMissing),
    };

    let mut ranked_map = RankedMap::default();

    // 1. retrieve all documents ids
    let mut documents_ids_to_reindex = Vec::new();
    for result in index.documents_fields_counts.documents_ids(writer)? {
        let document_id = result?;
        documents_ids_to_reindex.push(document_id);
    }

    // 2. remove the documents posting lists
    index.main.put_words_fst(writer, &fst::Set::default())?;
    index.main.put_ranked_map(writer, &ranked_map)?;
    index.main.put_number_of_documents(writer, |_| 0)?;
    index.facets.clear(writer)?;
    index.postings_lists.clear(writer)?;
    index.docs_words.clear(writer)?;

    let stop_words = match index.main.stop_words_fst(writer)? {
        Some(stop_words) => stop_words,
        None => fst::Set::default(),
    };

    let number_of_inserted_documents = documents_ids_to_reindex.len();
    let mut indexer = RawIndexer::new(stop_words);
    let mut ram_store = HashMap::new();

    if let Some(ref attributes_for_facetting) = index.main.attributes_for_faceting(writer)? {
        let facet_map = facets::facet_map_from_docids(writer, &index, &documents_ids_to_reindex, &attributes_for_facetting)?;
        index.facets.add(writer, facet_map)?;
    }
    // ^-- https://github.com/meilisearch/MeiliSearch/pull/631#issuecomment-626624470 --v
    for document_id in documents_ids_to_reindex {
        for result in index.documents_fields.document_fields(writer, document_id)? {
            let (field_id, bytes) = result?;
            let value: Value = serde_json::from_slice(bytes)?;
            ram_store.insert((document_id, field_id), value);
        }

        // For each key-value pair in the document.
        for ((document_id, field_id), value) in ram_store.drain() {
            index_document(
                writer,
                index.documents_fields,
                index.documents_fields_counts,
                &mut ranked_map,
                &mut indexer,
                &schema,
                field_id,
                document_id,
                &value,
            )?;
        }
    }

    // 4. write the new index in the main store
    write_documents_addition_index(
        writer,
        index,
        &ranked_map,
        number_of_inserted_documents,
        indexer,
    )?;

    index.main.put_schema(writer, &schema)?;

    Ok(())
}

pub fn write_documents_addition_index(
    writer: &mut heed::RwTxn<MainT>,
    index: &store::Index,
    ranked_map: &RankedMap,
    number_of_inserted_documents: usize,
    indexer: RawIndexer,
) -> MResult<()> {
    let indexed = indexer.build();
    let mut delta_words_builder = SetBuilder::memory();

    for (word, delta_set) in indexed.words_doc_indexes {
        delta_words_builder.insert(&word).unwrap();

        let set = match index.postings_lists.postings_list(writer, &word)? {
            Some(postings) => Union::new(&postings.matches, &delta_set).into_set_buf(),
            None => delta_set,
        };

        index.postings_lists.put_postings_list(writer, &word, &set)?;
    }

    for (id, words) in indexed.docs_words {
        index.docs_words.put_doc_words(writer, id, &words)?;
    }

    let delta_words = delta_words_builder
        .into_inner()
        .and_then(fst::Set::from_bytes)
        .unwrap();

    let words = match index.main.words_fst(writer)? {
        Some(words) => {
            let op = OpBuilder::new()
                .add(words.stream())
                .add(delta_words.stream())
                .r#union();

            let mut words_builder = SetBuilder::memory();
            words_builder.extend_stream(op).unwrap();
            words_builder
                .into_inner()
                .and_then(fst::Set::from_bytes)
                .unwrap()
        }
        None => delta_words,
    };

    index.main.put_words_fst(writer, &words)?;
    index.main.put_ranked_map(writer, ranked_map)?;
    index.main.put_number_of_documents(writer, |old| old + number_of_inserted_documents as u64)?;

    compute_short_prefixes(writer, index)?;

    Ok(())
}
