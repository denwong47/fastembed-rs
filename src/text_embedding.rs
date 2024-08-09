#[cfg(feature = "online")]
use crate::common::load_tokenizer_hf_hub;
use crate::{
    common::{load_tokenizer, normalize, Tokenizer, TokenizerFiles, DEFAULT_CACHE_DIR},
    models::text_embedding::models_list,
    pooling::Pooling,
    Embedding, EmbeddingModel, EmbeddingOutput, ModelInfo, OutputKey, SingleBatchOutput,
};
use anyhow::Result;
#[cfg(feature = "online")]
use hf_hub::{
    api::sync::{ApiBuilder, ApiRepo},
    Cache,
};
use ndarray::{s, Array, Dimension};
use ort::{ExecutionProviderDispatch, GraphOptimizationLevel, Session, Value};
use rayon::{
    iter::{FromParallelIterator, ParallelIterator},
    slice::ParallelSlice,
};
use std::{
    fmt::Display,
    path::{Path, PathBuf},
    thread::available_parallelism,
};

const DEFAULT_BATCH_SIZE: usize = 256;
const DEFAULT_MAX_LENGTH: usize = 512;
const DEFAULT_EMBEDDING_MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15;

/// Options for initializing the TextEmbedding model
#[derive(Debug, Clone)]
pub struct InitOptions {
    pub model_name: EmbeddingModel,
    pub execution_providers: Vec<ExecutionProviderDispatch>,
    pub max_length: usize,
    pub cache_dir: PathBuf,
    pub show_download_progress: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            model_name: DEFAULT_EMBEDDING_MODEL,
            execution_providers: Default::default(),
            max_length: DEFAULT_MAX_LENGTH,
            cache_dir: Path::new(DEFAULT_CACHE_DIR).to_path_buf(),
            show_download_progress: true,
        }
    }
}

/// Options for initializing UserDefinedEmbeddingModel
///
/// Model files are held by the UserDefinedEmbeddingModel struct
#[derive(Debug, Clone)]
pub struct InitOptionsUserDefined {
    pub execution_providers: Vec<ExecutionProviderDispatch>,
    pub max_length: usize,
}

impl Default for InitOptionsUserDefined {
    fn default() -> Self {
        Self {
            execution_providers: Default::default(),
            max_length: DEFAULT_MAX_LENGTH,
        }
    }
}

/// Convert InitOptions to InitOptionsUserDefined
///
/// This is useful for when the user wants to use the same options for both the default and user-defined models
impl From<InitOptions> for InitOptionsUserDefined {
    fn from(options: InitOptions) -> Self {
        InitOptionsUserDefined {
            execution_providers: options.execution_providers,
            max_length: options.max_length,
        }
    }
}

/// Struct for "bring your own" embedding models
///
/// The onnx_file and tokenizer_files are expecting the files' bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDefinedEmbeddingModel {
    pub onnx_file: Vec<u8>,
    pub tokenizer_files: TokenizerFiles,
    pub pooling: Option<Pooling>,
}

/// Output types and functions for the [`TextEmbedding`] model.
pub mod output {
    use crate::OutputPrecedence;

    use super::*;

    /// The default output precedence for the TextEmbedding model.
    pub const OUTPUT_TYPE_PRECENDENCE: &[OutputKey] = &[
        OutputKey::OnlyOne,
        OutputKey::ByName("last_hidden_state"),
        OutputKey::ByName("sentence_embedding"),
        // Better not to expose this unless the user explicitly asks for it.
        // OutputKey::ByName("token_embeddings"),
    ];

    /// Generates thea default array transformer for the [`TextEmbedding`] model using the
    /// provided output precedence.
    #[allow(unused_variables)]
    pub fn transformer_with_precedence(
        output_precedence: impl OutputPrecedence,
        pooling: Option<Pooling>,
    ) -> impl Fn(&[SingleBatchOutput]) -> anyhow::Result<Vec<Embedding>> {
        move |batches| {
            // Not using `par_iter` here: the operations here is probably not
            // computationally expensive enough to warrant spinning up costs of the threads.
            batches
                .iter()
                .map(|batch| {
                    batch
                        .select_and_pool_output(&output_precedence, pooling.clone())
                        .and_then(|array| match array.dim().ndim() {
                            // 2D tensor - `sentence-transformers` models
                            2 => Ok(array
                                .rows()
                                .into_iter()
                                .map(|row| normalize(row.as_slice().unwrap()))
                                .collect::<Vec<Embedding>>()),
                            // 3D tensor - `Qdrant`, `BERT` models etc
                            3 => Ok(array
                                .slice(s![.., 0, ..])
                                .rows()
                                .into_iter()
                                .map(|row| normalize(row.as_slice().unwrap()))
                                .collect::<Vec<Embedding>>()),
                            _ => Err(anyhow::Error::msg(format!(
                                "Invalid output shape: {shape:?}. Expected 2D or 3D tensor.",
                                shape = array.dim()
                            ))),
                        })
                })
                .try_fold(Vec::new(), |mut acc, res| {
                    acc.extend(res?);
                    Ok(acc)
                })
        }
    }
}

