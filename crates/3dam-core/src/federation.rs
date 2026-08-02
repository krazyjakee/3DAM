//! The federated query engine (phase 6, issues #39/#40) — tech-spec 07 §4–§6, ADR 0009 §5.
//!
//! Runs **on the querying side**: it fans a library read out to every registered federated peer,
//! merges the returns with the local page, and tags every hit with its origin. A peer is a client
//! of the other instance's ordinary `/api/v1` read surface (via [`ApiClient`]) — it yields catalog
//! rows, never bytes for processing; the only byte transfer here is proxying remote-owned previews.
//!
//! Frozen semantics (ADR 0009 §5): a **2.5 s fixed deadline** per query round — peers past it are
//! dropped from the round and the page is flagged `partial`; `total` is always `None` under
//! fan-out; cursor drift across pages is accepted (no snapshot isolation).

use crate::EmbeddedLibrary;
use dam_api::dto::*;
use dam_api::id::{AssetId, SourceId};
use dam_api::page::{Cursor, ItemWarning, Page, PageParams, PartialStatus};
use dam_api::service::{AuthContext, LibraryService};
use dam_api::{protocol_compatible, LibError, PeerAdvertise, FEDERATION_PROTOCOL_VERSION};
use dam_client::ApiClient;
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// The frozen per-round federated-query deadline (ADR 0009 §5). Fixed, not adaptive, in v1.
pub(crate) const QUERY_DEADLINE: Duration = Duration::from_millis(2500);
/// Byte proxying (thumbnail / preview / content for a peer asset) tolerates more than a query
/// round: the peer may be rendering the derivative on a cache miss.
pub(crate) const PROXY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a built peer list stays fresh before the registry re-reads the source table.
const PEERS_TTL: Duration = Duration::from_secs(30);
/// How long an `advertise()` reply is trusted (protocol version + spaces change ~never).
const ADVERTISE_TTL: Duration = Duration::from_secs(300);
/// Advertise handshake bound at `add_source` time (interactive; fail fast on a typo'd endpoint).
pub(crate) const ADD_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

fn ectx() -> AuthContext {
    AuthContext::embedded()
}

// ── the peer handle + registry ───────────────────────────────────────────────────────────────

/// One registered federated peer: its source row identity + a connected [`ApiClient`] transport.
pub(crate) struct Peer {
    pub source_id: SourceId,
    pub name: String,
    pub client: ApiClient,
    /// `advertise()` reply, cached with [`ADVERTISE_TTL`].
    advertise: Mutex<Option<(Instant, PeerAdvertise)>>,
}

impl Peer {
    pub(crate) fn new(source_id: SourceId, name: String, client: ApiClient) -> Peer {
        Peer {
            source_id,
            name,
            client,
            advertise: Mutex::new(None),
        }
    }

    /// The peer's self-description, fetched at most once per [`ADVERTISE_TTL`].
    pub(crate) async fn advertise(&self) -> Result<PeerAdvertise, LibError> {
        let mut guard = self.advertise.lock().await;
        if let Some((at, ad)) = guard.as_ref() {
            if at.elapsed() < ADVERTISE_TTL {
                return Ok(ad.clone());
            }
        }
        let ad = self.client.advertise().await?;
        *guard = Some((Instant::now(), ad.clone()));
        Ok(ad)
    }
}

pub(crate) type PeerList = Arc<Vec<Arc<Peer>>>;

struct PeerSnapshot {
    peers: PeerList,
    by_source: HashMap<SourceId, Arc<Peer>>,
}

/// Lazily-built, TTL-cached list of federated peers, rebuilt from the source table. Invalidated on
/// source add/remove so a new peer participates in the very next query.
pub(crate) struct PeerRegistry {
    /// The ordered fan-out list and O(1) owner index are published and invalidated atomically.
    cache: Mutex<Option<(Instant, PeerSnapshot)>>,
}

impl PeerRegistry {
    pub(crate) fn new() -> PeerRegistry {
        PeerRegistry {
            cache: Mutex::new(None),
        }
    }
    pub(crate) async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }
}

/// Build an [`ApiClient`] for a federated connection config.
pub(crate) async fn connect_peer(
    endpoint: &str,
    token: Option<String>,
) -> Result<ApiClient, LibError> {
    let url = url::Url::parse(endpoint)
        .map_err(|e| LibError::BadRequest(format!("invalid peer endpoint {endpoint:?}: {e}")))?;
    ApiClient::connect_with_token(url, token).await
}

