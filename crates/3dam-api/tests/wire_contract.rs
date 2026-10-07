use dam_api::accounts::{Role, ShareAccess, ShareResource};
use dam_api::admin::{
    AuthMode, CacheTarget, FlagKey, FlagValue, McpMode, OidcProvisioning, SetFlag, SetFlagReply,
};
use dam_api::dto::*;
use dam_api::event::{ChangeKind, EventTopic, JobEvent, LibraryEvent, SubscribeRequest};
use dam_api::page::Page;
use dam_api::service::{Scope, WhoAmI};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

fn contract() -> Value {
    serde_json::from_str(include_str!("../../../contracts/api-v1.json")).unwrap()
}

macro_rules! assert_wire_enum {
    ($fixture:literal, $ty:ty, $($variant:path => $wire:literal),+ $(,)?) => {{
        fn wire_name(value: $ty) -> &'static str {
            match value {
                $($variant => $wire),+
            }
        }
        let actual = [$($variant),+]
            .into_iter()
            .map(|value| {
                assert_eq!(serde_json::to_value(value).unwrap(), Value::String(wire_name(value).into()));
                wire_name(value)
            })
            .collect::<Vec<_>>();
        assert_eq!(serde_json::to_value(actual).unwrap(), contract()["fieldless_enums"][$fixture]);
    }};
}

fn assert_round_trips<T: DeserializeOwned + Serialize>(values: &Value) {
    for value in values.as_array().unwrap() {
        let decoded: T = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), *value);
    }
}

fn guard_origin(value: &Origin) {
    match value {
        Origin::Local | Origin::Peer(_) => {}
    }
}

fn guard_media_attributes(value: &MediaAttributes) {
    match value {
        MediaAttributes::Audio(_)
        | MediaAttributes::Image(_)
        | MediaAttributes::Model(_)
        | MediaAttributes::Video(_)
        | MediaAttributes::Document(_)
        | MediaAttributes::None => {}
    }
}

fn guard_filter_value(value: &FilterValue) {
    match value {
        FilterValue::Str(_)
        | FilterValue::Num(_)
        | FilterValue::Bool(_)
        | FilterValue::Range(_, _)
        | FilterValue::List(_) => {}
    }
}

fn guard_convert_target(value: &ConvertTarget) {
    match value {
        ConvertTarget::Image { .. } | ConvertTarget::Audio { .. } | ConvertTarget::Model { .. } => {
        }
    }
}

fn guard_job_result(value: &JobResult) {
    match value {
        JobResult::Convert(_) | JobResult::Export(_) => {}
    }
}

fn guard_source_state(value: &SourceState) {
    match value {
        SourceState::Online
        | SourceState::Offline
        | SourceState::Scanning
        | SourceState::Error(_) => {}
    }
}

fn guard_library_event(value: &LibraryEvent) {
    match value {
        LibraryEvent::AssetAdded(_)
        | LibraryEvent::AssetChanged { .. }
        | LibraryEvent::AssetRemoved { .. }
        | LibraryEvent::SourceState { .. }
        | LibraryEvent::JobProgress(_)
        | LibraryEvent::StreamLagged
        | LibraryEvent::CatalogReset => {}
    }
}

fn guard_job_event(value: &JobEvent) {
    match value {
        JobEvent::Progress(_)
        | JobEvent::Warning(_)
        | JobEvent::Done(_)
        | JobEvent::Failed { .. } => {}
    }
}

fn guard_flag_value(value: &FlagValue) {
    match value {
        FlagValue::Auth(_) | FlagValue::Mcp(_) | FlagValue::Bool(_) => {}
    }
}