/// Rust representation of the TextEmbedding model
pub struct TextEmbedding {
    pub tokenizer: Tokenizer,
    pub pooling: Option<Pooling>,
    session: Session,
    need_token_type_ids: bool,
}

impl Display for EmbeddingModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let model_info = TextEmbedding::list_supported_models()
            .into_iter()
            .find(|model| model.model == *self)
            .unwrap();
        write!(f, "{}", model_info.model_code)
    }
}

impl TextEmbedding {
    /// Try to generate a new TextEmbedding Instance
    ///
    /// Uses the highest level of Graph optimization
    ///
    /// Uses the total number of CPUs available as the number of intra-threads
    #[cfg(feature = "online")]
    pub fn try_new(options: InitOptions) -> Result<Self> {
        let InitOptions {
            model_name,
            execution_providers,
            max_length,
            cache_dir,
            show_download_progress,
        } = options;

        let threads = available_parallelism()?.get();

        let model_repo = TextEmbedding::retrieve_model(
            model_name.clone(),
            cache_dir.clone(),
            show_download_progress,
        )?;

        let model_file_name = TextEmbedding::get_model_info(&model_name).model_file;
        let model_file_reference = model_repo
            .get(&model_file_name)
            .unwrap_or_else(|_| panic!("Failed to retrieve {} ", model_file_name));

        // TODO: If more models need .onnx_data, implement a better way to handle this
        // Probably by adding `additional_files` field in the `ModelInfo` struct
        if model_name == EmbeddingModel::MultilingualE5Large {
            model_repo
                .get("model.onnx_data")
                .expect("Failed to retrieve model.onnx_data.");
        }

        // prioritise loading pooling config if available, if not (thanks qdrant!), look for it in hardcoded
        let post_processing = model_name.get_default_pooling_method();

        let session = Session::builder()?
            .with_execution_providers(execution_providers)?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(threads)?
            .commit_from_file(model_file_reference)?;

        let tokenizer = load_tokenizer_hf_hub(model_repo, max_length)?;
        dbg!((&model_name, &post_processing));
        Ok(Self::new(tokenizer, session, post_processing))
    }

    /// Create a TextEmbedding instance from model files provided by the user.
    ///
    /// This can be used for 'bring your own' embedding models
    pub fn try_new_from_user_defined(
        model: UserDefinedEmbeddingModel,
        options: InitOptionsUserDefined,
    ) -> Result<Self> {
        let InitOptionsUserDefined {
            execution_providers,
            max_length,
        } = options;

        let threads = available_parallelism()?.get();

        let session = Session::builder()?
            .with_execution_providers(execution_providers)?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(threads)?
            .commit_from_memory(&model.onnx_file)?;

        let tokenizer = load_tokenizer(model.tokenizer_files, max_length)?;
        dbg!(&model.pooling);
        Ok(Self::new(tokenizer, session, model.pooling))
    }

    /// Private method to return an instance
    fn new(tokenizer: Tokenizer, session: Session, post_process: Option<Pooling>) -> Self {
        let need_token_type_ids = session
            .inputs
            .iter()
            .any(|input| input.name == "token_type_ids");
        Self {
            tokenizer,
            session,
            need_token_type_ids,
            pooling: post_process,
        }
    }
    /// Return the TextEmbedding model's directory from cache or remote retrieval
    #[cfg(feature = "online")]
    fn retrieve_model(
        model: EmbeddingModel,
        cache_dir: PathBuf,
        show_download_progress: bool,
    ) -> Result<ApiRepo> {
        let cache = Cache::new(cache_dir);
        let api = ApiBuilder::from_cache(cache)
            .with_progress(show_download_progress)
            .build()
            .unwrap();

        let repo = api.model(model.to_string());
        Ok(repo)
    }

    /// Retrieve a list of supported models
    pub fn list_supported_models() -> Vec<ModelInfo<EmbeddingModel>> {
        models_list()
    }

    /// Get ModelInfo from EmbeddingModel
    pub fn get_model_info(model: &EmbeddingModel) -> ModelInfo<EmbeddingModel> {
        TextEmbedding::list_supported_models()
            .into_iter()
            .find(|m| &m.model == model)
            .expect("Model not found.")
    }