impl EmbeddedLibrary {
    /// The current federated peers (fail-soft: an unbuildable peer is skipped with a warning log —
    /// it will be retried on the next registry rebuild).
    pub(crate) async fn fed_peers(&self) -> PeerList {
        {
            let guard = self.fed.cache.lock().await;
            if let Some((at, snapshot)) = guard.as_ref() {
                if at.elapsed() < PEERS_TTL {
                    return snapshot.peers.clone();
                }
            }
        }
        let secrets = self.secrets.clone();
        let rows: Vec<(SourceId, String, String, Option<String>)> = self
            .db(move |s| {
                let mut out = Vec::new();
                for info in s.list_sources()? {
                    if info.kind != SourceKind::Federated {
                        continue;
                    }
                    match s
                        .get_source_connection(&info.id)
                        .and_then(|connection| secrets.resolve(connection))
                    {
                        Ok(dam_sources::SourceConnection::Federated(cfg)) => {
                            out.push((info.id, info.name, cfg.endpoint, cfg.token));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = s.set_source_error(&info.id, &error.to_string());
                            tracing::warn!(peer = %info.name, "federated peer credentials unavailable: {error}");
                        }
                    }
                }
                Ok(out)
            })
            .await
            .unwrap_or_default();
        let mut peers = Vec::new();
        for (id, name, endpoint, token) in rows {
            match connect_peer(&endpoint, token).await {
                Ok(client) => peers.push(Arc::new(Peer::new(id, name, client))),
                Err(e) => tracing::warn!(peer = %name, %endpoint, "federated peer skipped: {e}"),
            }
        }
        let peers = Arc::new(peers);
        let by_source = peers
            .iter()
            .map(|peer| (peer.source_id, peer.clone()))
            .collect();
        *self.fed.cache.lock().await = Some((
            Instant::now(),
            PeerSnapshot {
                peers: peers.clone(),
                by_source,
            },
        ));
        peers
    }

    /// Resolve a locally-issued owner hint in O(1). Loading the current snapshot also handles a
    /// just-added source; a missing id is intentionally not treated as a remote address.
    pub(crate) async fn fed_peer(&self, source: SourceId) -> Option<Arc<Peer>> {
        let _ = self.fed_peers().await;
        self.fed
            .cache
            .lock()
            .await
            .as_ref()
            .and_then(|(_, snapshot)| snapshot.by_source.get(&source).cloned())
    }
}