#[test]
fn fieldless_enums_match_the_committed_wire_vocabulary() {
    assert_wire_enum!("media_type", MediaType, MediaType::Audio => "audio", MediaType::Image => "image", MediaType::Model => "model", MediaType::Video => "video", MediaType::Document => "document");
    assert_wire_enum!("license_status", LicenseStatus, LicenseStatus::Permissive => "permissive", LicenseStatus::Attribution => "attribution", LicenseStatus::Restricted => "restricted", LicenseStatus::Unknown => "unknown");
    assert_wire_enum!("search_mode", SearchMode, SearchMode::Lexical => "lexical", SearchMode::Hybrid => "hybrid", SearchMode::Semantic => "semantic");
    assert_wire_enum!("facet_field", FacetField,
        FacetField::MediaType => "media_type", FacetField::Format => "format", FacetField::Source => "source", FacetField::Tag => "tag", FacetField::SizeBytes => "size_bytes", FacetField::License => "license", FacetField::UsageRight => "usage_right",
        FacetField::Width => "width", FacetField::Height => "height", FacetField::ColorDepth => "color_depth", FacetField::HasAlpha => "has_alpha", FacetField::ColorSpace => "color_space", FacetField::ImageClass => "image_class", FacetField::Tileability => "tileability", FacetField::TileClass => "tile_class",
        FacetField::Bpm => "bpm", FacetField::Duration => "duration", FacetField::SampleRate => "sample_rate", FacetField::BitDepth => "bit_depth", FacetField::Channels => "channels", FacetField::MusicalKey => "musical_key", FacetField::Loudness => "loudness", FacetField::Brightness => "brightness", FacetField::Harmonicity => "harmonicity", FacetField::AudioClass => "audio_class", FacetField::Codec => "codec", FacetField::Container => "container",
        FacetField::TriCount => "tri_count", FacetField::VertexCount => "vertex_count", FacetField::MeshCount => "mesh_count", FacetField::MaterialCount => "material_count", FacetField::TextureCount => "texture_count", FacetField::DependencyBytes => "dependency_bytes", FacetField::HasRig => "has_rig", FacetField::HasAnimation => "has_animation", FacetField::HasUv => "has_uv", FacetField::ModelClass => "model_class",
        FacetField::Fps => "fps", FacetField::Bitrate => "bitrate", FacetField::HasAudio => "has_audio", FacetField::VideoClass => "video_class", FacetField::PageCount => "page_count", FacetField::WordCount => "word_count", FacetField::Author => "author", FacetField::DocumentClass => "document_class", FacetField::Favorite => "favorite", FacetField::Path => "path", FacetField::Folder => "folder");
    assert_wire_enum!("filter_op", FilterOp, FilterOp::Eq => "eq", FilterOp::Ne => "ne", FilterOp::Lt => "lt", FilterOp::Lte => "lte", FilterOp::Gt => "gt", FilterOp::Gte => "gte", FilterOp::In => "in", FilterOp::Range => "range", FilterOp::Contains => "contains", FilterOp::Exists => "exists");
    assert_wire_enum!("sort_field", SortField, SortField::Name => "name", SortField::Size => "size", SortField::Scanned => "scanned", SortField::Relevance => "relevance");
    assert_wire_enum!("sort_dir", SortDir, SortDir::Asc => "asc", SortDir::Desc => "desc");
    assert_wire_enum!("source_kind", SourceKind, SourceKind::LocalFs => "local_fs", SourceKind::Sftp => "sftp", SourceKind::Smb => "smb", SourceKind::Federated => "federated");
    assert_wire_enum!("scan_mode", ScanMode, ScanMode::Full => "full", ScanMode::Delta => "delta", ScanMode::Quick => "quick");
    assert_wire_enum!("job_kind", JobKind, JobKind::Scan => "scan", JobKind::Enrich => "enrich", JobKind::Analyze => "analyze", JobKind::Convert => "convert", JobKind::Export => "export");
    assert_wire_enum!("job_state", JobState, JobState::Queued => "queued", JobState::Running => "running", JobState::Paused => "paused", JobState::Done => "done", JobState::Failed => "failed", JobState::Cancelled => "cancelled");
    assert_wire_enum!("collision_rule", CollisionRule, CollisionRule::Fail => "fail", CollisionRule::Suffix => "suffix", CollisionRule::Skip => "skip", CollisionRule::Overwrite => "overwrite");
    assert_wire_enum!("disposition", Disposition, Disposition::Write => "write", Disposition::Collision => "collision", Disposition::Skipped => "skipped", Disposition::Unsupported => "unsupported", Disposition::Done => "done", Disposition::Failed => "failed");
    assert_wire_enum!("upload_collision", UploadCollision, UploadCollision::Fail => "fail", UploadCollision::Suffix => "suffix", UploadCollision::Skip => "skip");
    assert_wire_enum!("dup_kind", DupKind, DupKind::Exact => "exact", DupKind::Near => "near");
    assert_wire_enum!("review_action", ReviewAction, ReviewAction::Accept => "accept", ReviewAction::Reject => "reject", ReviewAction::Undo => "undo");
    assert_wire_enum!("suggestion_state", SuggestionState, SuggestionState::Pending => "pending", SuggestionState::Confirmed => "confirmed", SuggestionState::Rejected => "rejected");
    assert_wire_enum!("collection_kind", CollectionKind, CollectionKind::Manual => "manual", CollectionKind::Smart => "smart");
    assert_wire_enum!("export_format", ExportFormat, ExportFormat::Json => "json", ExportFormat::Csv => "csv", ExportFormat::Sidecar => "sidecar");
    assert_wire_enum!("change_kind", ChangeKind, ChangeKind::Reanalyzed => "reanalyzed", ChangeKind::Retagged => "retagged", ChangeKind::LicenseSet => "license_set", ChangeKind::Metadata => "metadata", ChangeKind::NoteSet => "note_set", ChangeKind::Commented => "commented");
    assert_wire_enum!("event_topic", EventTopic, EventTopic::Assets => "assets", EventTopic::Sources => "sources", EventTopic::Jobs => "jobs", EventTopic::Analysis => "analysis");
    assert_wire_enum!("scope", Scope, Scope::Read => "read", Scope::Write => "write", Scope::Admin => "admin", Scope::McpUse => "mcp_use", Scope::Federate => "federate");
    assert_wire_enum!("auth_mode", AuthMode, AuthMode::Off => "off", AuthMode::Anonymous => "anonymous", AuthMode::Token => "token");
    assert_wire_enum!("mcp_mode", McpMode, McpMode::Off => "off", McpMode::ReadOnly => "read_only", McpMode::ReadWrite => "read_write");
    assert_wire_enum!("flag_key", FlagKey, FlagKey::Authentication => "authentication", FlagKey::McpServer => "mcp_server", FlagKey::NetworkWrites => "network_writes", FlagKey::AutoThumbnail => "auto_thumbnail", FlagKey::AutoAnalyze => "auto_analyze", FlagKey::Federation => "federation", FlagKey::UserAccounts => "user_accounts", FlagKey::Upload => "upload", FlagKey::Oidc => "oidc");
    assert_wire_enum!("cache_target", CacheTarget, CacheTarget::Thumbnails => "thumbnails", CacheTarget::Previews => "previews", CacheTarget::All => "all");
    assert_wire_enum!("oidc_provisioning", OidcProvisioning, OidcProvisioning::Linked => "linked", OidcProvisioning::AutoViewer => "auto_viewer", OidcProvisioning::AutoEditor => "auto_editor");
    assert_wire_enum!("account_role", Role, Role::Admin => "admin", Role::Editor => "editor", Role::Viewer => "viewer");
    assert_wire_enum!("share_resource", ShareResource, ShareResource::Source => "source", ShareResource::Collection => "collection");
    assert_wire_enum!("share_access", ShareAccess, ShareAccess::Read => "read", ShareAccess::Write => "write");
}

