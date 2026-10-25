pub mod client;
pub mod packets;
pub mod peer;
pub mod scenarios;

use std::{
    collections::hash_map::DefaultHasher,
    fmt::Write as _,
    hash::{Hash, Hasher},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use pumpkin_util::math::position::BlockPos;
use serde::{Deserialize, Serialize};

use crate::lockstep::{
    client::BotTarget,
    packets::RecordedClientPacket,
    peer::{
        ExternalJavaPeer, ExternalJavaServerProcess, InProcessPumpkinPeer, LockstepWorldSnapshot,
    },
    scenarios::{
        LockstepScenario, VanillaJavaExpectation, all_lockstep_scenarios, load_scenarios_from_path,
        scenario_from_world_save,
    },
};

pub const DEFAULT_LOCKSTEP_ORIGIN: BlockPos = BlockPos::new(0, 200, 0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockstepStepRecording {
    pub step: usize,
    pub label: String,
    pub state_hash: String,
    pub primary_packets: Vec<RecordedClientPacket>,
    pub observer_packets: Vec<RecordedClientPacket>,
    pub world_snapshot: LockstepWorldSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockstepScenarioRecording {
    pub scenario_id: String,
    pub category: String,
    pub description: String,
    pub total_steps: usize,
    pub steps: Vec<LockstepStepRecording>,
}

impl LockstepScenarioRecording {
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json_str(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockstepStepDiff {
    pub scenario_id: String,
    pub step: usize,
    pub label: String,
    pub peer_lockstep_mismatches: Vec<String>,
    pub baseline_mismatches: Vec<String>,
    pub java_parity_divergences: Vec<String>,
}

impl LockstepStepDiff {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.peer_lockstep_mismatches.is_empty()
            && self.baseline_mismatches.is_empty()
            && self.java_parity_divergences.is_empty()
    }

    #[must_use]
    pub fn format_human_readable(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "=== Lockstep Divergence in '{}' at step {} ('{}') ===",
            self.scenario_id, self.step, self.label
        );
        for m in &self.peer_lockstep_mismatches {
            let _ = writeln!(out, "  [PEER LOCKSTEP] {m}");
        }
        for m in &self.baseline_mismatches {
            let _ = writeln!(out, "  [BASELINE DIFF] {m}");
        }
        for m in &self.java_parity_divergences {
            let _ = writeln!(out, "  [JAVA vs PUMPKIN] {m}");
        }
        out
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockstepComplianceReport {
    pub total_scenarios: usize,
    pub peer_deterministic_scenarios: usize,
    pub baseline_compliant_scenarios: usize,
    pub java_compliant_scenarios: usize,
    pub java_diverged_scenarios: usize,
    pub missing_baseline_scenarios: Vec<String>,
    pub scenario_diffs: Vec<LockstepStepDiff>,
}

impl LockstepComplianceReport {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn java_compliance_percentage(&self) -> f64 {
        if self.total_scenarios == 0 {
            return 100.0;
        }
        (self.java_compliant_scenarios as f64 / self.total_scenarios as f64) * 100.0
    }

    #[must_use]
    pub fn format_summary(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Dual-Server Lockstep Parity Summary: {}/{} Java-compliant ({:.1}%), {}/{} peer-deterministic, {} Java-vs-Pumpkin diverged, {} missing baseline",
            self.java_compliant_scenarios,
            self.total_scenarios,
            self.java_compliance_percentage(),
            self.peer_deterministic_scenarios,
            self.total_scenarios,
            self.java_diverged_scenarios,
            self.missing_baseline_scenarios.len()
        );
        for diff in &self.scenario_diffs {
            let _ = write!(out, "{}", diff.format_human_readable());
        }
        out
    }
}

#[derive(Debug, Clone, Default)]
pub struct LockstepCliConfig {
    pub dump_dir: Option<PathBuf>,
    pub baseline_dir: Option<PathBuf>,
    pub strict_parity: bool,
    pub only_lockstep: bool,
    pub java_jar: Option<PathBuf>,
    pub java_addr: Option<SocketAddr>,
    pub scenarios_path: Option<PathBuf>,
    pub world_save: Option<PathBuf>,
    pub duration_ticks: Option<usize>,
}

impl LockstepCliConfig {
    #[must_use]
    pub fn from_env_and_args() -> Self {
        let mut cfg = Self::default();

        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_DUMP_DIR")
            && !val.trim().is_empty()
        {
            cfg.dump_dir = Some(PathBuf::from(val));
        }
        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_BASELINE_DIR")
            && !val.trim().is_empty()
        {
            cfg.baseline_dir = Some(PathBuf::from(val));
        }
        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_STRICT") {
            cfg.strict_parity = val == "1" || val.eq_ignore_ascii_case("true");
        }
        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_ONLY") {
            cfg.only_lockstep = val == "1" || val.eq_ignore_ascii_case("true");
        }
        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_JAVA_JAR")
            && !val.trim().is_empty()
        {
            cfg.java_jar = Some(PathBuf::from(val));
        }
        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_JAVA_ADDR")
            && let Ok(addr) = val.parse::<SocketAddr>()
        {
            cfg.java_addr = Some(addr);
        }
        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_SCENARIOS")
            && !val.trim().is_empty()
        {
            cfg.scenarios_path = Some(PathBuf::from(val));
        }
        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_WORLD")
            && !val.trim().is_empty()
        {
            cfg.world_save = Some(PathBuf::from(val));
        }
        if let Ok(val) = std::env::var("PUMPKIN_LOCKSTEP_TICKS")
            && let Ok(ticks) = val.parse::<usize>()
        {
            cfg.duration_ticks = Some(ticks);
        }

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dump-lockstep" => {
                    if let Some(dir) = args.next() {
                        cfg.dump_dir = Some(PathBuf::from(dir));
                    }
                }
                "--lockstep-baseline" => {
                    if let Some(dir) = args.next() {
                        cfg.baseline_dir = Some(PathBuf::from(dir));
                    }
                }
                "--strict-lockstep" => {
                    cfg.strict_parity = true;
                }
                "--only-lockstep" => {
                    cfg.only_lockstep = true;
                }
                "--lockstep-java-jar" => {
                    if let Some(jar) = args.next() {
                        cfg.java_jar = Some(PathBuf::from(jar));
                    }
                }
                "--lockstep-java-addr" => {
                    if let Some(addr_str) = args.next()
                        && let Ok(addr) = addr_str.parse::<SocketAddr>()
                    {
                        cfg.java_addr = Some(addr);
                    }
                }
                "--lockstep-scenarios" => {
                    if let Some(path) = args.next() {
                        cfg.scenarios_path = Some(PathBuf::from(path));
                    }
                }
                "--lockstep-world" => {
                    if let Some(dir) = args.next() {
                        cfg.world_save = Some(PathBuf::from(dir));
                    }
                }
                "--lockstep-ticks" => {
                    if let Some(t_str) = args.next()
                        && let Ok(ticks) = t_str.parse::<usize>()
                    {
                        cfg.duration_ticks = Some(ticks);
                    }
                }
                _ => {}
            }
        }

        if cfg.baseline_dir.is_none() {
            let default_dir = default_lockstep_baseline_dir();
            if default_dir.exists() {
                cfg.baseline_dir = Some(default_dir);
            }
        }

        cfg
    }
}