// ── the composite fan-out cursor ─────────────────────────────────────────────────────────────
//
// Each source (the local index + every peer) paginates with its own opaque cursor. The merged
// page consumes *part* of each source's page, so the continuation must remember, per source, the
// cursor the last fetch used plus how many of its items previous merges already consumed. A
// stream's own cursor only advances once its fetched page is fully consumed — un-consumed items
// are re-fetched next page rather than skipped (correctness over bandwidth; pages are ≤500 rows).
// Boundaries may still drift if a source re-ranks between pages — accepted (ADR 0009 §5).

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct StreamPos {
    /// The `after` cursor the stream's current page was fetched at (`None` = its first page).
    #[serde(default)]
    c: Option<String>,
    /// Items already consumed from the front of that page by previous merges.
    #[serde(default)]
    s: usize,
    /// Stream exhausted.
    #[serde(default)]
    d: bool,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct FedCursor {
    l: StreamPos,
    /// Keyed by the peer's source id string.
    p: BTreeMap<String, StreamPos>,
}

const FED_CURSOR_PREFIX: &str = "fed:";

fn decode_fed_cursor(after: &Option<Cursor>) -> FedCursor {
    after
        .as_ref()
        .and_then(|c| c.0.strip_prefix(FED_CURSOR_PREFIX))
        .and_then(|s| serde_json::from_str(s).ok())
        // An unrecognised cursor (e.g. fan-out just became active mid-listing) restarts the
        // listing — drift accepted rather than erroring the page (ADR 0009 §5).
        .unwrap_or_default()
}

fn encode_fed_cursor(fc: &FedCursor) -> Option<Cursor> {
    serde_json::to_string(fc)
        .ok()
        .map(|s| Cursor(format!("{FED_CURSOR_PREFIX}{s}")))
}

/// One source's fetched page, positioned for the merge.
struct StreamPage {
    items: Vec<AssetSummary>,
    /// The position that produced `items` (cursor used + drop count already applied).
    pos: StreamPos,
    /// The continuation the source handed back for this page.
    next: Option<String>,
}

/// Fetch one source's page at `pos`, honouring its consumed-prefix skip. When a previous merge
/// consumed a whole fetched page, this advances through continuations until real items surface.
async fn fetch_stream<Fut>(
    mut pos: StreamPos,
    fetch: impl Fn(Option<Cursor>) -> Fut,
) -> Result<StreamPage, LibError>
where
    Fut: std::future::Future<Output = Result<Page<AssetSummary>, LibError>>,
{
    if pos.d {
        return Ok(StreamPage {
            items: Vec::new(),
            pos,
            next: None,
        });
    }
    loop {
        let page = fetch(pos.c.clone().map(Cursor)).await?;
        let fetched = page.items.len();
        if pos.s >= fetched && fetched > 0 && page.cursor.is_some() {
            // The whole fetched page was consumed by earlier merges — move to its continuation.
            pos.s -= fetched;
            pos.c = page.cursor.map(|c| c.0);
            continue;
        }
        let next = page.cursor.map(|c| c.0);
        let items: Vec<AssetSummary> = page.items.into_iter().skip(pos.s).collect();
        if items.is_empty() && next.is_none() {
            pos.d = true;
        }
        return Ok(StreamPage { items, pos, next });
    }
}

// ── merge / re-rank ─────────────────────────────────────────────────────────────────────────

/// `true` when `a` (at in-stream rank `ai`) merges before `b`. Field sorts compare the field the
/// summary actually carries (name, size); rank-based sorts (relevance, scanned — whose key is not
/// in the summary) interleave by per-source rank position, which preserves each source's own
/// ordering. Ties break deterministically: local before remote (locality is cheaper to open),
/// then stable asset identity — so pagination is stable across pages (tech-spec 07 §6.1).
fn merges_before(
    a: &AssetSummary,
    ai: usize,
    a_local: bool,
    b: &AssetSummary,
    bi: usize,
    b_local: bool,
    sort: &Sort,
) -> bool {
    use std::cmp::Ordering;
    let ord = match sort.field {
        SortField::Name => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.name.cmp(&b.name)),
        SortField::Size => a.size.cmp(&b.size),
        SortField::Scanned | SortField::Relevance => ai.cmp(&bi),
    };
    let ord =
        if matches!(sort.field, SortField::Name | SortField::Size) && sort.dir == SortDir::Desc {
            ord.reverse()
        } else {
            ord
        };
    match ord {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => match (a_local, b_local) {
            (true, false) => true,
            (false, true) => false,
            _ => a.id.to_string() <= b.id.to_string(),
        },
    }
}

/// K-way merge of the fetched pages into one ordered page of at most `limit` items. Returns the
/// merged items and how many each input stream contributed (to advance its cursor).
fn merge_pages(
    streams: &[(bool, &[AssetSummary])], // (is_local, items)
    sort: &Sort,
    limit: usize,
) -> (Vec<AssetSummary>, Vec<usize>) {
    let mut idx = vec![0usize; streams.len()];
    let mut out = Vec::with_capacity(limit);
    while out.len() < limit {
        let mut best: Option<usize> = None;
        for (si, (is_local, items)) in streams.iter().enumerate() {
            let Some(item) = items.get(idx[si]) else {
                continue;
            };
            best = match best {
                None => Some(si),
                Some(bi_s) => {
                    let (b_local, b_items) = streams[bi_s];
                    let b_item = &b_items[idx[bi_s]];
                    if merges_before(item, idx[si], *is_local, b_item, idx[bi_s], b_local, sort) {
                        Some(si)
                    } else {
                        Some(bi_s)
                    }
                }
            };
        }
        let Some(si) = best else { break };
        out.push(streams[si].1[idx[si]].clone());
        idx[si] += 1;
    }
    (out, idx)
}