#[test]
fn tagged_and_untagged_union_examples_round_trip() {
    let variants = &contract()["variant_examples"];
    for value in variants["origin"].as_array().unwrap() {
        guard_origin(&serde_json::from_value(value.clone()).unwrap());
    }
    for value in variants["media_attributes"].as_array().unwrap() {
        guard_media_attributes(&serde_json::from_value(value.clone()).unwrap());
    }
    for value in variants["filter_value"].as_array().unwrap() {
        guard_filter_value(&serde_json::from_value(value.clone()).unwrap());
    }
    for value in variants["convert_target"].as_array().unwrap() {
        guard_convert_target(&serde_json::from_value(value.clone()).unwrap());
    }
    for value in variants["job_result"].as_array().unwrap() {
        guard_job_result(&serde_json::from_value(value.clone()).unwrap());
    }
    for value in variants["source_state"].as_array().unwrap() {
        guard_source_state(&serde_json::from_value(value.clone()).unwrap());
    }
    for value in variants["library_event"].as_array().unwrap() {
        guard_library_event(&serde_json::from_value(value.clone()).unwrap());
    }
    for value in variants["job_event"].as_array().unwrap() {
        guard_job_event(&serde_json::from_value(value.clone()).unwrap());
    }
    for value in variants["flag_value"].as_array().unwrap() {
        guard_flag_value(&serde_json::from_value(value.clone()).unwrap());
    }
    assert_round_trips::<Origin>(&variants["origin"]);
    assert_round_trips::<MediaAttributes>(&variants["media_attributes"]);
    assert_round_trips::<FilterValue>(&variants["filter_value"]);
    assert_round_trips::<ConvertTarget>(&variants["convert_target"]);
    assert_round_trips::<JobResult>(&variants["job_result"]);
    assert_round_trips::<SourceState>(&variants["source_state"]);
    assert_round_trips::<LibraryEvent>(&variants["library_event"]);
    assert_round_trips::<JobEvent>(&variants["job_event"]);
    assert_round_trips::<FlagValue>(&variants["flag_value"]);
}

