//! Loopback multi-stage P2P activation relay runtime.
//!
//! Connects `PartitionManifest`, `ResidencyTable`, and `ActivationFrame` into
//! a multi-stage point-to-point loopback pipeline where each stage verifies
//! its resident weights handle and forwards activations directly to
//! `next_stage_addr` without hairpinning through the coordinator.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::manifest::{PartitionManifest, StageEndpoint};
use crate::pipeline::{
    decode_activation_frame, encode_activation_frame, summarize_pipeline, ActivationError,
    BoundarySample, PipelineSummary, TelemetrySource,
};
use crate::residency::{ResidencyTable, StageStateKind, StateHandleId};
use crate::stage_auth::{self, StageAuthError};
use crate::{digest, Digest};

const MAX_FRAME_WIRE_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub enum RuntimeError {
    Io(io::Error),
    Activation(ActivationError),
    StageAuth(StageAuthError),
    Residency(crate::residency::ResidencyError),
    FrameTooLarge(usize),
    InvalidManifest(String),
}

impl From<io::Error> for RuntimeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ActivationError> for RuntimeError {
    fn from(e: ActivationError) -> Self {
        Self::Activation(e)
    }
}

impl From<StageAuthError> for RuntimeError {
    fn from(e: StageAuthError) -> Self {
        Self::StageAuth(e)
    }
}

impl From<crate::residency::ResidencyError> for RuntimeError {
    fn from(e: crate::residency::ResidencyError) -> Self {
        Self::Residency(e)
    }
}

fn write_framed_packet(stream: &mut TcpStream, packet: &[u8]) -> io::Result<()> {
    let len = u32::try_from(packet.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "packet too large"))?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(packet)?;
    stream.flush()
}