/// Advance a stream's position after the merge consumed `consumed` of its `fetched` items.
fn advance(pos: &mut StreamPos, consumed: usize, fetched: usize, next: Option<String>) {
    if pos.d {
        return;
    }
    if consumed >= fetched {
        match next {
            Some(n) => {
                pos.c = Some(n);
                pos.s = 0;
            }
            None => pos.d = true,
        }
    } else {
        pos.s += consumed;
    }
}

fn peer_warning(name: &str, code: &str, message: String) -> ItemWarning {
    ItemWarning {
        subject: name.to_string(),
        code: code.to_string(),
        message,
    }
}

// ── the fan-out entry points ─────────────────────────────────────────────────────────────────

/// Fan a query out across the local index and every federated peer, and merge one page.
///
/// Returns `Ok(None)` when fan-out does not apply — no federated sources, or the query is pinned
/// to a local source — in which case the caller runs the plain local path. Routing: a `Source`
/// filter naming a federated source sends the query to that peer alone (its whole catalog *is*
/// that source, so the filter is stripped); a `Source` filter naming a local source disables
/// fan-out; no source filter merges local + all peers.
pub(crate) async fn federated_query(
    lib: &EmbeddedLibrary,
    req: &QueryRequest,
) -> Result<Option<Page<AssetSummary>>, LibError> {
    let peers = lib.fed_peers().await;
    if peers.is_empty() {
        return Ok(None);
    }

    let mut peer_target: Option<Arc<Peer>> = None;
    let mut local_source_filter = false;
    for f in &req.filters {
        if f.field == FacetField::Source {
            if let FilterValue::Str(s) = &f.value {
                match peers.iter().find(|p| p.source_id.to_string() == *s) {
                    Some(p) => peer_target = Some(p.clone()),
                    None => local_source_filter = true,
                }
            }
        }
    }
    if peer_target.is_none() && local_source_filter {
        return Ok(None);
    }

    let limit = req.page.clamped(500);
    let mut fc = decode_fed_cursor(&req.page.after);

    // The request peers answer: their own catalog only (federation is one hop, never transitive).
    let mut fwd = req.clone();
    fwd.local_only = true;
    fwd.include_facets = false;
    if peer_target.is_some() {
        fwd.filters.retain(|f| f.field != FacetField::Source);
    }

    let active_peers: Vec<Arc<Peer>> = match &peer_target {
        Some(p) => vec![p.clone()],
        None => peers.iter().cloned().collect(),
    };

    // Local page (skipped under peer-only routing). A local failure is a hard error — fail-soft
    // applies to remote edges, not our own index.
    let local_fut = async {
        if peer_target.is_some() {
            return None;
        }
        let base = req.clone();
        Some(
            fetch_stream(fc.l.clone(), |after| {
                let mut r = base.clone();
                r.local_only = true;
                r.include_facets = false;
                r.page = PageParams { after, limit };
                lib.local_query(r)
            })
            .await,
        )
    };

    // Peer pages, raced against the frozen deadline. A dropped peer never taints the rest.
    let peer_futs = active_peers.iter().map(|p| {
        let peer = p.clone();
        let fwd = fwd.clone();
        let pos =
            fc.p.get(&p.source_id.to_string())
                .cloned()
                .unwrap_or_default();
        async move {
            let fetch = |after| {
                let mut r = fwd.clone();
                r.page = PageParams { after, limit };
                let client = &peer.client;
                async move { client.query(&ectx(), r).await }
            };
            let res = match tokio::time::timeout(QUERY_DEADLINE, fetch_stream(pos, fetch)).await {
                Ok(r) => r,
                Err(_) => Err(LibError::SourceUnavailable(format!(
                    "no answer within the {}ms federated deadline",
                    QUERY_DEADLINE.as_millis()
                ))),
            };
            (peer, res)
        }
    });

    let (local_res, peer_results) = tokio::join!(local_fut, futures::future::join_all(peer_futs));

    let mut partial = PartialStatus::default();
    // (is_local, page) in stream order; failed peers drop out with a warning.
    let mut pages: Vec<(bool, Option<SourceId>, StreamPage)> = Vec::new();
    if let Some(local) = local_res {
        pages.push((true, None, local?));
    }
    for (peer, res) in peer_results {
        match res {
            Ok(mut sp) => {
                for item in &mut sp.items {
                    item.origin = Origin::Peer(peer.name.clone());
                    // Re-attribute to the *local* federated source row. The id the peer sent names a
                    // source in the peer's catalog and is meaningless here; the local row is what a
                    // share, a `Source` filter, and a visibility ceiling are expressed against.
                    item.source_id = Some(peer.source_id);
                }
                pages.push((false, Some(peer.source_id), sp));
            }
            Err(e) => {
                partial.complete = false;
                partial
                    .warnings
                    .push(peer_warning(&peer.name, "peer_dropped", e.to_string()));
            }
        }
    }

    let stream_views: Vec<(bool, &[AssetSummary])> = pages
        .iter()
        .map(|(is_local, _, sp)| (*is_local, sp.items.as_slice()))
        .collect();
    let (items, consumed) = merge_pages(&stream_views, &req.sort, limit as usize);

    // Advance each answering stream's position; a dropped peer keeps its old position (retried
    // next page). Then decide whether any stream can still produce items.
    for (i, (is_local, sid, sp)) in pages.iter().enumerate() {
        let mut pos = sp.pos.clone();
        advance(&mut pos, consumed[i], sp.items.len(), sp.next.clone());
        if *is_local {
            fc.l = pos;
        } else if let Some(sid) = sid {
            fc.p.insert(sid.to_string(), pos);
        }
    }
    if peer_target.is_some() {
        fc.l.d = true; // peer-only routing: the local stream never participates in this listing
    }
    let answered_exhausted = pages
        .iter()
        .enumerate()
        .all(|(i, (_, _, sp))| consumed[i] >= sp.items.len() && sp.next.is_none());
    let dropped_any = !partial.complete;
    let cursor = if answered_exhausted && !dropped_any {
        None
    } else {
        encode_fed_cursor(&fc)
    };

    Ok(Some(Page {
        items,
        cursor,
        // No true cross-peer count exists under fan-out — always None (UI renders "N+").
        total: None,
        partial,
    }))
}