#[must_use]
pub fn default_lockstep_baseline_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/tests/lockstep_parity")
}

#[must_use]
pub fn scenario_id_to_filename(scenario_id: &str) -> String {
    let sanitized: String = scenario_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{sanitized}.json")
}

fn compute_step_hash(
    primary_packets: &[RecordedClientPacket],
    observer_packets: &[RecordedClientPacket],
    snapshot: &LockstepWorldSnapshot,
) -> String {
    let mut hasher = DefaultHasher::new();
    if let Ok(s) = serde_json::to_string(&(primary_packets, observer_packets, snapshot)) {
        s.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Runs a single scenario across two servers in lockstep:
/// - If `external_java` is `Some`, runs `peer_a` (`InProcessPumpkinPeer`) and `ExternalJavaPeer` in lockstep.
/// - Otherwise, runs `peer_a` and `peer_b` (`InProcessPumpkinPeer` instances) in lockstep.
#[expect(
    clippy::too_many_lines,
    reason = "Executes tick-by-tick dual-server lockstep loop and compares snapshots"
)]
pub async fn run_scenario_lockstep(
    scenario: &LockstepScenario,
    peer_a: &mut InProcessPumpkinPeer,
    peer_b: Option<&mut InProcessPumpkinPeer>,
    mut external_java: Option<(&mut ExternalJavaServerProcess, &mut ExternalJavaPeer)>,
) -> Result<(LockstepScenarioRecording, Vec<LockstepStepDiff>), String> {
    peer_a.reset_for_scenario(scenario).await?;

    let effective_steps = scenario.effective_steps();
    let mut step_recordings = Vec::with_capacity(effective_steps.len());
    let mut peer_mismatches_by_step: Vec<(usize, String, Vec<String>)> = Vec::new();

    if let Some((java_proc, java_peer)) = external_java.as_mut() {
        java_peer.reset_for_scenario(java_proc, scenario).await?;

        for step_spec in &effective_steps {
            let (prim_a, obs_a, snap_a) = peer_a
                .step_tick(&step_spec.actions, &scenario.watched_offsets)
                .await?;
            let (prim_java, obs_java) = java_peer.step_tick(java_proc, &step_spec.actions).await?;

            let mut java_step_diffs = Vec::new();
            diff_packet_streams("PrimaryBot", &prim_java, &prim_a, &mut java_step_diffs);
            diff_packet_streams("ObserverBot", &obs_java, &obs_a, &mut java_step_diffs);
            if !java_step_diffs.is_empty() {
                peer_mismatches_by_step.push((
                    step_spec.step,
                    step_spec.label.clone(),
                    java_step_diffs,
                ));
            }

            let state_hash = compute_step_hash(&prim_a, &obs_a, &snap_a);
            step_recordings.push(LockstepStepRecording {
                step: step_spec.step,
                label: step_spec.label.clone(),
                state_hash,
                primary_packets: prim_a,
                observer_packets: obs_a,
                world_snapshot: snap_a,
            });
        }
    } else if let Some(peer_b_ref) = peer_b {
        peer_b_ref.reset_for_scenario(scenario).await?;

        for step_spec in &effective_steps {
            let ((prim_a, obs_a, snap_a), (prim_b, obs_b, snap_b)) = tokio::try_join!(
                peer_a.step_tick(&step_spec.actions, &scenario.watched_offsets),
                peer_b_ref.step_tick(&step_spec.actions, &scenario.watched_offsets)
            )?;

            let mut step_peer_diffs = Vec::new();
            diff_packet_streams(
                "PrimaryBot (PeerB vs PeerA)",
                &prim_b,
                &prim_a,
                &mut step_peer_diffs,
            );
            diff_packet_streams(
                "ObserverBot (PeerB vs PeerA)",
                &obs_b,
                &obs_a,
                &mut step_peer_diffs,
            );
            diff_world_snapshots("PeerB", &snap_b, "PeerA", &snap_a, &mut step_peer_diffs);

            if !step_peer_diffs.is_empty() {
                peer_mismatches_by_step.push((
                    step_spec.step,
                    step_spec.label.clone(),
                    step_peer_diffs,
                ));
            }

            let state_hash = compute_step_hash(&prim_a, &obs_a, &snap_a);
            step_recordings.push(LockstepStepRecording {
                step: step_spec.step,
                label: step_spec.label.clone(),
                state_hash,
                primary_packets: prim_a,
                observer_packets: obs_a,
                world_snapshot: snap_a,
            });
        }
    }

    let recording = LockstepScenarioRecording {
        scenario_id: scenario.id.clone(),
        category: scenario.category.clone(),
        description: scenario.description.clone(),
        total_steps: step_recordings.len(),
        steps: step_recordings,
    };

    let java_divergences = evaluate_vanilla_java_expectations(scenario, &recording);
    let mut diffs: Vec<LockstepStepDiff> = Vec::new();

    if external_java.is_some() {
        for (step_idx, label, java_diffs) in peer_mismatches_by_step {
            diffs.push(LockstepStepDiff {
                scenario_id: scenario.id.clone(),
                step: step_idx,
                label,
                peer_lockstep_mismatches: Vec::new(),
                baseline_mismatches: Vec::new(),
                java_parity_divergences: java_diffs,
            });
        }
    } else {
        for (step_idx, label, peer_diffs) in peer_mismatches_by_step {
            diffs.push(LockstepStepDiff {
                scenario_id: scenario.id.clone(),
                step: step_idx,
                label,
                peer_lockstep_mismatches: peer_diffs,
                baseline_mismatches: Vec::new(),
                java_parity_divergences: Vec::new(),
            });
        }
    }

    for (step_idx, msg) in java_divergences {
        if let Some(existing) = diffs.iter_mut().find(|d| d.step == step_idx) {
            existing.java_parity_divergences.push(msg);
        } else {
            let label = recording
                .steps
                .get(step_idx)
                .map_or_else(|| format!("step_{step_idx}"), |s| s.label.clone());
            diffs.push(LockstepStepDiff {
                scenario_id: scenario.id.clone(),
                step: step_idx,
                label,
                peer_lockstep_mismatches: Vec::new(),
                baseline_mismatches: Vec::new(),
                java_parity_divergences: vec![msg],
            });
        }
    }

    Ok((recording, diffs))
}