fn read_framed_packet(stream: &mut TcpStream) -> Result<Vec<u8>, RuntimeError> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_WIRE_BYTES {
        return Err(RuntimeError::FrameTooLarge(len));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

struct StageLoopContext<F>
where
    F: Fn(usize, &[u8]) -> Vec<u8> + Send + Sync + 'static,
{
    listener: TcpListener,
    stage: StageEndpoint,
    epoch: u64,
    model_digest: Digest,
    token_count: usize,
    shared_fn: Arc<F>,
    samples: Arc<Mutex<Vec<BoundarySample>>>,
    outputs: Arc<Mutex<Vec<Vec<u8>>>>,
    cluster_secret: Option<[u8; 32]>,
}

impl<F> StageLoopContext<F>
where
    F: Fn(usize, &[u8]) -> Vec<u8> + Send + Sync + 'static,
{
    fn run(&self) -> Result<(), RuntimeError> {
        let mut residency = ResidencyTable::new(64 * 1024 * 1024, self.epoch);
        let weights_handle = StateHandleId {
            stage_index: self.stage.stage_index as u16,
            kind: StageStateKind::Weights,
            slot: 0,
        };
        let stage_weights_digest: Digest = digest(&self.model_digest);
        residency.register(
            weights_handle.clone(),
            self.epoch,
            1024,
            stage_weights_digest,
            true,
        )?;

        for step in 0..self.token_count {
            let (mut stream, _) = self.listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let packet = read_framed_packet(&mut stream)?;

            let compute_start = Instant::now();
            residency.verify_resident(&weights_handle, &stage_weights_digest)?;
            let (_frame, raw) = match self.cluster_secret {
                Some(secret) => {
                    let (src_stage, dst_stage) = receive_hop(self.stage.stage_index);
                    let hop_key =
                        stage_auth::derive_hop_key(&secret, self.epoch, src_stage, dst_stage);
                    stage_auth::open_stage_frame(&hop_key, &packet, self.epoch, dst_stage)?
                }
                None => decode_activation_frame(&packet, self.epoch)?,
            };
            let shared: &F = self.shared_fn.as_ref();
            let out = shared(self.stage.stage_index, &raw);
            let compute_us = compute_start.elapsed().as_micros() as u64;

            if let Some(next_addr) = &self.stage.next_stage_addr {
                let next_frame = match self.cluster_secret {
                    Some(secret) => {
                        let hop_key = stage_auth::derive_hop_key(
                            &secret,
                            self.epoch,
                            self.stage.stage_index as u16,
                            (self.stage.stage_index + 1) as u16,
                        );
                        stage_auth::seal_stage_frame(
                            &hop_key,
                            self.epoch,
                            step as u64,
                            step as u32,
                            self.stage.stage_index as u16,
                            (self.stage.stage_index + 1) as u16,
                            &out,
                        )?
                    }
                    None => encode_activation_frame(
                        self.epoch,
                        step as u64,
                        step as u32,
                        self.stage.stage_index as u16,
                        (self.stage.stage_index + 1) as u16,
                        &out,
                    )?,
                };
                let link_start = Instant::now();
                let mut next_stream = TcpStream::connect(next_addr)?;
                next_stream.set_nodelay(true)?;
                write_framed_packet(&mut next_stream, &next_frame)?;
                let rtt_us = link_start.elapsed().as_micros() as u64;

                self.samples.lock().unwrap().push(BoundarySample {
                    boundary_index: self.stage.stage_index as u16,
                    raw_bytes: out.len() as u64,
                    wire_bytes: next_frame.len() as u64,
                    link_rtt_us: rtt_us,
                    stage_compute_us: compute_us,
                });
            } else {
                self.samples.lock().unwrap().push(BoundarySample {
                    boundary_index: self.stage.stage_index as u16,
                    raw_bytes: out.len() as u64,
                    wire_bytes: 0,
                    link_rtt_us: 0,
                    stage_compute_us: compute_us,
                });
                self.outputs.lock().unwrap().push(out);
            }
        }
        Ok(())
    }
}

fn receive_hop(stage_index: usize) -> (u16, u16) {
    if stage_index == 0 {
        (0, 1)
    } else {
        ((stage_index - 1) as u16, stage_index as u16)
    }
}

/// Runs a multi-stage loopback pipeline for `tokens` sequential steps.
///
/// Each stage runs in its own thread on `127.0.0.1`, verifies its resident
/// weights handle in `ResidencyTable`, decodes/verifies the incoming
/// `ActivationFrame`, applies `stage_fn`, and forwards directly to
/// `next_stage_addr` (or returns the final output to the caller).
pub fn run_loopback_pipeline<F>(
    manifest: &PartitionManifest,
    tokens: &[Vec<u8>],
    stage_fn: F,
) -> Result<(Vec<Vec<u8>>, PipelineSummary), RuntimeError>
where
    F: Fn(usize, &[u8]) -> Vec<u8> + Send + Sync + 'static,
{
    run_pipeline_inner(manifest, None, tokens, stage_fn)
}

/// Runs an authenticated multi-stage loopback pipeline for `tokens` sequential
/// steps. Activation frames are sealed with per-hop, epoch-scoped keys derived
/// from `cluster_secret`; tampered or stale frames fail with
/// `RuntimeError::StageAuth`.
pub fn run_authenticated_loopback_pipeline<F>(
    manifest: &PartitionManifest,
    cluster_secret: &[u8; 32],
    tokens: &[Vec<u8>],
    stage_fn: F,
) -> Result<(Vec<Vec<u8>>, PipelineSummary), RuntimeError>
where
    F: Fn(usize, &[u8]) -> Vec<u8> + Send + Sync + 'static,
{
    run_pipeline_inner(manifest, Some(*cluster_secret), tokens, stage_fn)
}

fn run_pipeline_inner<F>(
    manifest: &PartitionManifest,
    cluster_secret: Option<[u8; 32]>,
    tokens: &[Vec<u8>],
    stage_fn: F,
) -> Result<(Vec<Vec<u8>>, PipelineSummary), RuntimeError>
where
    F: Fn(usize, &[u8]) -> Vec<u8> + Send + Sync + 'static,
{
    manifest
        .verify()
        .map_err(|e| RuntimeError::InvalidManifest(e.to_string()))?;

    let stage_count = manifest.stages.len();
    let listeners: Vec<TcpListener> = (0..stage_count)
        .map(|_| TcpListener::bind("127.0.0.1:0"))
        .collect::<io::Result<Vec<_>>>()?;
    let addrs: Vec<SocketAddr> = listeners
        .iter()
        .map(|l| l.local_addr())
        .collect::<io::Result<Vec<_>>>()?;

    let mut bound_stages: Vec<StageEndpoint> = manifest.stages.clone();
    for (idx, stage) in bound_stages.iter_mut().enumerate() {
        stage.addr = addrs[idx].to_string();
        stage.next_stage_addr = addrs.get(idx + 1).map(|a| a.to_string());
    }

    let epoch = manifest.epoch;
    let model_digest = manifest.model_digest;
    let token_count = tokens.len();
    let shared_fn = Arc::new(stage_fn);
    let samples = Arc::new(Mutex::new(Vec::<BoundarySample>::new()));
    let final_outputs = Arc::new(Mutex::new(Vec::<Vec<u8>>::with_capacity(token_count)));

    let mut handles = Vec::with_capacity(stage_count);
    for (idx, listener) in listeners.into_iter().enumerate() {
        let stage = bound_stages[idx].clone();
        let fn_ref = Arc::clone(&shared_fn);
        let samples_ref = Arc::clone(&samples);
        let outputs_ref = Arc::clone(&final_outputs);

        let handle = thread::spawn(move || -> Result<(), RuntimeError> {
            StageLoopContext {
                listener,
                stage,
                epoch,
                model_digest,
                token_count,
                shared_fn: fn_ref,
                samples: samples_ref,
                outputs: outputs_ref,
                cluster_secret,
            }
            .run()
        });
        handles.push(handle);
    }

    // Drive each token into stage 0.
    let entry_addr = bound_stages[0].addr.clone();
    for (step, token) in tokens.iter().enumerate() {
        let frame = match cluster_secret {
            Some(secret) => {
                let hop_key = stage_auth::derive_hop_key(&secret, epoch, 0, 1);
                stage_auth::seal_stage_frame(
                    &hop_key,
                    epoch,
                    step as u64,
                    step as u32,
                    0,
                    1,
                    token,
                )?
            }
            None => encode_activation_frame(epoch, step as u64, step as u32, 0, 1, token)?,
        };
        let mut stream = TcpStream::connect(&entry_addr)?;
        stream.set_nodelay(true)?;
        write_framed_packet(&mut stream, &frame)?;
    }

    for handle in handles {
        handle
            .join()
            .map_err(|_| RuntimeError::InvalidManifest("stage thread panicked".into()))??;
    }

    let collected_samples = samples.lock().unwrap().clone();
    let collected_outputs = final_outputs.lock().unwrap().clone();
    let summary = summarize_pipeline(
        TelemetrySource::Measured,
        &collected_samples,
        token_count as u64,
    );
    Ok((collected_outputs, summary))
}
