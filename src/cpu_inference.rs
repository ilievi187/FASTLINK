//! CPU-only multi-stage loopback MLP correctness foundation.
//!
//! This module provides a small, genuine CPU dense-matrix executor for
//! exercising the existing authenticated loopback stage transport. It is
//! deliberately **not** LLM inference, a tokenizer/sampler, a model loader, a
//! multi-machine deployment, or a throughput benchmark. Weights are fixed
//! caller-supplied `f32` arrays.
//!
//! Activation tensors are serialized as concatenated little-endian IEEE-754
//! `f32` values (`f32::to_le_bytes`). Dense weights are row-major by output
//! feature: `weights[output_index * input_features + input_index]`.
//!
//! Transport framing, per-hop authentication, and loopback sockets are fully
//! delegated to [`crate::node_runtime::run_authenticated_loopback_pipeline`].

use std::fmt;
use std::sync::{Arc, Mutex};

use crate::manifest::PartitionManifest;
use crate::node_runtime::{run_authenticated_loopback_pipeline, RuntimeError};
use crate::pipeline::PipelineSummary;
use crate::{digest, Digest};

/// Maximum number of dense layers in one CPU MLP.
pub const MAX_CPU_LAYERS: usize = 64;
/// Maximum input or output feature count for one layer.
pub const MAX_CPU_FEATURES: usize = 4_096;
/// Maximum number of input tokens accepted by one pipeline call.
pub const MAX_CPU_TOKENS: usize = 32;
/// Maximum total weights plus biases across all layers.
pub const MAX_CPU_PARAMETERS: usize = 4_000_000;
/// Width of one serialized activation value.
pub const CPU_FLOAT_BYTES: usize = 4;

/// Declared activation applied after a dense layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuActivation {
    Identity,
    Relu,
}

impl CpuActivation {
    fn apply(self, value: f32) -> f32 {
        match self {
            Self::Identity => value,
            Self::Relu => {
                if value > 0.0 {
                    value
                } else {
                    0.0
                }
            }
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Identity => 0,
            Self::Relu => 1,
        }
    }
}

/// Safe failure classes for CPU MLP validation/execution.
#[derive(Debug)]
pub enum CpuInferenceError {
    EmptyModel,
    InvalidInputFeatures(usize),
    TooManyLayers(usize),
    InvalidDimensions {
        layer: usize,
        input: usize,
        output: usize,
    },
    LayerInputMismatch {
        layer: usize,
        expected: usize,
        got: usize,
    },
    WeightLength {
        layer: usize,
        expected: usize,
        got: usize,
    },
    BiasLength {
        layer: usize,
        expected: usize,
        got: usize,
    },
    NonFiniteParameter {
        layer: usize,
    },
    TooManyParameters(usize),
    EmptyInput,
    TooManyTokens(usize),
    InputLength {
        token: usize,
        expected: usize,
        got: usize,
    },
    NonFiniteInput {
        token: usize,
    },
    ManifestMismatch(String),
    StageLayout(String),
    InvalidActivationEncoding {
        expected: usize,
        got: usize,
    },
    NonFiniteActivation,
    OutputCount {
        expected: usize,
        got: usize,
    },
    Transport(RuntimeError),
    Internal(String),
}

