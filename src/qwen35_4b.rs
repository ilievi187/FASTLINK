//! Measured Qwen3.5-4B budget and a memory-fit layer split.
//!
//! Constants come from `docs/v4/MEMORY_BUDGET.md` (commit `674b400` on
//! `feat/fastlink-v4`) and from the GGUF header that document names:
//! `Qwen3.5-4B-3D-Coder-Q4_K_M.gguf`, SHA-256
//! `32c37c83d3ca055d30aefa679f1a1670d3e2eb889ee7cc805792d53f219cfae8`.
//! The recurrent-layer array is the GGUF key
//! `qwen35.attention.recurrent_layers` (1 = recurrent, 0 = full attention).
//! It matches the llama.cpp `qwen35` fallback `(i + 1) % full_attention_interval != 0`.
//!
//! This module does not read a GPU, a socket, or a clock. Decode tok/s is
//! not produced: the memory budget has no decode measurement and no
//! per-layer compute rate. The v3 27B scheduler is a different model.

use std::cmp::Ordering;

/// GGUF `qwen35.block_count`.
pub const BLOCK_COUNT: u32 = 32;
/// GGUF `qwen35.full_attention_interval`.
pub const FULL_ATTENTION_INTERVAL: u32 = 4;
/// GGUF `qwen35.attention.head_count_kv`.
pub const KV_HEADS: u32 = 4;
/// GGUF `qwen35.attention.key_length` / `value_length`.
pub const HEAD_DIM: u32 = 256;
/// Q4_0 block: 32 elements stored as 16 data bytes + 4 scale bytes.
pub const Q4_0_BLOCK_ELEMS: u32 = 32;
pub const Q4_0_BLOCK_BYTES: u32 = 20;
/// GGUF `qwen35.ssm.state_size` and `qwen35.ssm.inner_size`.
pub const SSM_STATE_SIZE: u32 = 128;
pub const SSM_INNER_SIZE: u32 = 4096;
/// GGUF `qwen35.context_length`.
pub const CONTEXT_LENGTH: u64 = 262_144;
/// GGUF file size. MEMORY_BUDGET §2.1.
pub const WEIGHT_FILE_BYTES: u64 = 2_708_804_096;
/// `qwen35.attention.recurrent_layers`, layer 0 first.
pub const RECURRENT_LAYERS: [u8; BLOCK_COUNT as usize] = [
    1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0,
];

/// MEMORY_BUDGET §4 "~3.7 GB" free on the 7900 XT, as decimal bytes.
pub const FREE_VRAM_7900_XT_BYTES: u64 = 3_700_000_000;
/// MEMORY_BUDGET §4 "~7 GB" free on the 6600 XT, as decimal bytes.
pub const FREE_VRAM_6600_XT_BYTES: u64 = 7_000_000_000;
/// MEMORY_BUDGET §2.4. Documented estimate, not a measurement taken here.
pub const ESTIMATED_COMPUTE_BUFFER_BYTES: u64 = 256 * 1024 * 1024;
/// MEMORY_BUDGET §2.5 high end. Documented estimate, not a measurement taken here.
pub const ESTIMATED_BACKEND_HIGH_BYTES: u64 = 192 * 1024 * 1024;