/// Cross-peer "find similar" (issue #40). `local` is the already-ranked local page; matched-space
/// peers rank the shipped vector in their own index and merge into one globally-ranked list.
/// Mismatched-space peers are **omitted from the unified ranking** (the safe default of tech-spec
/// 07 §5 — distances across spaces are not comparable) and flagged with a `space_mismatch`
/// warning; incompatible-protocol peers likewise never co-rank.
pub(crate) async fn federated_similar(
    lib: &EmbeddedLibrary,
    req: &SimilarRequest,
    media: MediaType,
    space: String,
    vector: Vec<f32>,
    local: Vec<SimilarHit>,
) -> Page<SimilarHit> {
    let peers = lib.fed_peers().await;
    let mut partial = PartialStatus::default();
    let mut all = local;

    let peer_futs = peers.iter().map(|p| {
        let peer = p.clone();
        let (media, space, vector) = (media, space.clone(), vector.clone());
        let filters = req.filters.clone();
        let k = req.k;
        async move {
            let run = async {
                let ad = peer.advertise().await?;
                if !protocol_compatible(&ad.protocol_version, FEDERATION_PROTOCOL_VERSION) {
                    return Err(LibError::Unsupported(format!(
                        "peer speaks federation protocol {}, this build speaks {}",
                        ad.protocol_version, FEDERATION_PROTOCOL_VERSION
                    )));
                }
                if ad.spaces.get(media.as_str()) != Some(&space) {
                    return Ok(None); // mismatched embedding space — never co-rank (issue #40)
                }
                let hits = peer
                    .client
                    .find_similar_by_vector(
                        &ectx(),
                        dam_api::VectorSimilarRequest {
                            media,
                            space: space.clone(),
                            vector,
                            k,
                            filters,
                        },
                    )
                    .await?;
                Ok(Some(hits.items))
            };
            let res = match tokio::time::timeout(QUERY_DEADLINE, run).await {
                Ok(r) => r,
                Err(_) => Err(LibError::SourceUnavailable(format!(
                    "no answer within the {}ms federated deadline",
                    QUERY_DEADLINE.as_millis()
                ))),
            };
            (peer, res)
        }
    });

    for (peer, res) in futures::future::join_all(peer_futs).await {
        match res {
            Ok(Some(hits)) => {
                for mut hit in hits {
                    hit.asset.origin = Origin::Peer(peer.name.clone());
                    hit.asset.source_id = Some(peer.source_id);
                    all.push(hit);
                }
            }
            Ok(None) => partial.warnings.push(peer_warning(
                &peer.name,
                "space_mismatch",
                "peer ranks in a different embedding space — omitted from unified ranking"
                    .to_string(),
            )),
            Err(e) => {
                partial.complete = false;
                partial
                    .warnings
                    .push(peer_warning(&peer.name, "peer_dropped", e.to_string()));
            }
        }
    }

    // One globally-ranked list: cosine in a shared space is comparable across peers. Local wins
    // score ties (locality is cheaper to open).
    all.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let la = matches!(a.asset.origin, Origin::Local);
                let lb = matches!(b.asset.origin, Origin::Local);
                lb.cmp(&la)
            })
    });
    all.truncate(req.k as usize);
    Page {
        items: all,
        cursor: None,
        total: None,
        partial,
    }
}