impl fmt::Display for CpuInferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyModel => write!(f, "CPU MLP has no layers"),
            Self::InvalidInputFeatures(n) => write!(f, "invalid MLP input feature count {n}"),
            Self::TooManyLayers(n) => write!(f, "CPU MLP layer count {n} exceeds limit"),
            Self::InvalidDimensions {
                layer,
                input,
                output,
            } => write!(
                f,
                "layer {layer} has invalid dimensions {input} -> {output}"
            ),
            Self::LayerInputMismatch {
                layer,
                expected,
                got,
            } => write!(
                f,
                "layer {layer} input feature count {got} does not match {expected}"
            ),
            Self::WeightLength {
                layer,
                expected,
                got,
            } => write!(
                f,
                "layer {layer} weight length {got} does not match {expected}"
            ),
            Self::BiasLength {
                layer,
                expected,
                got,
            } => write!(
                f,
                "layer {layer} bias length {got} does not match {expected}"
            ),
            Self::NonFiniteParameter { layer } => {
                write!(f, "layer {layer} contains a non-finite weight or bias")
            }
            Self::TooManyParameters(n) => write!(f, "CPU MLP parameter count {n} exceeds limit"),
            Self::EmptyInput => write!(f, "CPU MLP input batch is empty"),
            Self::TooManyTokens(n) => write!(f, "CPU MLP token count {n} exceeds limit"),
            Self::InputLength {
                token,
                expected,
                got,
            } => write!(
                f,
                "input token {token} has {got} features, expected {expected}"
            ),
            Self::NonFiniteInput { token } => {
                write!(f, "input token {token} contains a non-finite value")
            }
            Self::ManifestMismatch(reason) => {
                write!(f, "CPU MLP/manifest mismatch: {reason}")
            }
            Self::StageLayout(reason) => write!(f, "invalid CPU MLP stage layout: {reason}"),
            Self::InvalidActivationEncoding { expected, got } => {
                write!(f, "activation payload has {got} bytes, expected {expected}")
            }
            Self::NonFiniteActivation => write!(f, "activation output is non-finite"),
            Self::OutputCount { expected, got } => {
                write!(f, "pipeline returned {got} outputs, expected {expected}")
            }
            Self::Transport(_) => write!(f, "authenticated loopback transport failed"),
            Self::Internal(reason) => write!(f, "CPU MLP execution failed: {reason}"),
        }
    }
}

impl std::error::Error for CpuInferenceError {}

/// One dense matrix-vector layer with a declared activation.
#[derive(Clone, Debug, PartialEq)]
pub struct CpuDenseLayer {
    pub input_features: usize,
    pub output_features: usize,
    pub weights: Vec<f32>,
    pub bias: Vec<f32>,
    pub activation: CpuActivation,
}

impl CpuDenseLayer {
    pub fn new(
        input_features: usize,
        output_features: usize,
        weights: Vec<f32>,
        bias: Vec<f32>,
        activation: CpuActivation,
    ) -> Result<Self, CpuInferenceError> {
        let layer = Self {
            input_features,
            output_features,
            weights,
            bias,
            activation,
        };
        layer.validate(0)?;
        Ok(layer)
    }

    fn validate(&self, layer_index: usize) -> Result<(), CpuInferenceError> {
        if self.input_features == 0
            || self.output_features == 0
            || self.input_features > MAX_CPU_FEATURES
            || self.output_features > MAX_CPU_FEATURES
        {
            return Err(CpuInferenceError::InvalidDimensions {
                layer: layer_index,
                input: self.input_features,
                output: self.output_features,
            });
        }
        let expected_weights = self
            .input_features
            .checked_mul(self.output_features)
            .ok_or(CpuInferenceError::TooManyParameters(usize::MAX))?;
        if self.weights.len() != expected_weights {
            return Err(CpuInferenceError::WeightLength {
                layer: layer_index,
                expected: expected_weights,
                got: self.weights.len(),
            });
        }
        if self.bias.len() != self.output_features {
            return Err(CpuInferenceError::BiasLength {
                layer: layer_index,
                expected: self.output_features,
                got: self.bias.len(),
            });
        }
        if self
            .weights
            .iter()
            .chain(self.bias.iter())
            .any(|value| !value.is_finite())
        {
            return Err(CpuInferenceError::NonFiniteParameter { layer: layer_index });
        }
        Ok(())
    }

    fn forward(&self, input: &[f32]) -> Result<Vec<f32>, CpuInferenceError> {
        if input.len() != self.input_features {
            return Err(CpuInferenceError::InputLength {
                token: 0,
                expected: self.input_features,
                got: input.len(),
            });
        }
        let mut output = Vec::with_capacity(self.output_features);
        for output_index in 0..self.output_features {
            let mut value = self.bias[output_index];
            let row = output_index * self.input_features;
            for (input_index, input_value) in input.iter().enumerate() {
                value += self.weights[row + input_index] * input_value;
            }
            value = self.activation.apply(value);
            if !value.is_finite() {
                return Err(CpuInferenceError::NonFiniteActivation);
            }
            output.push(value);
        }
        Ok(output)
    }
}

/// Bounded CPU MLP description.
#[derive(Clone, Debug, PartialEq)]
pub struct CpuMlpSpec {
    pub input_features: usize,
    pub layers: Vec<CpuDenseLayer>,
}