#[must_use]
pub fn compare_against_baseline(
    expected: &LockstepScenarioRecording,
    actual: &LockstepScenarioRecording,
) -> Vec<(usize, String)> {
    let mut mismatches = Vec::new();
    if expected.steps.len() != actual.steps.len() {
        mismatches.push((
            0,
            format!(
                "Step count mismatch: baseline={} vs actual={}",
                expected.steps.len(),
                actual.steps.len()
            ),
        ));
    }

    let common = expected.steps.len().min(actual.steps.len());
    for idx in 0..common {
        let exp_s = &expected.steps[idx];
        let act_s = &actual.steps[idx];
        let mut step_msgs = Vec::new();
        diff_packet_streams(
            "PrimaryBot",
            &exp_s.primary_packets,
            &act_s.primary_packets,
            &mut step_msgs,
        );
        diff_packet_streams(
            "ObserverBot",
            &exp_s.observer_packets,
            &act_s.observer_packets,
            &mut step_msgs,
        );
        diff_world_snapshots(
            "Baseline",
            &exp_s.world_snapshot,
            "Actual",
            &act_s.world_snapshot,
            &mut step_msgs,
        );
        for m in step_msgs {
            mismatches.push((exp_s.step, m));
        }
    }
    mismatches
}

fn diff_packet_streams(
    stream_name: &str,
    expected: &[RecordedClientPacket],
    actual: &[RecordedClientPacket],
    out: &mut Vec<String>,
) {
    if expected == actual {
        return;
    }
    let exp_kinds: Vec<&str> = expected
        .iter()
        .map(RecordedClientPacket::packet_kind)
        .collect();
    let act_kinds: Vec<&str> = actual
        .iter()
        .map(RecordedClientPacket::packet_kind)
        .collect();
    if exp_kinds != act_kinds {
        out.push(format!(
            "{stream_name} packet sequence differs: expected {exp_kinds:?}, got {act_kinds:?}"
        ));
    }
    let min_len = expected.len().min(actual.len());
    for i in 0..min_len {
        if expected[i] != actual[i] {
            out.push(format!(
                "{stream_name} packet[{i}] payload mismatch: expected {:?}, got {:?}",
                expected[i], actual[i]
            ));
        }
    }
}