/// Contexts the placement table evaluates. 8K/32K/64K/128K as powers of two.
pub const PLACEMENT_CONTEXTS: [u64; 4] = [8_192, 32_768, 65_536, 131_072];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementError {
    Context,
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideBudget {
    pub layers: u32,
    pub attention_layers: u32,
    pub recurrent_layers: u32,
    pub weight_bytes: u64,
    pub kv_bytes: u64,
    pub ssm_state_bytes: u64,
    /// Compute buffer + high backend estimate. Zero when `layers` is zero.
    pub estimated_overhead_bytes: u64,
    /// `(weights + kv + ssm + overhead) * 1.1`, rounded up. Zero when `layers` is zero.
    pub envelope_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementRow {
    pub context_tokens: u64,
    pub local_layers: u32,
    pub remote_layers: u32,
    pub local: SideBudget,
    pub remote: SideBudget,
    pub feasible: bool,
    /// Always `None`. See module docs.
    pub decode_tokens_per_s: Option<u64>,
}

pub fn is_recurrent_layer(layer: u32) -> bool {
    usize::try_from(layer)
        .ok()
        .and_then(|index| RECURRENT_LAYERS.get(index).copied())
        == Some(1)
}

pub fn is_attention_layer(layer: u32) -> bool {
    usize::try_from(layer)
        .ok()
        .and_then(|index| RECURRENT_LAYERS.get(index).copied())
        == Some(0)
}

/// f32 SSM state only: `state_size * inner_size * 4`. Conv state is not measured.
pub fn ssm_state_bytes() -> usize {
    SSM_STATE_SIZE as usize * SSM_INNER_SIZE as usize * 4
}

/// q4_0 K+V bytes for one KV head and one token.
pub fn kv_q4_0_bytes_per_token_per_head() -> usize {
    let elems = 2 * HEAD_DIM;
    elems as usize * Q4_0_BLOCK_BYTES as usize / Q4_0_BLOCK_ELEMS as usize
}

pub fn per_layer_weight_bytes() -> u64 {
    WEIGHT_FILE_BYTES / u64::from(BLOCK_COUNT)
}

fn ceil_tenths(bytes: u64) -> u64 {
    bytes.saturating_mul(11).saturating_add(9) / 10
}

fn side_budget(layer_lo: u32, layer_hi: u32, context_tokens: u64) -> SideBudget {
    let layers = layer_hi.saturating_sub(layer_lo);
    if layers == 0 {
        return SideBudget {
            layers: 0,
            attention_layers: 0,
            recurrent_layers: 0,
            weight_bytes: 0,
            kv_bytes: 0,
            ssm_state_bytes: 0,
            estimated_overhead_bytes: 0,
            envelope_bytes: 0,
        };
    }
    let mut attention_layers = 0u32;
    let mut recurrent_layers = 0u32;
    for layer in layer_lo..layer_hi {
        if is_recurrent_layer(layer) {
            recurrent_layers += 1;
        } else {
            attention_layers += 1;
        }
    }
    let weight_bytes = u64::from(layers) * per_layer_weight_bytes();
    let kv_bytes = u64::from(attention_layers)
        * u64::from(KV_HEADS)
        * kv_q4_0_bytes_per_token_per_head() as u64
        * context_tokens;
    let ssm_state_bytes = u64::from(recurrent_layers) * ssm_state_bytes() as u64;
    let estimated_overhead_bytes = ESTIMATED_COMPUTE_BUFFER_BYTES + ESTIMATED_BACKEND_HIGH_BYTES;
    let base = weight_bytes
        .saturating_add(kv_bytes)
        .saturating_add(ssm_state_bytes)
        .saturating_add(estimated_overhead_bytes);
    SideBudget {
        layers,
        attention_layers,
        recurrent_layers,
        weight_bytes,
        kv_bytes,
        ssm_state_bytes,
        estimated_overhead_bytes,
        envelope_bytes: ceil_tenths(base),
    }
}

/// Memory-fit split of a contiguous prefix.
///
/// Local layers `0..k` are the 7900 XT. Remote layers `k..32` are the 6600 XT.
/// A side with no layers costs 0. A side with layers pays the §2.4/§2.5 high
/// estimate once, then the §2.7 10% margin.
///
/// Among splits that fit both free-byte budgets, the chosen split minimizes
/// the maximum of `envelope / free`. Ties keep more layers on the 6600 XT.
/// Decode tok/s is left unset.
pub fn select_split(
    context_tokens: u64,
    local_free: u64,
    remote_free: u64,
) -> Result<PlacementRow, PlacementError> {
    if context_tokens == 0 || context_tokens > CONTEXT_LENGTH {
        return Err(PlacementError::Context);
    }
    if local_free == 0 || remote_free == 0 {
        return Err(PlacementError::Budget);
    }
    let mut best: Option<(bool, u128, u64, u32, SideBudget, SideBudget)> = None;
    for k in 0..=BLOCK_COUNT {
        let local = side_budget(0, k, context_tokens);
        let remote = side_budget(k, BLOCK_COUNT, context_tokens);
        let local_over = local.envelope_bytes.saturating_sub(local_free);
        let remote_over = remote.envelope_bytes.saturating_sub(remote_free);
        let feasible = local.envelope_bytes <= local_free && remote.envelope_bytes <= remote_free;
        let overflow = u64::from(!feasible) * local_over.saturating_add(remote_over);
        let ratio = if feasible {
            (local.envelope_bytes as u128 * u128::from(remote_free))
                .max(remote.envelope_bytes as u128 * u128::from(local_free))
        } else {
            u128::MAX
        };
        let replace = match best {
            None => true,
            Some((best_ok, best_ratio, best_over, best_k, _, _)) => {
                // Feasible beats infeasible. Then smaller max-utilization,
                // then smaller overflow, then more layers on the 6600 XT.
                (!feasible, ratio, overflow, k).cmp(&(!best_ok, best_ratio, best_over, best_k))
                    == Ordering::Less
            }
        };
        if replace {
            best = Some((feasible, ratio, overflow, k, local, remote));
        }
    }
    let (feasible, _, _, k, local, remote) = best.ok_or(PlacementError::Budget)?;
    Ok(PlacementRow {
        context_tokens,
        local_layers: k,
        remote_layers: BLOCK_COUNT - k,
        local,
        remote,
        feasible,
        decode_tokens_per_s: None,
    })
}

pub fn placement_table() -> Result<[PlacementRow; 4], PlacementError> {
    let mut rows = [select_split(
        PLACEMENT_CONTEXTS[0],
        FREE_VRAM_7900_XT_BYTES,
        FREE_VRAM_6600_XT_BYTES,
    )?; 4];
    for (slot, context) in PLACEMENT_CONTEXTS.into_iter().enumerate() {
        rows[slot] = select_split(context, FREE_VRAM_7900_XT_BYTES, FREE_VRAM_6600_XT_BYTES)?;
    }
    Ok(rows)
}

/// Largest local prefix that still fits, or `None` when even `k = 0` does not.
pub fn max_feasible_local_layers(context_tokens: u64) -> Result<Option<u32>, PlacementError> {
    let mut found = None;
    for k in 0..=BLOCK_COUNT {
        let local = side_budget(0, k, context_tokens);
        let remote = side_budget(k, BLOCK_COUNT, context_tokens);
        if local.envelope_bytes <= FREE_VRAM_7900_XT_BYTES
            && remote.envelope_bytes <= FREE_VRAM_6600_XT_BYTES
        {
            found = Some(k);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_shape_matches_the_interval_rule() {
        assert_eq!(
            per_layer_weight_bytes() * u64::from(BLOCK_COUNT),
            WEIGHT_FILE_BYTES
        );
        assert_eq!(ssm_state_bytes(), 2_097_152);
        assert_eq!(kv_q4_0_bytes_per_token_per_head(), 320);
        let mut recurrent = 0;
        let mut attention = 0;
        for layer in 0..BLOCK_COUNT {
            let by_interval = (layer + 1) % FULL_ATTENTION_INTERVAL != 0;
            assert_eq!(is_recurrent_layer(layer), by_interval);
            assert_eq!(is_attention_layer(layer), !by_interval);
            if by_interval {
                recurrent += 1;
            } else {
                attention += 1;
            }
        }
        assert_eq!(recurrent, 24);
        assert_eq!(attention, 8);
    }

    #[test]
    fn selected_splits_match_the_published_envelopes() {
        let rows = placement_table().unwrap();
        let expected = [
            (
                8_192,
                9,
                2,
                7,
                761_851_152u64,
                20_971_520u64,
                14_680_064u64,
                1_393_991_263u64,
                6,
                17,
                1_946_952_944u64,
                62_914_560u64,
                35_651_584u64,
                2_766_809_250u64,
            ),
            (
                32_768,
                10,
                2,
                8,
                846_501_280,
                83_886_080,
                16_777_216,
                1_558_619_287,
                6,
                16,
                1_862_302_816,
                251_658_240,
                33_554_432,
                2_879_005_290,
            ),
            (
                65_536,
                10,
                2,
                8,
                846_501_280,
                167_772_160,
                16_777_216,
                1_650_893_975,
                6,
                16,
                1_862_302_816,
                503_316_480,
                33_554_432,
                3_155_829_354,
            ),
            (
                131_072,
                11,
                2,
                9,
                931_151_408,
                335_544_320,
                18_874_368,
                1_930_865_359,
                6,
                15,
                1_777_652_688,
                1_006_632_960,
                31_457_280,
                3_614_055_474,
            ),
        ];
        for (row, item) in rows.iter().zip(expected) {
            let (
                tokens,
                k,
                attn_l,
                ssm_l,
                w_l,
                kv_l,
                ssm_b_l,
                env_l,
                attn_r,
                ssm_r,
                w_r,
                kv_r,
                ssm_b_r,
                env_r,
            ) = item;
            assert_eq!(row.context_tokens, tokens);
            assert_eq!(row.local_layers, k);
            assert_eq!(row.remote_layers, BLOCK_COUNT - k);
            assert!(row.feasible);
            assert_eq!(row.local.attention_layers, attn_l);
            assert_eq!(row.local.recurrent_layers, ssm_l);
            assert_eq!(row.local.weight_bytes, w_l);
            assert_eq!(row.local.kv_bytes, kv_l);
            assert_eq!(row.local.ssm_state_bytes, ssm_b_l);
            assert_eq!(row.local.envelope_bytes, env_l);
            assert_eq!(row.remote.attention_layers, attn_r);
            assert_eq!(row.remote.recurrent_layers, ssm_r);
            assert_eq!(row.remote.weight_bytes, w_r);
            assert_eq!(row.remote.kv_bytes, kv_r);
            assert_eq!(row.remote.ssm_state_bytes, ssm_b_r);
            assert_eq!(row.remote.envelope_bytes, env_r);
            assert_eq!(row.local.estimated_overhead_bytes, 469_762_048);
            assert_eq!(row.remote.estimated_overhead_bytes, 469_762_048);
            assert!(row.decode_tokens_per_s.is_none());
            assert!(row.local.envelope_bytes <= FREE_VRAM_7900_XT_BYTES);
            assert!(row.remote.envelope_bytes <= FREE_VRAM_6600_XT_BYTES);
        }
        assert_eq!(max_feasible_local_layers(8_192).unwrap(), Some(32));
        assert_eq!(max_feasible_local_layers(32_768).unwrap(), Some(30));
        assert_eq!(max_feasible_local_layers(65_536).unwrap(), Some(27));
        assert_eq!(max_feasible_local_layers(131_072).unwrap(), Some(23));
    }

    #[test]
    fn tiny_budgets_are_infeasible_and_do_not_invent_a_rate() {
        let row = select_split(8_192, 1_000, 1_000).unwrap();
        assert!(!row.feasible);
        assert!(row.decode_tokens_per_s.is_none());
        assert_eq!(row.local_layers + row.remote_layers, BLOCK_COUNT);
    }
}