impl CpuMlpSpec {
    pub fn new(
        input_features: usize,
        layers: Vec<CpuDenseLayer>,
    ) -> Result<Self, CpuInferenceError> {
        let spec = Self {
            input_features,
            layers,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Validates dimensions, lengths, finite parameters, and layer chaining.
    pub fn validate(&self) -> Result<(), CpuInferenceError> {
        if self.input_features == 0 || self.input_features > MAX_CPU_FEATURES {
            return Err(CpuInferenceError::InvalidInputFeatures(self.input_features));
        }
        if self.layers.is_empty() {
            return Err(CpuInferenceError::EmptyModel);
        }
        if self.layers.len() > MAX_CPU_LAYERS {
            return Err(CpuInferenceError::TooManyLayers(self.layers.len()));
        }
        let mut expected_input = self.input_features;
        let mut parameters = 0usize;
        for (layer_index, layer) in self.layers.iter().enumerate() {
            layer.validate(layer_index)?;
            if layer.input_features != expected_input {
                return Err(CpuInferenceError::LayerInputMismatch {
                    layer: layer_index,
                    expected: expected_input,
                    got: layer.input_features,
                });
            }
            expected_input = layer.output_features;
            parameters = parameters
                .checked_add(layer.weights.len())
                .and_then(|n| n.checked_add(layer.bias.len()))
                .ok_or(CpuInferenceError::TooManyParameters(usize::MAX))?;
            if parameters > MAX_CPU_PARAMETERS {
                return Err(CpuInferenceError::TooManyParameters(parameters));
            }
        }
        Ok(())
    }

    /// Monolithic reference forward pass over all declared layers.
    pub fn forward(&self, input: &[f32]) -> Result<Vec<f32>, CpuInferenceError> {
        self.validate()?;
        if input.len() != self.input_features {
            return Err(CpuInferenceError::InputLength {
                token: 0,
                expected: self.input_features,
                got: input.len(),
            });
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(CpuInferenceError::NonFiniteInput { token: 0 });
        }
        let mut values = input.to_vec();
        for layer in &self.layers {
            values = layer.forward(&values)?;
        }
        Ok(values)
    }

    /// Canonical model digest bound to dense parameters and activation choices.
    pub fn digest(&self) -> Digest {
        let mut material = Vec::new();
        material.extend_from_slice(b"FASTLINK-CPU-MLP-V1");
        material.extend_from_slice(&(self.input_features as u64).to_le_bytes());
        material.extend_from_slice(&(self.layers.len() as u64).to_le_bytes());
        for layer in &self.layers {
            material.extend_from_slice(&(layer.input_features as u64).to_le_bytes());
            material.extend_from_slice(&(layer.output_features as u64).to_le_bytes());
            material.push(layer.activation.tag());
            for weight in &layer.weights {
                material.extend_from_slice(&weight.to_le_bytes());
            }
            for bias in &layer.bias {
                material.extend_from_slice(&bias.to_le_bytes());
            }
        }
        digest(&material)
    }
}

/// Serializes activations as concatenated little-endian IEEE-754 `f32` values.
pub fn encode_f32_le(values: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * CPU_FLOAT_BYTES);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Decodes exactly `expected` little-endian `f32` values and rejects non-finite data.
pub fn decode_f32_le(bytes: &[u8], expected: usize) -> Result<Vec<f32>, CpuInferenceError> {
    let expected_bytes = expected.checked_mul(CPU_FLOAT_BYTES).ok_or(
        CpuInferenceError::InvalidActivationEncoding {
            expected,
            got: bytes.len(),
        },
    )?;
    if bytes.len() != expected_bytes {
        return Err(CpuInferenceError::InvalidActivationEncoding {
            expected,
            got: bytes.len(),
        });
    }
    let mut values = Vec::with_capacity(expected);
    for index in 0..expected {
        let offset = index * CPU_FLOAT_BYTES;
        let value = f32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        if !value.is_finite() {
            return Err(CpuInferenceError::NonFiniteActivation);
        }
        values.push(value);
    }
    Ok(values)
}

/// Runs a CPU MLP split across the authenticated loopback stage runtime.
///
/// Each stage applies only the layers assigned by the manifest's contiguous
/// `layer_start..layer_end` slice. `manifest.model_digest` must equal
/// [`CpuMlpSpec::digest`]. Shapes, finite parameters, model digest, layer
/// coverage, and input tensor lengths are validated before any socket is
/// opened. Transport framing/authentication remains entirely in
/// [`run_authenticated_loopback_pipeline`].
pub fn run_authenticated_cpu_mlp_pipeline(
    spec: &CpuMlpSpec,
    manifest: &PartitionManifest,
    cluster_secret: &[u8; 32],
    inputs: &[Vec<f32>],
) -> Result<(Vec<Vec<f32>>, PipelineSummary), CpuInferenceError> {
    spec.validate()?;
    manifest
        .verify()
        .map_err(|e| CpuInferenceError::ManifestMismatch(e.to_string()))?;
    if manifest.model_digest != spec.digest() {
        return Err(CpuInferenceError::ManifestMismatch(
            "manifest model_digest does not match CPU MLP parameters".into(),
        ));
    }
    if manifest.total_layers as usize != spec.layers.len() {
        return Err(CpuInferenceError::ManifestMismatch(format!(
            "manifest has {} layers, CPU MLP has {}",
            manifest.total_layers,
            spec.layers.len()
        )));
    }

    let mut ranges = Vec::with_capacity(manifest.stages.len());
    let mut expected_start = 0usize;
    for stage in &manifest.stages {
        let start = stage.layer_start as usize;
        let end = stage.layer_end as usize;
        if start != expected_start || start >= end || end > spec.layers.len() {
            return Err(CpuInferenceError::StageLayout(format!(
                "stage {} covers {start}..{end}, expected start {expected_start}",
                stage.stage_index
            )));
        }
        ranges.push((start, end));
        expected_start = end;
    }
    if ranges.is_empty() || expected_start != spec.layers.len() {
        return Err(CpuInferenceError::StageLayout(
            "stage ranges do not cover every CPU MLP layer".into(),
        ));
    }

    if inputs.is_empty() {
        return Err(CpuInferenceError::EmptyInput);
    }
    if inputs.len() > MAX_CPU_TOKENS {
        return Err(CpuInferenceError::TooManyTokens(inputs.len()));
    }
    for (token, input) in inputs.iter().enumerate() {
        if input.len() != spec.input_features {
            return Err(CpuInferenceError::InputLength {
                token,
                expected: spec.input_features,
                got: input.len(),
            });
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(CpuInferenceError::NonFiniteInput { token });
        }
    }

    let encoded_inputs: Vec<Vec<u8>> = inputs.iter().map(|input| encode_f32_le(input)).collect();
    let spec_ref = Arc::new(spec.clone());
    let ranges_ref = Arc::new(ranges);
    let error_slot = Arc::new(Mutex::new(None::<CpuInferenceError>));
    let stage_slot = Arc::clone(&error_slot);

    let stage_fn = move |stage_index: usize, raw: &[u8]| -> Vec<u8> {
        let result = (|| -> Result<Vec<u8>, CpuInferenceError> {
            let (start, end) = *ranges_ref.get(stage_index).ok_or_else(|| {
                CpuInferenceError::StageLayout(format!(
                    "stage index {stage_index} has no layer range"
                ))
            })?;
            let mut values = decode_f32_le(raw, spec_ref.layers[start].input_features)?;
            for layer in &spec_ref.layers[start..end] {
                values = layer.forward(&values)?;
            }
            Ok(encode_f32_le(&values))
        })();
        match result {
            Ok(output) => output,
            Err(error) => {
                if let Ok(mut slot) = stage_slot.lock() {
                    if slot.is_none() {
                        *slot = Some(error);
                    }
                }
                Vec::new()
            }
        }
    };

    let (raw_outputs, summary) =
        run_authenticated_loopback_pipeline(manifest, cluster_secret, &encoded_inputs, stage_fn)
            .map_err(|error| {
                error_slot
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take())
                    .unwrap_or(CpuInferenceError::Transport(error))
            })?;

    let expected_output = spec
        .layers
        .last()
        .map(|layer| layer.output_features)
        .ok_or(CpuInferenceError::EmptyModel)?;
    let mut outputs = Vec::with_capacity(raw_outputs.len());
    for raw in raw_outputs {
        outputs.push(decode_f32_le(&raw, expected_output)?);
    }
    if outputs.len() != inputs.len() {
        return Err(CpuInferenceError::OutputCount {
            expected: inputs.len(),
            got: outputs.len(),
        });
    }
    Ok((outputs, summary))
}