fn diff_world_snapshots(
    exp_label: &str,
    expected: &LockstepWorldSnapshot,
    act_label: &str,
    actual: &LockstepWorldSnapshot,
    out: &mut Vec<String>,
) {
    if expected.player_pos != actual.player_pos {
        out.push(format!(
            "player_pos mismatch: {exp_label}={:?} vs {act_label}={:?}",
            expected.player_pos, actual.player_pos
        ));
    }
    if expected.on_ground != actual.on_ground
        || expected.sprinting != actual.sprinting
        || expected.sneaking != actual.sneaking
        || expected.flying != actual.flying
    {
        out.push(format!(
            "player flags mismatch: {exp_label}=(ground={},sprint={},sneak={},fly={}) vs {act_label}=(ground={},sprint={},sneak={},fly={})",
            expected.on_ground,
            expected.sprinting,
            expected.sneaking,
            expected.flying,
            actual.on_ground,
            actual.sprinting,
            actual.sneaking,
            actual.flying
        ));
    }
    if expected.gamemode != actual.gamemode {
        out.push(format!(
            "gamemode mismatch: {exp_label}='{}' vs {act_label}='{}'",
            expected.gamemode, actual.gamemode
        ));
    }
    if expected.selected_slot != actual.selected_slot {
        out.push(format!(
            "selected_slot mismatch: {exp_label}={} vs {act_label}={}",
            expected.selected_slot, actual.selected_slot
        ));
    }
    if expected.inventory != actual.inventory {
        out.push(format!(
            "inventory mismatch: {exp_label}={:?} vs {act_label}={:?}",
            expected.inventory, actual.inventory
        ));
    }
    if expected.watched_blocks != actual.watched_blocks {
        out.push(format!(
            "watched_blocks mismatch: {exp_label}={:?} vs {act_label}={:?}",
            expected.watched_blocks, actual.watched_blocks
        ));
    }
}