// ── peer read proxying (previews + detail for merged peer hits) ─────────────────────────────

/// Route a follow-up read to its locally registered owner. A valid hint performs exactly one
/// network request. Missing/stale hints (old bookmarks and removed/reconfigured sources) use one
/// bounded, concurrent recovery round, so latency never grows with peer count.
async fn try_peers<T, F, Fut>(
    lib: &EmbeddedLibrary,
    owner: Option<SourceId>,
    call: F,
) -> Option<(SourceId, T)>
where
    F: Fn(Arc<Peer>) -> Fut,
    Fut: std::future::Future<Output = Result<T, LibError>>,
{
    if let Some(source) = owner {
        if let Some(peer) = lib.fed_peer(source).await {
            if let Ok(Ok(value)) = tokio::time::timeout(PROXY_TIMEOUT, call(peer)).await {
                return Some((source, value));
            }
        }
    }

    let peers = lib.fed_peers().await;
    let mut attempts = peers
        .iter()
        .filter(|peer| Some(peer.source_id) != owner)
        .cloned()
        .map(|peer| {
            let source = peer.source_id;
            let future = call(peer);
            async move { future.await.ok().map(|value| (source, value)) }
        })
        .collect::<futures::stream::FuturesUnordered<_>>();
    tokio::time::timeout(PROXY_TIMEOUT, async {
        while let Some(result) = attempts.next().await {
            if result.is_some() {
                return result;
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// Detail record for a peer asset, origin re-tagged to the owning peer.
pub(crate) async fn proxy_get_asset(
    lib: &EmbeddedLibrary,
    id: &AssetId,
    owner: Option<SourceId>,
) -> Option<Asset> {
    let id = *id;
    try_peers(lib, owner, |peer| async move {
        let mut asset = peer.client.get_asset(&ectx(), &id).await?;
        asset.summary.origin = Origin::Peer(peer.name.clone());
        // Both attribution fields name the local federated source row, not the peer's own source.
        asset.source_id = peer.source_id;
        asset.summary.source_id = Some(peer.source_id);
        Ok(asset)
    })
    .await
    .map(|(_, asset)| asset)
}

/// Preview-sized raw bytes for a peer asset (audio playback / viewer islands). Pass-through, no
/// local cache — content reads are interactive one-offs.
pub(crate) async fn proxy_read_content(
    lib: &EmbeddedLibrary,
    id: &AssetId,
    owner: Option<SourceId>,
) -> Option<AssetContent> {
    let id = *id;
    try_peers(lib, owner, |peer| async move {
        peer.client.read_content(&ectx(), &id).await
    })
    .await
    .map(|(_, content)| content)
}

/// Stat and stream peer-owned content through the peer's HTTP range transport. No cache is used:
/// validators and revocation remain the owning peer's authority, and dropping the downstream body
/// drops reqwest's upstream response stream as well.
pub(crate) async fn proxy_content_metadata(
    lib: &EmbeddedLibrary,
    id: &AssetId,
    owner: Option<SourceId>,
) -> Option<AssetContentMetadata> {
    let id = *id;
    try_peers(lib, owner, |peer| async move {
        peer.client.content_metadata(&ectx(), &id).await
    })
    .await
    .map(|(_, metadata)| metadata)
}

pub(crate) async fn proxy_stream_content(
    lib: &EmbeddedLibrary,
    id: &AssetId,
    range: ContentRange,
    owner: Option<SourceId>,
) -> Option<AssetContentStream> {
    let id = *id;
    try_peers(lib, owner, |peer| async move {
        peer.client.stream_content(&ectx(), &id, range).await
    })
    .await
    .map(|(_, stream)| stream)
}

pub(crate) async fn proxy_read_related(
    lib: &EmbeddedLibrary,
    id: &AssetId,
    rel: &str,
    owner: Option<SourceId>,
) -> Option<AssetContent> {
    let id = *id;
    let rel = rel.to_string();
    try_peers(lib, owner, |peer| {
        let rel = rel.clone();
        async move { peer.client.read_related_content(&ectx(), &id, &rel).await }
    })
    .await
    .map(|(_, content)| content)
}

/// Thumbnail for a peer asset — the one sanctioned federated byte transfer (a remote-owned
/// derivative, tech-spec 07 §4), cached locally under `cache/peer/` with the frozen 7-day TTL.
pub(crate) async fn proxy_thumbnail(
    lib: &EmbeddedLibrary,
    id: &AssetId,
    edge: u32,
    owner: Option<SourceId>,
) -> Option<AssetContent> {
    let flight = format!(
        "peer-thumbnail:{}:{id}:{edge}",
        owner.map_or_else(|| "legacy".into(), |v| v.to_string())
    );
    lib.cache
        .singleflight(flight, || async move {
            if let Some(source) = owner {
                // A removed source must not keep serving its old cached bytes. A stale bookmark
                // skips this cache and enters the bounded recovery round below.
                if lib.fed_peer(source).await.is_some() {
                    let name = format!("{source}/{id}-{edge}.png");
                    if let Some(bytes) = peer_cache_read(lib, &name).await {
                        return Some(png_content(bytes));
                    }
                }
            }
            let id = *id;
            let (source, content) = try_peers(lib, owner, |peer| async move {
                peer.client.read_thumbnail(&ectx(), &id, edge).await
            })
            .await?;
            let name = format!("{source}/{id}-{edge}.png");
            peer_cache_write(lib, &name, content.bytes.clone()).await;
            Some(content)
        })
        .await
}

/// Interactive 3D preview blob for a peer asset, cached like the thumbnail tier.
pub(crate) async fn proxy_model_preview(
    lib: &EmbeddedLibrary,
    id: &AssetId,
    owner: Option<SourceId>,
) -> Option<AssetContent> {
    let flight = format!(
        "peer-model:{}:{id}",
        owner.map_or_else(|| "legacy".into(), |v| v.to_string())
    );
    lib.cache
        .singleflight(flight, || async move {
            if let Some(source) = owner {
                if lib.fed_peer(source).await.is_some() {
                    let name = format!("{source}/{id}.dmsh");
                    if let Some(bytes) = peer_cache_read(lib, &name).await {
                        return Some(AssetContent {
                            bytes,
                            content_type: "model/x-dam-preview".to_string(),
                            format: "dmsh".to_string(),
                            media: MediaType::Model,
                        });
                    }
                }
            }
            let id = *id;
            let (source, content) = try_peers(lib, owner, |peer| async move {
                peer.client.read_model_preview(&ectx(), &id).await
            })
            .await?;
            let name = format!("{source}/{id}.dmsh");
            peer_cache_write(lib, &name, content.bytes.clone()).await;
            Some(content)
        })
        .await
}

fn png_content(bytes: Vec<u8>) -> AssetContent {
    AssetContent {
        bytes,
        content_type: "image/png".to_string(),
        format: "png".to_string(),
        media: MediaType::Image,
    }
}

fn peer_cache_dir(lib: &EmbeddedLibrary) -> std::path::PathBuf {
    lib.data_dir.join("cache").join("peer")
}

async fn peer_cache_read(lib: &EmbeddedLibrary, name: &str) -> Option<Vec<u8>> {
    let path = peer_cache_dir(lib).join(name);
    let cache = lib.cache.clone();
    lib.run_bg(move |_| Ok(cache.read(&path, crate::cache::Tier::Peer)))
        .await
        .ok()
        .flatten()
}

async fn peer_cache_write(lib: &EmbeddedLibrary, name: &str, bytes: Vec<u8>) {
    let path = peer_cache_dir(lib).join(name);
    let cache = lib.cache.clone();
    let _ = lib
        .run_bg(move |_| {
            cache.publish(&path, &bytes, crate::cache::Tier::Peer);
            Ok(())
        })
        .await;
}