    /// Method to generate an [`ort::SessionOutputs`] wrapped in a [`EmbeddingOutput`]
    /// instance, which can be used to extract the embeddings with default or custom
    /// methods as well as output key precedence.
    ///
    /// Metadata that could be useful for creating the array transformer is
    /// returned alongside the [`EmbeddingOutput`] instance, such as pooling methods
    /// etc.
    ///
    /// # Note
    ///
    /// This is a lower level method than [`TextEmbedding::embed`], and is useful
    /// when you need to extract the session outputs in a custom way.
    ///
    /// If you want to extract the embeddings directly, use [`TextEmbedding::embed`].
    ///
    /// If you want to use the raw session outputs, use [`EmbeddingOutput::into_raw`]
    /// on the output of this method.
    ///
    /// If you want to choose a different export key or customise the way the batch
    /// arrays are aggregated, you can define your own array transformer
    /// and use it on [`EmbeddingOutput::export_with_transformer`] to extract the
    /// embeddings with your custom output type.
    pub fn transform<'e, 'r, 's, S: AsRef<str> + Send + Sync>(
        &'e self,
        texts: Vec<S>,
        batch_size: Option<usize>,
    ) -> Result<EmbeddingOutput<'r, 's>>
    where
        'e: 'r,
        'e: 's,
    {
        // Determine the batch size, default if not specified
        let batch_size = batch_size.unwrap_or(DEFAULT_BATCH_SIZE);

        let batches =
            anyhow::Result::<Vec<_>>::from_par_iter(texts.par_chunks(batch_size).map(|batch| {
                // Encode the texts in the batch
                let inputs = batch.iter().map(|text| text.as_ref()).collect();
                let encodings = self.tokenizer.encode_batch(inputs, true).unwrap();

                // Extract the encoding length and batch size
                let encoding_length = encodings[0].len();
                let batch_size = batch.len();

                let max_size = encoding_length * batch_size;

                // Preallocate arrays with the maximum size
                let mut ids_array = Vec::with_capacity(max_size);
                let mut mask_array = Vec::with_capacity(max_size);
                let mut typeids_array = Vec::with_capacity(max_size);

                // Not using par_iter because the closure needs to be FnMut
                encodings.iter().for_each(|encoding| {
                    let ids = encoding.get_ids();
                    let mask = encoding.get_attention_mask();
                    let typeids = encoding.get_type_ids();

                    // Extend the preallocated arrays with the current encoding
                    // Requires the closure to be FnMut
                    ids_array.extend(ids.iter().map(|x| *x as i64));
                    mask_array.extend(mask.iter().map(|x| *x as i64));
                    typeids_array.extend(typeids.iter().map(|x| *x as i64));
                });

                // Create CowArrays from vectors
                let inputs_ids_array =
                    Array::from_shape_vec((batch_size, encoding_length), ids_array)?;

                let attention_mask_array =
                    Array::from_shape_vec((batch_size, encoding_length), mask_array)?;

                let token_type_ids_array =
                    Array::from_shape_vec((batch_size, encoding_length), typeids_array)?;

                let mut session_inputs = ort::inputs![
                    "input_ids" => Value::from_array(inputs_ids_array)?,
                    "attention_mask" => Value::from_array(attention_mask_array.view())?,
                ]?;

                if self.need_token_type_ids {
                    session_inputs.push((
                        "token_type_ids".into(),
                        Value::from_array(token_type_ids_array)?.into(),
                    ));
                }

                Ok(
                    // Package all the data required for post-processing (e.g. pooling)
                    // into a SingleBatchOutput struct.
                    SingleBatchOutput {
                        session_outputs: self
                            .session
                            .run(session_inputs)
                            .map_err(anyhow::Error::new)?,
                        attention_mask_array,
                    },
                )
            }))?;

        Ok(EmbeddingOutput::new(batches))
    }

    /// Method to generate sentence embeddings for a Vec of texts.
    ///
    /// Accepts a [`Vec`] consisting of elements of either [`String`], &[`str`],
    /// [`std::ffi::OsString`], &[`std::ffi::OsStr`].
    ///
    /// The output is a [`Vec`] of [`Embedding`]s.
    ///
    /// # Note
    ///
    /// This method is a higher level method than [`TextEmbedding::transform`] by utilizing
    /// the default output precedence and array transformer for the [`TextEmbedding`] model.
    pub fn embed<S: AsRef<str> + Send + Sync>(
        &self,
        texts: Vec<S>,
        batch_size: Option<usize>,
    ) -> Result<Vec<Embedding>> {
        let batches = self.transform(texts, batch_size)?;

        batches.export_with_transformer(output::transformer_with_precedence(
            output::OUTPUT_TYPE_PRECENDENCE,
            self.pooling.clone(),
        ))
    }
}