#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "Evaluates all Vanilla Java 26.3 protocol and state expectation variants"
)]
pub fn evaluate_vanilla_java_expectations(
    scenario: &LockstepScenario,
    recording: &LockstepScenarioRecording,
) -> Vec<(usize, String)> {
    let mut divergences = Vec::new();

    for check in &scenario.java_expectations {
        match check {
            VanillaJavaExpectation::ExpectPacketKind {
                step,
                max_step,
                bot,
                packet_kind,
                description,
            } => {
                let mut found = false;
                let end = (*max_step).min(recording.steps.len().saturating_sub(1));
                for s_idx in *step..=end {
                    if let Some(step_rec) = recording.steps.get(s_idx) {
                        let pkts = match bot {
                            BotTarget::Primary => &step_rec.primary_packets,
                            BotTarget::Observer => &step_rec.observer_packets,
                        };
                        if pkts.iter().any(|p| p.packet_kind() == packet_kind) {
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    let actual_kinds: Vec<String> = (*step..=end)
                        .filter_map(|s_idx| recording.steps.get(s_idx))
                        .flat_map(|sr| match bot {
                            BotTarget::Primary => &sr.primary_packets,
                            BotTarget::Observer => &sr.observer_packets,
                        })
                        .map(|p| p.packet_kind().to_string())
                        .collect();
                    divergences.push((
                        *step,
                        format!(
                            "{description}: expected packet '{packet_kind}' on {bot:?} in steps {step}..={max_step}, got {actual_kinds:?}"
                        ),
                    ));
                }
            }
            VanillaJavaExpectation::ExpectSameTickBlockChangedAck {
                step,
                sequence,
                description,
            } => {
                let has_same_tick_ack = recording.steps.get(*step).is_some_and(|sr| {
                    sr.primary_packets.iter().any(|p| {
                        matches!(
                            p,
                            RecordedClientPacket::BlockChangedAck { sequence: seq } if *seq == *sequence
                        )
                    })
                });
                if !has_same_tick_ack {
                    let next_tick_ack = recording.steps.get(*step + 1).is_some_and(|sr| {
                        sr.primary_packets.iter().any(|p| {
                            matches!(
                                p,
                                RecordedClientPacket::BlockChangedAck { sequence: seq } if *seq == *sequence
                            )
                        })
                    });
                    let detail = if next_tick_ack {
                        format!(
                            "{description}: Vanilla Java sends BlockChangedAck({sequence}) at end of step {step}, but Pumpkin sent it 1 tick later on step {}",
                            step + 1
                        )
                    } else {
                        format!(
                            "{description}: Vanilla Java sends BlockChangedAck({sequence}) on step {step}, but Pumpkin did not emit it on step {step}"
                        )
                    };
                    divergences.push((*step, detail));
                }
            }
            VanillaJavaExpectation::ExpectBlockStatePrefix {
                step,
                offset,
                expected_state_prefix,
                description,
            } => {
                if let Some(sr) = recording.steps.get(*step) {
                    let actual_state = sr
                        .world_snapshot
                        .watched_blocks
                        .iter()
                        .find(|b| b.pos == *offset)
                        .map_or("<unwatched>", |b| b.state.as_str());
                    if !actual_state.starts_with(expected_state_prefix.as_str()) {
                        divergences.push((
                            *step,
                            format!(
                                "{description}: expected block at {offset:?} to start with '{expected_state_prefix}', got '{actual_state}'"
                            ),
                        ));
                    }
                }
            }
            VanillaJavaExpectation::ExpectBlockStateContains {
                step,
                offset,
                expected_substring,
                description,
            } => {
                if let Some(sr) = recording.steps.get(*step) {
                    let actual_state = sr
                        .world_snapshot
                        .watched_blocks
                        .iter()
                        .find(|b| b.pos == *offset)
                        .map_or("<unwatched>", |b| b.state.as_str());
                    if !actual_state.contains(expected_substring.as_str()) {
                        divergences.push((
                            *step,
                            format!(
                                "{description}: expected block at {offset:?} to contain '{expected_substring}', got '{actual_state}'"
                            ),
                        ));
                    }
                }
            }
            VanillaJavaExpectation::ExpectPlayerGamemode {
                step,
                expected_gamemode,
                description,
            } => {
                if let Some(sr) = recording.steps.get(*step)
                    && &sr.world_snapshot.gamemode != expected_gamemode
                {
                    divergences.push((
                        *step,
                        format!(
                            "{description}: expected gamemode '{expected_gamemode}', got '{}'",
                            sr.world_snapshot.gamemode
                        ),
                    ));
                }
            }
            VanillaJavaExpectation::ExpectPlayerFlags {
                step,
                sprinting,
                sneaking,
                flying,
                description,
            } => {
                if let Some(sr) = recording.steps.get(*step) {
                    let ws = &sr.world_snapshot;
                    if let Some(exp_sprint) = sprinting
                        && ws.sprinting != *exp_sprint
                    {
                        divergences.push((
                            *step,
                            format!(
                                "{description}: expected sprinting={exp_sprint}, got {}",
                                ws.sprinting
                            ),
                        ));
                    }
                    if let Some(exp_sneak) = sneaking
                        && ws.sneaking != *exp_sneak
                    {
                        divergences.push((
                            *step,
                            format!(
                                "{description}: expected sneaking={exp_sneak}, got {}",
                                ws.sneaking
                            ),
                        ));
                    }
                    if let Some(exp_fly) = flying
                        && ws.flying != *exp_fly
                    {
                        divergences.push((
                            *step,
                            format!(
                                "{description}: expected flying={exp_fly}, got {}",
                                ws.flying
                            ),
                        ));
                    }
                }
            }
            VanillaJavaExpectation::ExpectSelectedSlot {
                step,
                slot,
                description,
            } => {
                if let Some(sr) = recording.steps.get(*step)
                    && sr.world_snapshot.selected_slot != *slot
                {
                    divergences.push((
                        *step,
                        format!(
                            "{description}: expected selected_slot={slot}, got {}",
                            sr.world_snapshot.selected_slot
                        ),
                    ));
                }
            }
            VanillaJavaExpectation::ExpectInventoryHasItem {
                step,
                item,
                min_count,
                description,
            } => {
                if let Some(sr) = recording.steps.get(*step) {
                    let total_count: u32 = sr
                        .world_snapshot
                        .inventory
                        .iter()
                        .filter(|s| &s.item.item == item)
                        .map(|s| u32::from(s.item.count))
                        .sum();
                    if total_count < u32::from(*min_count) {
                        divergences.push((
                            *step,
                            format!(
                                "{description}: expected inventory to have at least {min_count}x {item}, got {total_count} (inventory: {:?})",
                                sr.world_snapshot.inventory
                            ),
                        ));
                    }
                }
            }
            VanillaJavaExpectation::ExpectSpawnEntity {
                step,
                max_step,
                bot,
                entity_type,
                description,
            } => {
                let mut found = false;
                let end = (*max_step).min(recording.steps.len().saturating_sub(1));
                for s_idx in *step..=end {
                    if let Some(sr) = recording.steps.get(s_idx) {
                        let pkts = match bot {
                            BotTarget::Primary => &sr.primary_packets,
                            BotTarget::Observer => &sr.observer_packets,
                        };
                        if pkts.iter().any(|p| {
                            matches!(
                                p,
                                RecordedClientPacket::SpawnEntity { entity_type: et, .. } if et == entity_type
                            )
                        }) {
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    divergences.push((
                        *step,
                        format!(
                            "{description}: expected SpawnEntity({entity_type}) on {bot:?} in steps {step}..={max_step}, but none arrived"
                        ),
                    ));
                }
            }
        }
    }

    divergences
}

/// Runs the full Dual-Server Lockstep Harness across all scenarios.
#[expect(
    clippy::print_stdout,
    clippy::too_many_lines,
    reason = "Orchestrates persistent lockstep peers, baseline comparison, and summary reporting"
)]
pub async fn run_all_lockstep_tests(
    cfg: &LockstepCliConfig,
) -> Result<LockstepComplianceReport, String> {
    let mut scenarios = if let Some(path) = &cfg.scenarios_path {
        load_scenarios_from_path(path)?
    } else if let Some(world_dir) = &cfg.world_save {
        vec![scenario_from_world_save(
            world_dir.clone(),
            cfg.duration_ticks.unwrap_or(20),
        )]
    } else {
        all_lockstep_scenarios()
    };
    if let Some(ticks) = cfg.duration_ticks {
        for sc in &mut scenarios {
            sc.duration_ticks = sc.duration_ticks.max(ticks);
        }
    }
    let world_save_path = cfg
        .world_save
        .as_deref()
        .or_else(|| scenarios.first().and_then(|s| s.world_save.as_deref()));

    let mut report = LockstepComplianceReport {
        total_scenarios: scenarios.len(),
        ..Default::default()
    };

    if let Some(dump_dir) = &cfg.dump_dir {
        std::fs::create_dir_all(dump_dir).map_err(|e| {
            format!(
                "Failed to create lockstep dump directory {}: {e}",
                dump_dir.display()
            )
        })?;
    }

    let mut external_java = if cfg.java_jar.is_some() || cfg.java_addr.is_some() {
        println!("--- Starting External Java Edition Server for Live Dual-Server Lockstep ---");
        let mut proc = ExternalJavaServerProcess::connect_or_spawn(
            cfg.java_jar.as_deref(),
            cfg.java_addr,
            world_save_path,
        )
        .await?;
        let peer = ExternalJavaPeer::boot(&mut proc, DEFAULT_LOCKSTEP_ORIGIN).await?;
        Some((proc, peer))
    } else {
        None
    };

    let mut peer_a =
        InProcessPumpkinPeer::boot_with_world(DEFAULT_LOCKSTEP_ORIGIN, world_save_path).await?;
    let mut peer_b = if external_java.is_none() {
        Some(InProcessPumpkinPeer::boot_with_world(DEFAULT_LOCKSTEP_ORIGIN, world_save_path).await?)
    } else {
        None
    };

    for scenario in &scenarios {
        println!(
            "--- Running Lockstep Scenario: {} ({}) ---",
            scenario.id, scenario.category
        );
        let (recording, mut step_diffs) = run_scenario_lockstep(
            scenario,
            &mut peer_a,
            peer_b.as_mut(),
            external_java.as_mut().map(|(proc, peer)| (proc, peer)),
        )
        .await?;

        if let Some(dump_dir) = &cfg.dump_dir {
            let path = dump_dir.join(scenario_id_to_filename(&scenario.id));
            let json = recording
                .to_pretty_json()
                .map_err(|e| format!("Failed to serialize lockstep recording: {e}"))?;
            std::fs::write(&path, json)
                .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        }

        // Compare against committed baseline JSON if available
        let mut baseline_matched = false;
        if let Some(baseline_dir) = &cfg.baseline_dir {
            let path = baseline_dir.join(scenario_id_to_filename(&scenario.id));
            if path.exists() {
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| format!("Failed to read baseline {}: {e}", path.display()))?;
                let expected = LockstepScenarioRecording::from_json_str(&raw)
                    .map_err(|e| format!("Failed to parse baseline {}: {e}", path.display()))?;
                let base_mismatches = compare_against_baseline(&expected, &recording);
                if base_mismatches.is_empty() {
                    baseline_matched = true;
                } else {
                    for (step_idx, msg) in base_mismatches {
                        if let Some(existing) = step_diffs.iter_mut().find(|d| d.step == step_idx) {
                            existing.baseline_mismatches.push(msg);
                        } else {
                            let label = recording
                                .steps
                                .get(step_idx)
                                .map_or_else(|| format!("step_{step_idx}"), |s| s.label.clone());
                            step_diffs.push(LockstepStepDiff {
                                scenario_id: scenario.id.clone(),
                                step: step_idx,
                                label,
                                peer_lockstep_mismatches: Vec::new(),
                                baseline_mismatches: vec![msg],
                                java_parity_divergences: Vec::new(),
                            });
                        }
                    }
                }
            } else {
                report.missing_baseline_scenarios.push(scenario.id.clone());
            }
        }

        let peer_ok = step_diffs
            .iter()
            .all(|d| d.peer_lockstep_mismatches.is_empty());
        let java_ok = step_diffs
            .iter()
            .all(|d| d.java_parity_divergences.is_empty());

        if peer_ok {
            report.peer_deterministic_scenarios += 1;
        }
        if baseline_matched {
            report.baseline_compliant_scenarios += 1;
        }
        if java_ok {
            report.java_compliant_scenarios += 1;
            println!("--- Lockstep Scenario {}: [COMPLIANT] ---", scenario.id);
        } else {
            report.java_diverged_scenarios += 1;
            println!(
                "--- Lockstep Scenario {}: [DIVERGED (Java vs Pumpkin)] ---",
                scenario.id
            );
        }

        for diff in step_diffs {
            if !diff.is_empty() {
                report.scenario_diffs.push(diff);
            }
        }
    }

    peer_a.shutdown().await;
    if let Some(pb) = peer_b {
        pb.shutdown().await;
    }

    println!("\n{}", report.format_summary());

    if cfg.strict_parity && report.java_diverged_scenarios > 0 {
        return Err(format!(
            "Strict lockstep parity failed: {} scenario(s) diverged from Vanilla Java",
            report.java_diverged_scenarios
        ));
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockstep::scenarios::load_structure_template;

    #[test]
    fn scenario_ids_are_unique_and_well_formed() {
        let scenarios = all_lockstep_scenarios();
        assert!(scenarios.len() >= 35);
        let mut ids = std::collections::BTreeSet::new();
        for sc in &scenarios {
            assert!(
                ids.insert(sc.id.as_str()),
                "Duplicate lockstep scenario ID: {}",
                sc.id
            );
            assert!(
                !sc.effective_steps().is_empty(),
                "Scenario {} has no steps",
                sc.id
            );
            if let Some(struct_id) = &sc.structure {
                let res = load_structure_template(struct_id);
                assert!(res.is_ok(), "Structure {struct_id} failed to load: {res:?}");
                if let Ok(tpl) = res {
                    assert!(tpl.block_count() > 0);
                }
            }
        }
    }

    #[test]
    fn passive_duration_ticks_scenario_expands_automatically() {
        let sc = scenario_from_world_save(PathBuf::from("/tmp/test_world"), 15);
        let steps = sc.effective_steps();
        assert_eq!(steps.len(), 15);
        assert_eq!(steps[0].step, 0);
        assert_eq!(steps[14].step, 14);
        assert!(steps.iter().all(|s| s.actions.is_empty()));
    }
}
