//! TBC sidecar metadata: the structures serialized into the `.tbc.json`
//! document and the conversion from a decoder's per-run metadata.

use serde::{Deserialize, Serialize};
use tape_decode::{DecoderMetadata, FieldInfoEntry};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TbcMetadata {
    pcm_audio_parameters: PcmAudioParameters,
    video_parameters: VideoParameters,
}

/// Full metadata, including the per-field array, as read back from a sidecar.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TbcMetadataFull {
    pub(crate) fields: Vec<FieldInfoEntry>,
    pub(crate) pcm_audio_parameters: PcmAudioParameters,
    pub(crate) video_parameters: VideoParameters,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PcmAudioParameters {
    pub(crate) bits: usize,
    pub(crate) is_little_endian: bool,
    pub(crate) is_signed: bool,
    pub(crate) sample_rate: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoParameters {
    pub(crate) number_of_sequential_fields: usize,
    pub(crate) os_info: String,
    pub(crate) git_branch: String,
    #[serde(default)]
    pub(crate) git_commit: String,
    #[serde(default, rename = "gitRelease")]
    pub(crate) git_release: String,
    pub(crate) system: String,
    #[serde(default)]
    pub(crate) medium: String,
    pub(crate) field_width: usize,
    pub(crate) sample_rate: f64,
    /// Input RF capture sample rate in Hz; written as `rfSourceSampleRate`.
    /// `#[serde(default)]` so sidecars produced before this field existed still
    /// deserialize (compare treats it as provenance and does not diff it).
    #[serde(default)]
    pub(crate) rf_source_sample_rate: f64,
    pub(crate) black_16b_ire: f64,
    pub(crate) white_16b_ire: f64,
    pub(crate) field_height: usize,
    pub(crate) colour_burst_start: i64,
    pub(crate) colour_burst_end: i64,
    pub(crate) active_video_start: i64,
    pub(crate) active_video_end: i64,
    pub(crate) tape_format: String,
}

/// Provenance metadata derived from the selected decode profile name, the
/// build's git state, and the RF source sample rate. Threaded through to
/// [`metadata_to_tbc`] so the JSON sidecar carries a clear Medium / Format /
/// System / git chain plus `rfSourceSampleRate` for post audio alignment.
#[derive(Clone)]
pub(crate) struct MetadataContext {
    pub(crate) medium: String,
    pub(crate) format: String,
    pub(crate) system: String,
    pub(crate) git_branch: String,
    pub(crate) git_commit: String,
    pub(crate) git_release: String,
    /// Input RF capture sample rate in Hz (from `--frequency`), written to the
    /// sidecar as `videoParameters.rfSourceSampleRate`.
    pub(crate) rf_source_sample_rate_hz: f64,
}

/// Recording-speed suffixes that may trail a profile name and are not part of
/// the format (e.g. `SECAM_VHS_LP` -> system "SECAM", format "VHS").
const PROFILE_SPEED_SUFFIXES: &[&str] = &["EP", "LP", "VP"];

/// Parse a profile name of the form `<SYSTEM>_<FORMAT>[_<SPEED>]` into its
/// `(system, format)` pair. The trailing segment is dropped only when it is a
/// recording-speed suffix, so `NTSC_BETAMAX_HIFI` keeps `BETAMAX_HIFI` as the
/// format. A bare name with no `_` yields `(name, "")`.
pub(crate) fn parse_profile_flags(name: &str) -> (&str, &str) {
    let name = name.trim();
    let (system, rest) = match name.split_once('_') {
        Some((s, r)) => (s, r),
        None => (name, ""),
    };
    let format = match rest.rsplit_once('_') {
        Some((head, last)) if PROFILE_SPEED_SUFFIXES.iter().any(|s| *s == last) => head,
        _ => rest,
    };
    (system, format)
}

impl MetadataContext {
    /// Build the metadata context from the selected profile name and the RF
    /// source sample rate. A custom `--profile-file` (no name) yields an
    /// "UNKNOWN" system / "custom" format. `rf_source_sample_rate_hz` is the
    /// input RF capture rate in Hz (e.g. 40 MHz -> 40_000_000.0).
    pub(crate) fn from_profile_name(name: Option<&str>, rf_source_sample_rate_hz: f64) -> Self {
        let (system, format) = match name {
            Some(n) if !n.trim().is_empty() => parse_profile_flags(n),
            _ => ("UNKNOWN", "custom"),
        };
        Self {
            medium: "TAPE".to_string(),
            format: format.to_string(),
            system: system.to_string(),
            // Injected at build time by crates/tape-decode-cli/build.rs.
            git_branch: option_env!("GIT_BRANCH").unwrap_or("UNKNOWN").to_string(),
            git_commit: option_env!("GIT_COMMIT").unwrap_or("UNKNOWN").to_string(),
            git_release: option_env!("GIT_RELEASE").unwrap_or("UNKNOWN").to_string(),
            rf_source_sample_rate_hz,
        }
    }
}

pub(crate) fn metadata_to_tbc(
    metadata: &DecoderMetadata,
    field_count: usize,
    ctx: &MetadataContext,
) -> TbcMetadata {
    TbcMetadata {
        pcm_audio_parameters: PcmAudioParameters {
            bits: 16,
            is_little_endian: true,
            is_signed: true,
            sample_rate: 0,
        },
        video_parameters: VideoParameters {
            number_of_sequential_fields: field_count,
            os_info: String::new(),
            // The JSON `system` / `tapeFormat` / `medium` come from the profile
            // name (the user's "profile list info"), not from DecoderMetadata,
            // so the sidecar records the format the user actually selected.
            git_branch: ctx.git_branch.clone(),
            git_commit: ctx.git_commit.clone(),
            git_release: ctx.git_release.clone(),
            system: ctx.system.clone(),
            medium: ctx.medium.clone(),
            field_width: metadata.field_width,
            sample_rate: metadata.sample_rate,
            rf_source_sample_rate: ctx.rf_source_sample_rate_hz,
            black_16b_ire: metadata.black_16b_ire,
            white_16b_ire: metadata.white_16b_ire,
            field_height: metadata.field_height,
            colour_burst_start: metadata.colour_burst_start,
            colour_burst_end: metadata.colour_burst_end,
            active_video_start: metadata.active_video_start,
            active_video_end: metadata.active_video_end,
            tape_format: ctx.format.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_decoder_metadata() -> DecoderMetadata {
        DecoderMetadata {
            system: "PAL",
            field_width: 1135,
            sample_rate: 17_734_475.0,
            black_16b_ire: 1024.0,
            white_16b_ire: 1024.0,
            field_height: 313,
            colour_burst_start: 0,
            colour_burst_end: 0,
            active_video_start: 0,
            active_video_end: 0,
        }
    }

    #[test]
    fn parse_profile_flags_covers_systems_and_speeds() {
        assert_eq!(parse_profile_flags("NTSC_VHS"), ("NTSC", "VHS"));
        assert_eq!(parse_profile_flags("PAL_VHS"), ("PAL", "VHS"));
        assert_eq!(parse_profile_flags("SECAM_VHS"), ("SECAM", "VHS"));
        assert_eq!(parse_profile_flags("SECAM_VHS_LP"), ("SECAM", "VHS"));
        assert_eq!(parse_profile_flags("MESECAM_VHS_EP"), ("MESECAM", "VHS"));
        assert_eq!(parse_profile_flags("405_BETAMAX"), ("405", "BETAMAX"));
        assert_eq!(parse_profile_flags("819_QUADRUPLEX"), ("819", "QUADRUPLEX"));
        assert_eq!(parse_profile_flags("MPAL_VHS"), ("MPAL", "VHS"));
        assert_eq!(parse_profile_flags("NLINHA_VHS"), ("NLINHA", "VHS"));
        // HIFI / HI are part of the format, not speed suffixes.
        assert_eq!(parse_profile_flags("NTSC_BETAMAX_HIFI"), ("NTSC", "BETAMAX_HIFI"));
        assert_eq!(parse_profile_flags("PAL_UMATIC_HI"), ("PAL", "UMATIC_HI"));
    }

    #[test]
    fn metadata_to_tbc_writes_context_flags() {
        let dm = dummy_decoder_metadata();
        let ctx = MetadataContext::from_profile_name(Some("SECAM_VHS_LP"), 40.0 * 1000_000.0);
        let tbc = metadata_to_tbc(&dm, 42, &ctx);
        assert_eq!(tbc.video_parameters.system, "SECAM");
        assert_eq!(tbc.video_parameters.tape_format, "VHS");
        assert_eq!(tbc.video_parameters.medium, "TAPE");
        assert_eq!(tbc.video_parameters.number_of_sequential_fields, 42);
    }

    #[test]
    fn metadata_context_custom_profile_file() {
        let ctx = MetadataContext::from_profile_name(None, 40.0 * 1000_000.0);
        assert_eq!(ctx.system, "UNKNOWN");
        assert_eq!(ctx.format, "custom");
        assert_eq!(ctx.medium, "TAPE");
    }

    #[test]
    fn metadata_to_tbc_serializes_camel_case_with_git() {
        let dm = dummy_decoder_metadata();
        let ctx = MetadataContext::from_profile_name(Some("405_BETAMAX"), 40.0 * 1000_000.0);
        let tbc = metadata_to_tbc(&dm, 1, &ctx);
        let json = serde_json::to_string(&tbc).expect("serialize");
        // camelCase JSON keys carry the profile-derived flags.
        assert!(json.contains("\"system\":\"405\""), "json: {json}");
        assert!(json.contains("\"tapeFormat\":\"BETAMAX\""), "json: {json}");
        assert!(json.contains("\"medium\":\"TAPE\""), "json: {json}");
        assert!(json.contains("\"gitBranch\":\""), "json: {json}");
        assert!(json.contains("\"gitCommit\":\""), "json: {json}");
        assert!(json.contains("\"gitRelease\":\""), "json: {json}");
        // Built inside a git checkout, so build.rs injected real (non-UNKNOWN) values.
        assert_ne!(ctx.git_branch, "UNKNOWN", "git branch not injected by build.rs");
        assert_ne!(ctx.git_commit, "UNKNOWN", "git commit not injected by build.rs");
        assert_ne!(ctx.git_release, "UNKNOWN", "git release not injected by build.rs");
        assert_ne!(ctx.git_release, "", "git release is empty");
    }

    #[test]
    fn metadata_to_tbc_writes_rf_source_sample_rate_in_hz() {
        let dm = dummy_decoder_metadata();
        // A 40 MHz RF capture is recorded in Hz, matching videoParameters.sampleRate's unit.
        let ctx = MetadataContext::from_profile_name(Some("PAL_VHS"), 40.0 * 1000_000.0);
        let tbc = metadata_to_tbc(&dm, 1, &ctx);
        assert_eq!(tbc.video_parameters.rf_source_sample_rate, 40_000_000.0);
        let json = serde_json::to_string(&tbc).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let rf = value["videoParameters"]["rfSourceSampleRate"]
            .as_f64()
            .expect("rfSourceSampleRate number");
        assert!(
            (rf - 40_000_000.0).abs() < 1e-6,
            "rfSourceSampleRate was {rf}, expected 40000000.0; json: {json}"
        );
    }
}
