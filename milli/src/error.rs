use std::collections::BTreeSet;
use std::convert::Infallible;
use std::fmt::Write;
use std::{io, str};

use heed::{Error as HeedError, MdbError};
use rayon::ThreadPoolBuildError;
use serde_json::Value;
use thiserror::Error;

use crate::documents::{self, DocumentsBatchCursorError};
use crate::{CriterionError, DocumentId, FieldId, Object, SortError};

pub fn is_reserved_keyword(keyword: &str) -> bool {
    ["_geo", "_geoDistance", "_geoPoint", "_geoRadius", "_geoBoundingBox"].contains(&keyword)
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("internal: {0}.")]
    InternalError(#[from] InternalError),
    #[error(transparent)]
    IoError(#[from] io::Error),
    #[error(transparent)]
    UserError(#[from] UserError),
}

#[derive(Error, Debug)]
pub enum InternalError {
    #[error("{}", HeedError::DatabaseClosing)]
    DatabaseClosing,
    #[error("Missing {} in the {db_name} database.", key.unwrap_or("key"))]
    DatabaseMissingEntry { db_name: &'static str, key: Option<&'static str> },
    #[error(transparent)]
    FieldIdMapMissingEntry(#[from] FieldIdMapMissingEntry),
    #[error("Missing {key} in the field id mapping.")]
    FieldIdMappingMissingEntry { key: FieldId },
    #[error(transparent)]
    Fst(#[from] fst::Error),
    #[error(transparent)]
    DocumentsError(#[from] documents::Error),
    #[error("Invalid compression type have been specified to grenad.")]
    GrenadInvalidCompressionType,
    #[error("Invalid grenad file with an invalid version format.")]
    GrenadInvalidFormatVersion,
    #[error("Invalid merge while processing {process}.")]
    IndexingMergingKeys { process: &'static str },
    #[error("{}", HeedError::InvalidDatabaseTyping)]
    InvalidDatabaseTyping,
    #[error(transparent)]
    RayonThreadPool(#[from] ThreadPoolBuildError),
    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),
    #[error(transparent)]
    Serialization(#[from] SerializationError),
    #[error(transparent)]
    Store(#[from] MdbError),
    #[error(transparent)]
    Utf8(#[from] str::Utf8Error),
    #[error("An indexation process was explicitly aborted.")]
    AbortedIndexation,
    #[error("The matching words list contains at least one invalid member.")]
    InvalidMatchingWords,
    #[error(transparent)]
    ArroyError(#[from] arroy::Error),
    #[error(transparent)]
    VectorEmbeddingError(#[from] crate::vector::Error),
}

#[derive(Error, Debug)]
pub enum SerializationError {
    #[error("{}", match .db_name {
        Some(name) => format!("decoding from the {name} database failed"),
        None => "decoding failed".to_string(),
    })]
    Decoding { db_name: Option<&'static str> },
    #[error("{}", match .db_name {
        Some(name) => format!("encoding into the {name} database failed"),
        None => "encoding failed".to_string(),
    })]
    Encoding { db_name: Option<&'static str> },
    #[error("number is not a valid finite number")]
    InvalidNumberSerialization,
}

#[derive(Error, Debug)]
pub enum FieldIdMapMissingEntry {
    #[error("unknown field id {field_id} coming from the {process} process")]
    FieldId { field_id: FieldId, process: &'static str },
    #[error("unknown field name {field_name} coming from the {process} process")]
    FieldName { field_name: String, process: &'static str },
}

#[derive(Error, Debug)]
pub enum UserError {
    #[error("A document cannot contain more than 65,535 fields.")]
    AttributeLimitReached,
    #[error(transparent)]
    CriterionError(#[from] CriterionError),
    #[error("Maximum number of documents reached.")]
    DocumentLimitReached,
    #[error(
        "Document identifier `{}` is invalid. \
A document identifier can be of type integer or string, \
only composed of alphanumeric characters (a-z A-Z 0-9), hyphens (-) and underscores (_).", .document_id.to_string()
    )]
    InvalidDocumentId { document_id: Value },
    #[error("Invalid facet distribution, {}", format_invalid_filter_distribution(.invalid_facets_name, .valid_facets_name))]
    InvalidFacetsDistribution {
        invalid_facets_name: BTreeSet<String>,
        valid_facets_name: BTreeSet<String>,
    },
    #[error(transparent)]
    InvalidGeoField(#[from] GeoError),
    #[error("Invalid vector dimensions: expected: `{}`, found: `{}`.", .expected, .found)]
    InvalidVectorDimensions { expected: usize, found: usize },
    #[error("The `_vectors.{subfield}` field in the document with id: `{document_id}` is not an array. Was expecting an array of floats or an array of arrays of floats but instead got `{value}`.")]
    InvalidVectorsType { document_id: Value, value: Value, subfield: String },
    #[error("The `_vectors` field in the document with id: `{document_id}` is not an object. Was expecting an object with a key for each embedder with manually provided vectors, but instead got `{value}`")]
    InvalidVectorsMapType { document_id: Value, value: Value },
    #[error("{0}")]
    InvalidFilter(String),
    #[error("Invalid type for filter subexpression: expected: {}, found: {1}.", .0.join(", "))]
    InvalidFilterExpression(&'static [&'static str], Value),
    #[error("Attribute `{}` is not sortable. {}",
        .field,
        match .valid_fields.is_empty() {
            true => "This index does not have configured sortable attributes.".to_string(),
            false => format!("Available sortable attributes are: `{}{}`.",
                    valid_fields.iter().map(AsRef::as_ref).collect::<Vec<&str>>().join(", "),
                    .hidden_fields.then_some(", <..hidden-attributes>").unwrap_or(""),
                ),
        }
    )]
    InvalidSortableAttribute { field: String, valid_fields: BTreeSet<String>, hidden_fields: bool },
    #[error("Attribute `{}` is not facet-searchable. {}",
        .field,
        match .valid_fields.is_empty() {
            true => "This index does not have configured facet-searchable attributes. To make it facet-searchable add it to the `filterableAttributes` index settings.".to_string(),
            false => format!("Available facet-searchable attributes are: `{}{}`. To make it facet-searchable add it to the `filterableAttributes` index settings.",
                    valid_fields.iter().map(AsRef::as_ref).collect::<Vec<&str>>().join(", "),
                    .hidden_fields.then_some(", <..hidden-attributes>").unwrap_or(""),
                ),
        }
    )]
    InvalidFacetSearchFacetName {
        field: String,
        valid_fields: BTreeSet<String>,
        hidden_fields: bool,
    },
    #[error("Attribute `{}` is not searchable. Available searchable attributes are: `{}{}`.",
        .field,
        .valid_fields.iter().map(AsRef::as_ref).collect::<Vec<&str>>().join(", "),
        .hidden_fields.then_some(", <..hidden-attributes>").unwrap_or(""),
    )]
    InvalidSearchableAttribute {
        field: String,
        valid_fields: BTreeSet<String>,
        hidden_fields: bool,
    },
    #[error("an environment is already opened with different options")]
    InvalidLmdbOpenOptions,
    #[error("You must specify where `sort` is listed in the rankingRules setting to use the sort parameter at search time.")]
    SortRankingRuleMissing,
    #[error("The database file is in an invalid state.")]
    InvalidStoreFile,
    #[error("Maximum database size has been reached.")]
    MaxDatabaseSizeReached,
    #[error("Document doesn't have a `{}` attribute: `{}`.", .primary_key, serde_json::to_string(.document).unwrap())]
    MissingDocumentId { primary_key: String, document: Object },
    #[error("Document have too many matching `{}` attribute: `{}`.", .primary_key, serde_json::to_string(.document).unwrap())]
    TooManyDocumentIds { primary_key: String, document: Object },
    #[error("The primary key inference failed as the engine did not find any field ending with `id` in its name. Please specify the primary key manually using the `primaryKey` query parameter.")]
    NoPrimaryKeyCandidateFound,
    #[error("The primary key inference failed as the engine found {} fields ending with `id` in their names: '{}' and '{}'. Please specify the primary key manually using the `primaryKey` query parameter.", .candidates.len(), .candidates.get(0).unwrap(), .candidates.get(1).unwrap())]
    MultiplePrimaryKeyCandidatesFound { candidates: Vec<String> },
    #[error("There is no more space left on the device. Consider increasing the size of the disk/partition.")]
    NoSpaceLeftOnDevice,
    #[error("Index already has a primary key: `{0}`.")]
    PrimaryKeyCannotBeChanged(String),
    #[error(transparent)]
    SerdeJson(serde_json::Error),
    #[error(transparent)]
    SortError(#[from] SortError),
    #[error("An unknown internal document id have been used: `{document_id}`.")]
    UnknownInternalDocumentId { document_id: DocumentId },
    #[error("`minWordSizeForTypos` setting is invalid. `oneTypo` and `twoTypos` fields should be between `0` and `255`, and `twoTypos` should be greater or equals to `oneTypo` but found `oneTypo: {0}` and twoTypos: {1}`.")]
    InvalidMinTypoWordLenSetting(u8, u8),
    #[error(transparent)]
    VectorEmbeddingError(#[from] crate::vector::Error),
    #[error(transparent)]
    MissingDocumentField(#[from] crate::prompt::error::RenderPromptError),
    #[error(transparent)]
    InvalidPrompt(#[from] crate::prompt::error::NewPromptError),
    #[error("`.embedders.{0}.documentTemplate`: Invalid template: {1}.")]
    InvalidPromptForEmbeddings(String, crate::prompt::error::NewPromptError),
    #[error("Too many embedders in the configuration. Found {0}, but limited to 256.")]
    TooManyEmbedders(usize),
    #[error("Cannot find embedder with name {0}.")]
    InvalidEmbedder(String),
    #[error("Too many vectors for document with id {0}: found {1}, but limited to 256.")]
    TooManyVectors(String, usize),
    #[error("`.embedders.{embedder_name}`: Field `{field}` unavailable for source `{source_}` (only available for sources: {}). Available fields: {}",
        allowed_sources_for_field
         .iter()
         .map(|accepted| format!("`{}`", accepted))
         .collect::<Vec<String>>()
         .join(", "),
        allowed_fields_for_source
         .iter()
         .map(|accepted| format!("`{}`", accepted))
         .collect::<Vec<String>>()
         .join(", ")
    )]
    InvalidFieldForSource {
        embedder_name: String,
        source_: crate::vector::settings::EmbedderSource,
        field: &'static str,
        allowed_fields_for_source: &'static [&'static str],
        allowed_sources_for_field: &'static [crate::vector::settings::EmbedderSource],
    },
    #[error("`.embedders.{embedder_name}.model`: Invalid model `{model}` for OpenAI. Supported models: {:?}", crate::vector::openai::EmbeddingModel::supported_models())]
    InvalidOpenAiModel { embedder_name: String, model: String },
    #[error("`.embedders.{embedder_name}`: Missing field `{field}` (note: this field is mandatory for source {source_})")]
    MissingFieldForSource {
        field: &'static str,
        source_: crate::vector::settings::EmbedderSource,
        embedder_name: String,
    },
}

impl From<crate::vector::Error> for Error {
    fn from(value: crate::vector::Error) -> Self {
        match value.fault() {
            FaultSource::User => Error::UserError(value.into()),
            FaultSource::Runtime => Error::InternalError(value.into()),
            FaultSource::Bug => Error::InternalError(value.into()),
            FaultSource::Undecided => Error::InternalError(value.into()),
        }
    }
}

impl From<arroy::Error> for Error {
    fn from(value: arroy::Error) -> Self {
        match value {
            arroy::Error::Heed(heed) => heed.into(),
            arroy::Error::Io(io) => io.into(),
            arroy::Error::InvalidVecDimension { expected, received } => {
                Error::UserError(UserError::InvalidVectorDimensions { expected, found: received })
            }
            arroy::Error::DatabaseFull
            | arroy::Error::InvalidItemAppend
            | arroy::Error::UnmatchingDistance { .. }
            | arroy::Error::MissingMetadata => {
                Error::InternalError(InternalError::ArroyError(value))
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum GeoError {
    #[error("The `_geo` field in the document with the id: `{document_id}` is not an object. Was expecting an object with the `_geo.lat` and `_geo.lng` fields but instead got `{value}`.")]
    NotAnObject { document_id: Value, value: Value },
    #[error("The `_geo` field in the document with the id: `{document_id}` contains the following unexpected fields: `{value}`.")]
    UnexpectedExtraFields { document_id: Value, value: Value },
    #[error("Could not find latitude nor longitude in the document with the id: `{document_id}`. Was expecting `_geo.lat` and `_geo.lng` fields.")]
    MissingLatitudeAndLongitude { document_id: Value },
    #[error("Could not find latitude in the document with the id: `{document_id}`. Was expecting a `_geo.lat` field.")]
    MissingLatitude { document_id: Value },
    #[error("Could not find longitude in the document with the id: `{document_id}`. Was expecting a `_geo.lng` field.")]
    MissingLongitude { document_id: Value },
    #[error("Could not parse latitude nor longitude in the document with the id: `{document_id}`. Was expecting finite numbers but instead got `{lat}` and `{lng}`.")]
    BadLatitudeAndLongitude { document_id: Value, lat: Value, lng: Value },
    #[error("Could not parse latitude in the document with the id: `{document_id}`. Was expecting a finite number but instead got `{value}`.")]
    BadLatitude { document_id: Value, value: Value },
    #[error("Could not parse longitude in the document with the id: `{document_id}`. Was expecting a finite number but instead got `{value}`.")]
    BadLongitude { document_id: Value, value: Value },
}

fn format_invalid_filter_distribution(
    invalid_facets_name: &BTreeSet<String>,
    valid_facets_name: &BTreeSet<String>,
) -> String {
    if valid_facets_name.is_empty() {
        return "this index does not have configured filterable attributes.".into();
    }

    let mut result = String::new();

    match invalid_facets_name.len() {
        0 => (),
        1 => write!(
            result,
            "attribute `{}` is not filterable.",
            invalid_facets_name.first().unwrap()
        )
        .unwrap(),
        _ => write!(
            result,
            "attributes `{}` are not filterable.",
            invalid_facets_name.iter().map(AsRef::as_ref).collect::<Vec<&str>>().join(", ")
        )
        .unwrap(),
    };

    match valid_facets_name.len() {
        1 => write!(
            result,
            " The available filterable attribute is `{}`.",
            valid_facets_name.first().unwrap()
        )
        .unwrap(),
        _ => write!(
            result,
            " The available filterable attributes are `{}`.",
            valid_facets_name.iter().map(AsRef::as_ref).collect::<Vec<&str>>().join(", ")
        )
        .unwrap(),
    }

    result
}

/// A little macro helper to autogenerate From implementation that needs two `Into`.
/// Given the following parameters: `error_from_sub_error!(FieldIdMapMissingEntry => InternalError)`
/// the macro will create the following code:
/// ```ignore
/// impl From<FieldIdMapMissingEntry> for Error {
///     fn from(error: FieldIdMapMissingEntry) -> Error {
///         Error::from(InternalError::from(error))
///     }
/// }
/// ```
macro_rules! error_from_sub_error {
    () => {};
    ($sub:ty => $intermediate:ty) => {
        impl From<$sub> for Error {
            fn from(error: $sub) -> Error {
                Error::from(<$intermediate>::from(error))
            }
        }
    };
    ($($sub:ty => $intermediate:ty $(,)?),+) => {
        $(error_from_sub_error!($sub => $intermediate);)+
    };
}

error_from_sub_error! {
    FieldIdMapMissingEntry => InternalError,
    fst::Error => InternalError,
    documents::Error => InternalError,
    str::Utf8Error => InternalError,
    ThreadPoolBuildError => InternalError,
    SerializationError => InternalError,
    GeoError => UserError,
    CriterionError => UserError,
}

impl<E> From<grenad::Error<E>> for Error
where
    Error: From<E>,
{
    fn from(error: grenad::Error<E>) -> Error {
        match error {
            grenad::Error::Io(error) => Error::IoError(error),
            grenad::Error::Merge(error) => Error::from(error),
            grenad::Error::InvalidCompressionType => {
                Error::InternalError(InternalError::GrenadInvalidCompressionType)
            }
            grenad::Error::InvalidFormatVersion => {
                Error::InternalError(InternalError::GrenadInvalidFormatVersion)
            }
        }
    }
}

impl From<DocumentsBatchCursorError> for Error {
    fn from(error: DocumentsBatchCursorError) -> Error {
        match error {
            DocumentsBatchCursorError::Grenad(e) => Error::from(e),
            DocumentsBatchCursorError::SerdeJson(e) => Error::from(InternalError::from(e)),
        }
    }
}

impl From<Infallible> for Error {
    fn from(_error: Infallible) -> Error {
        unreachable!()
    }
}

impl From<HeedError> for Error {
    fn from(error: HeedError) -> Error {
        use self::Error::*;
        use self::InternalError::*;
        use self::SerializationError::*;
        use self::UserError::*;

        match error {
            HeedError::Io(error) => Error::from(error),
            HeedError::Mdb(MdbError::MapFull) => UserError(MaxDatabaseSizeReached),
            HeedError::Mdb(MdbError::Invalid) => UserError(InvalidStoreFile),
            HeedError::Mdb(error) => InternalError(Store(error)),
            // TODO use the encoding
            HeedError::Encoding(_) => InternalError(Serialization(Encoding { db_name: None })),
            HeedError::Decoding(_) => InternalError(Serialization(Decoding { db_name: None })),
            HeedError::InvalidDatabaseTyping => InternalError(InvalidDatabaseTyping),
            HeedError::DatabaseClosing => InternalError(DatabaseClosing),
            HeedError::BadOpenOptions { .. } => UserError(InvalidLmdbOpenOptions),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FaultSource {
    User,
    Runtime,
    Bug,
    Undecided,
}

impl std::fmt::Display for FaultSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FaultSource::User => "user error",
            FaultSource::Runtime => "runtime error",
            FaultSource::Bug => "coding error",
            FaultSource::Undecided => "error",
        };
        f.write_str(s)
    }
}

#[test]
fn conditionally_lookup_for_error_message() {
    let prefix = "Attribute `name` is not sortable.";
    let messages = vec![
        (BTreeSet::new(), "This index does not have configured sortable attributes."),
        (BTreeSet::from(["age".to_string()]), "Available sortable attributes are: `age`."),
    ];

    for (list, suffix) in messages {
        let err = UserError::InvalidSortableAttribute {
            field: "name".to_string(),
            valid_fields: list,
            hidden_fields: false,
        };

        assert_eq!(err.to_string(), format!("{} {}", prefix, suffix));
    }
}