#[test]
fn representative_dtos_decode_and_preserve_their_wire_shape() {
    let values = &contract()["representatives"];
    let _: QueryRequest = serde_json::from_value(values["query_request_minimal"].clone()).unwrap();
    assert_round_trips::<QueryRequest>(&Value::Array(vec![values["query_request_full"].clone()]));
    assert_round_trips::<AssetSummary>(&Value::Array(vec![values["asset_summary"].clone()]));
    assert_round_trips::<Asset>(&Value::Array(vec![values["asset"].clone()]));
    assert_round_trips::<Page<AssetSummary>>(&Value::Array(vec![values["asset_page"].clone()]));
    assert_round_trips::<JobStatus>(&Value::Array(vec![values["job_status"].clone()]));
    assert_round_trips::<SourceInfo>(&Value::Array(vec![values["source_info"].clone()]));
    assert_round_trips::<Collection>(&Value::Array(vec![values["collection"].clone()]));
    assert_round_trips::<ConvertRequest>(&Value::Array(vec![values["convert_request"].clone()]));
    assert_round_trips::<ConvertReport>(&Value::Array(vec![values["convert_report"].clone()]));
    assert_round_trips::<WhoAmI>(&Value::Array(vec![values["whoami"].clone()]));
    assert_round_trips::<SetFlag>(&Value::Array(vec![values["set_flag"].clone()]));
    assert_round_trips::<SetFlagReply>(&Value::Array(vec![values["set_flag_reply"].clone()]));
    assert_round_trips::<dam_api::error::ErrorBody>(&Value::Array(vec![
        values["error_body"].clone()
    ]));
    assert_round_trips::<SubscribeRequest>(&Value::Array(
        vec![values["subscribe_request"].clone()],
    ));
}

#[test]
fn backwards_compatible_defaults_and_optional_null_are_explicit() {
    let query: QueryRequest = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(query.page.limit, 100);
    assert!(!query.include_facets);
    assert!(query.filters.is_empty());

    let subscription: SubscribeRequest = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(subscription.topics.is_empty());

    let omitted: SetFlag = serde_json::from_value(serde_json::json!({"value": true})).unwrap();
    let explicit: SetFlag = serde_json::from_value(serde_json::json!({
        "value": true, "expected_version": null
    }))
    .unwrap();
    assert_eq!(omitted.expected_version, explicit.expected_version);
}

/// A federated peer is a separately deployed 3dam, so the reader must accept the spelling the
/// previous release wrote. `suggested` was the pre-#112 wire value for what is now `pending`;
/// rejecting it fails the *whole* `Asset` decode, so one tagged asset on a peer that is one
/// release behind turns every detail read into a `404`.
#[test]
fn a_peer_on_the_previous_release_still_decodes() {
    let legacy = serde_json::json!({
        "name": "texture", "state": "suggested", "source": "auto", "confidence": 0.7
    });
    let tag: TagRef = serde_json::from_value(legacy).unwrap();
    assert_eq!(tag.state, SuggestionState::Pending);
    // The alias is read-only: the committed vocabulary is still what goes out on the wire.
    assert_eq!(
        serde_json::to_value(tag.state).unwrap(),
        Value::String("pending".into())
    );
}
