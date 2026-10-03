//! Asset discovery, content delivery, derivative reads, prefetching, and visible statistics.

use crate::*;

impl EmbeddedLibrary {
    pub(crate) async fn query_impl(
        &self,
        ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        // Federated fan-out (phase 6, issue #39): merge local + peer pages when federated sources
        // are registered. `local_only` marks a peer-bound call — one hop, never transitive.
        // Restricted contexts never fan out (a peer's catalog is outside their reachable set);
        // their query runs locally under the ceiling predicate.
        if !req.local_only {
            if let Some(page) = federation::federated_query(self, &req, &ctx.visibility).await? {
                return Ok(page);
            }
        }
        self.local_query_vis(req, ctx.visibility.clone()).await
    }

    pub(crate) async fn get_asset_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<Asset, LibError> {
        self.get_asset_from(ctx, id, None).await
    }

    pub(crate) async fn get_asset_from_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<Asset, LibError> {
        let id = *id;
        // The detail read is the one path that surfaces collection membership, so it carries the
        // ceiling: an asset reached through a collection share must not enumerate the *other*
        // collections holding it (issue #42 — unreachable is absent, not merely denied).
        let vis = ctx.visibility.clone();
        let local = if self.asset_visible(ctx, &id).await? {
            self.db(move |s| s.get_asset_detail(&id, &vis)).await
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        };
        match local {
            // A merged result can name a peer-owned asset: proxy the detail read (phase 6).
            // Never for a restricted context — the ceiling can't vouch for peer-owned ids.
            Err(LibError::NotFound(_)) if Self::may_proxy_peer(ctx, source) => {
                federation::proxy_get_asset(self, &id, source, ctx.visibility.is_full())
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            r => r,
        }
    }

    pub(crate) async fn read_content_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        self.read_content_from(ctx, id, None).await
    }

    pub(crate) async fn read_content_from_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        let local = if self.asset_visible(ctx, &id).await? {
            self.db(move |s| {
                let asset = s.get_asset(&id)?;
                read_asset_content(s, &secrets, &asset, &scratch)
            })
            .await
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        };
        match local {
            Err(LibError::NotFound(_)) if Self::may_proxy_peer(ctx, source) => {
                federation::proxy_read_content(self, &id, source, ctx.visibility.is_full())
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            r => r,
        }
    }

    pub(crate) async fn content_metadata_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContentMetadata, LibError> {
        self.content_metadata_from(ctx, id, None).await
    }

    pub(crate) async fn content_metadata_from_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContentMetadata, LibError> {
        let id = *id;
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        let local = if self.asset_visible(ctx, &id).await? {
            self.db(move |store| {
                let asset = store.get_asset(&id)?;
                let connection = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
                let source = dam_sources::open_source(&connection, &scratch)?;
                let stat = source.content_stat(&asset.path)?;
                Ok(content_metadata(&asset, stat))
            })
            .await
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        };
        match local {
            Err(LibError::NotFound(_)) if Self::may_proxy_peer(ctx, source) => {
                federation::proxy_content_metadata(self, &id, source, ctx.visibility.is_full())
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            result => result,
        }
    }

    pub(crate) async fn stream_content_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
    ) -> Result<AssetContentStream, LibError> {
        self.stream_content_from(ctx, id, range, None).await
    }

    pub(crate) async fn stream_content_from_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
        source: Option<SourceId>,
    ) -> Result<AssetContentStream, LibError> {
        let id = *id;
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        let local = if self.asset_visible(ctx, &id).await? {
            self.db(move |store| {
                let asset = store.get_asset(&id)?;
                let connection = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
                let source = dam_sources::open_source(&connection, &scratch)?;
                let stat = source.content_stat(&asset.path)?;
                if range.last() >= stat.len {
                    return Err(LibError::BadRequest(
                        "content stream range is outside the representation".into(),
                    ));
                }
                let metadata = content_metadata(&asset, stat);
                Ok((source, asset.path, metadata))
            })
            .await
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        };
        match local {
            Ok((source, path, metadata)) => {
                Ok(source_content_stream(source, path, metadata, range))
            }
            Err(LibError::NotFound(_)) if Self::may_proxy_peer(ctx, source) => {
                federation::proxy_stream_content(self, &id, range, source, ctx.visibility.is_full())
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn read_related_content_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
    ) -> Result<AssetContent, LibError> {
        self.read_related_content_from(ctx, id, rel, None).await
    }

    pub(crate) async fn read_related_content_from_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        let rel = rel.to_string();
        let rel2 = rel.clone();
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        let local = if self.asset_visible(ctx, &id).await? {
            self.db(move |s| {
                let asset = s.get_asset(&id)?;
                read_related_content(s, &secrets, &asset, &rel2, &scratch)
            })
            .await
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        };
        match local {
            Err(LibError::NotFound(_)) if Self::may_proxy_peer(ctx, source) => {
                federation::proxy_read_related(self, &id, &rel, source, ctx.visibility.is_full())
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            r => r,
        }
    }

    pub(crate) async fn read_thumbnail_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
    ) -> Result<AssetContent, LibError> {
        self.read_thumbnail_from(ctx, id, max_edge, None).await
    }

    pub(crate) async fn read_thumbnail_from_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        let edge = max_edge.clamp(THUMB_MIN_EDGE, THUMB_MAX_EDGE);
        let data_dir = self.data_dir.clone();
        let cache = self.cache.clone();
        let secrets = self.secrets.clone();
        // Fast path: a cheap cache probe on the unbounded pool, so an already-rendered thumbnail is
        // never stuck behind background generation.
        let probe_dir = data_dir.clone();
        let probe = if self.asset_visible(ctx, &id).await? {
            self.db(move |s| {
                let asset = s.get_asset(&id)?;
                Ok((
                    thumb_cache_lookup(&cache, &probe_dir, &asset, edge),
                    asset.summary.media == MediaType::Model,
                ))
            })
            .await
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        };
        let is_model = match probe {
            Ok((Some(hit), _)) => return Ok(hit),
            Ok((None, is_model)) => is_model,
            // Peer-owned asset: fetch its remote-owned preview — the one sanctioned federated byte
            // transfer (tech-spec 07 §4) — through the 7-day local peer cache. Full-visibility only.
            Err(LibError::NotFound(_)) if Self::may_proxy_peer(ctx, source) => {
                return federation::proxy_thumbnail(
                    self,
                    &id,
                    edge,
                    source,
                    ctx.visibility.is_full(),
                )
                .await
                .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            Err(e) => return Err(e),
        };
        // Cache miss: the expensive render/decode runs on the bounded background pool so a grid
        // burst can't starve an interactive inspector read (preview / waveform / detail).
        let key = if is_model {
            format!("model-derivatives:{id}:{edge}")
        } else {
            format!("thumbnail:{id}:{edge}")
        };
        let cache = self.cache.clone();
        self.cache
            .singleflight(key, || async move {
                self.run_interactive(move |s| {
                    let asset = s.get_asset(&id)?;
                    if is_model {
                        let derivatives =
                            gen_model_derivatives(&cache, &data_dir, s, &secrets, &asset, edge)?;
                        derivatives.thumbnail.map(png_content).ok_or_else(|| {
                            LibError::Unsupported("3D thumbnail generation failed".into())
                        })
                    } else {
                        gen_thumbnail(&cache, &data_dir, s, &secrets, &asset, edge)
                    }
                })
                .await
            })
            .await
    }

    pub(crate) async fn read_model_preview_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        self.read_model_preview_from(ctx, id, None).await
    }

    pub(crate) async fn read_model_preview_from_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        let data_dir = self.data_dir.clone();
        let cache = self.cache.clone();
        let secrets = self.secrets.clone();
        let probe_dir = data_dir.clone();
        let probe = if self.asset_visible(ctx, &id).await? {
            self.db(move |s| {
                let asset = s.get_asset(&id)?;
                if asset.summary.media != MediaType::Model {
                    return Err(LibError::Unsupported(
                        "3D preview is only available for model assets".into(),
                    ));
                }
                Ok(model_preview_cache_lookup(&cache, &probe_dir, &asset))
            })
            .await
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        };
        match probe {
            Ok(Some(hit)) => return Ok(hit),
            Ok(None) => {}
            Err(LibError::NotFound(_)) if Self::may_proxy_peer(ctx, source) => {
                return federation::proxy_model_preview(
                    self,
                    &id,
                    source,
                    ctx.visibility.is_full(),
                )
                .await
                .ok_or_else(|| LibError::NotFound(format!("asset {id}")));
            }
            Err(error) => return Err(error),
        }
        let edge = background::PREGEN_THUMB_EDGE;
        let key = format!("model-derivatives:{id}:{edge}");
        let cache = self.cache.clone();
        self.cache
            .singleflight(key, || async move {
                self.run_interactive(move |store| {
                    let asset = store.get_asset(&id)?;
                    let derivatives =
                        gen_model_derivatives(&cache, &data_dir, store, &secrets, &asset, edge)?;
                    Ok(preview_content(derivatives.preview))
                })
                .await
            })
            .await
    }

    pub(crate) async fn prefetch_impl(
        &self,
        ctx: &AuthContext,
        req: PrefetchRequest,
    ) -> Result<(), LibError> {
        // A pure optimisation: for a restricted context it's a safe no-op (the on-demand reads
        // still generate + guard), which keeps hidden ids from steering the warm path.
        if req.assets.is_empty() || !ctx.visibility.is_full() {
            return Ok(());
        }
        // Warm the exact thumbnail edge the client will request (the grid uses a variable edge the
        // hosted pipeline can't all pre-render). Input, queue, task, and CPU concurrency are all
        // bounded: overlapping calls feed one deduplicated queue and at most one bg-pool worker.
        let edge = req
            .edge
            .unwrap_or(background::PREGEN_THUMB_EDGE)
            .clamp(THUMB_MIN_EDGE, THUMB_MAX_EDGE);
        let mut unique = HashSet::new();
        for id in req.assets.into_iter().take(PREFETCH_INPUT_SCAN_CAP) {
            unique.insert(id);
            if unique.len() == PREFETCH_INPUT_CAP {
                break;
            }
        }
        let peer_assets: Vec<_> = unique.iter().copied().collect();
        let relay = req.relay;
        {
            let mut pending = self.prefetch_pending.lock().unwrap();
            for id in unique {
                if pending.len() == PREFETCH_QUEUE_CAP {
                    break;
                }
                pending.insert((id, edge));
            }
            self.prefetch_max_pending
                .fetch_max(pending.len(), Ordering::Relaxed);
        }
        if self
            .prefetch_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let store = self.store.clone();
            let secrets = self.secrets.clone();
            let data_dir = self.data_dir.clone();
            let cache = self.cache.clone();
            let governor = self.governor.clone();
            let pending = self.prefetch_pending.clone();
            let running = self.prefetch_running.clone();
            self.bg_pool.spawn(move || {
                let cancel = cache.background_cancel();
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        running.store(false, Ordering::Release);
                        break;
                    }
                    let work = {
                        let mut pending = pending.lock().unwrap();
                        let Some(item) = pending.iter().next().copied() else {
                            // Publish idle while holding the queue lock. A concurrent producer either
                            // already sees `true`, or inserts after this and starts the next worker.
                            running.store(false, Ordering::Release);
                            break;
                        };
                        pending.remove(&item);
                        item
                    };
                    let (id, edge) = work;
                    let Ok(asset) = store.get_asset(&id) else {
                        continue; // vanished (or peer-owned) — fail-soft
                    };
                    let key = if asset.summary.media == MediaType::Model {
                        format!("model-derivatives:{id}:{edge}")
                    } else {
                        format!("thumbnail:{id}:{edge}")
                    };
                    let _ = cache.singleflight_blocking(key, |priority_cancel| {
                        let _ = crate::derivatives::warm_derivatives(
                            &cache,
                            &data_dir,
                            &store,
                            &secrets,
                            &asset,
                            edge,
                            &governor,
                            priority_cancel,
                        );
                    });
                }
            });
        }

        // Preserve merged-grid warming without spawning one task per peer. One sequential fan-out
        // task is admitted per library, peers and ids are capped/deduplicated, and the relay bit
        // prevents mutually registered libraries from bouncing hints forever.
        if !relay
            && self
                .prefetch_peer_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let peers = self.fed_peers().await;
            let running = self.prefetch_peer_running.clone();
            tokio::spawn(async move {
                let request = PrefetchRequest {
                    assets: peer_assets,
                    edge: Some(edge),
                    relay: true,
                };
                for peer in peers.iter().take(PREFETCH_PEER_CAP) {
                    let _ = tokio::time::timeout(
                        federation::QUERY_DEADLINE,
                        peer.client
                            .prefetch(&AuthContext::embedded(), request.clone()),
                    )
                    .await;
                }
                running.store(false, Ordering::Release);
            });
        }
        Ok(())
    }

    pub(crate) async fn library_stats_impl(
        &self,
        ctx: &AuthContext,
        source: Option<SourceId>,
    ) -> Result<LibraryStats, LibError> {
        let vis = ctx.visibility.clone();
        let Some(sid) = source else {
            let mut stats = self.db(move |s| s.stats(None, &vis)).await?;
            // Unscoped stats describe *the library the caller can reach*, not just the local
            // index: a federated source is one row in this sidebar, so leaving its assets out of
            // the aggregate reads as an empty library whenever the catalog is peer-only. The
            // owner's totals therefore fan out exactly like the browse grid already does; a
            // restricted account is additionally narrowed to the peers it may reach, so its
            // aggregate keeps agreeing with its federated search and MCP view. Unavailable peers
            // fail soft because this DTO predates per-peer partial warnings.
            for peer in self.fed_peers().await.iter().filter(|peer| {
                ctx.visibility.is_full() || ctx.visibility.allows_source(&peer.source_id)
            }) {
                let peer_stats = tokio::time::timeout(
                    federation::QUERY_DEADLINE,
                    peer.client.library_stats(&AuthContext::embedded(), None),
                )
                .await;
                let Ok(Ok(peer_stats)) = peer_stats else {
                    continue;
                };
                stats.total += peer_stats.total;
                stats.unanalyzed += peer_stats.unanalyzed;
                for (media, count) in peer_stats.by_media {
                    *stats.by_media.entry(media).or_default() += count;
                }
                for (tag, count) in peer_stats.tags {
                    *stats.tags.entry(tag).or_default() += count;
                }
                // The peer is one source in this library's namespace. Do not leak or collide
                // its internal source names in the outer sidebar aggregate.
                stats.by_source.insert(peer.name.clone(), peer_stats.total);
            }
            return Ok(stats);
        };
        // A source-scoped read of an unreachable source is absent, not an aggregate oracle.
        if !ctx.visibility.allows_source(&sid) {
            return Err(LibError::NotFound(format!("source {sid}")));
        }
        // A federated source's counts live on the peer — proxy the read so the sidebar shows the
        // peer's own numbers, as fresh as the call (phase 6). Local kinds scope the local catalog.
        if let Some(peer) = self
            .fed_peers()
            .await
            .iter()
            .find(|p| p.source_id == sid)
            .cloned()
        {
            return tokio::time::timeout(
                federation::QUERY_DEADLINE,
                peer.client.library_stats(&AuthContext::embedded(), None),
            )
            .await
            .map_err(|_| LibError::SourceUnavailable(format!("peer '{}' timed out", peer.name)))?;
        }
        self.db(move |s| s.stats(Some(&sid), &vis)).await
    }
}

#[cfg(test)]
mod tests {
    use crate::resolve_sibling;

    #[test]
    fn resolve_sibling_confines_to_source() {
        assert_eq!(
            resolve_sibling("models/scene.gltf", "scene.bin").unwrap(),
            "models/scene.bin"
        );
        assert_eq!(
            resolve_sibling("models/scene.gltf", "textures/wall.png").unwrap(),
            "models/textures/wall.png"
        );
        assert_eq!(
            resolve_sibling("a/b/scene.gltf", "../shared.bin").unwrap(),
            "a/shared.bin"
        );
        assert_eq!(
            resolve_sibling("scene.gltf", "scene.bin").unwrap(),
            "scene.bin"
        );
        assert_eq!(
            resolve_sibling("m/s.gltf", "./x/../y.bin").unwrap(),
            "m/y.bin"
        );

        assert!(resolve_sibling("models/s.gltf", "../../etc/passwd").is_err());
        assert!(resolve_sibling("s.gltf", "../secret").is_err());
        assert!(resolve_sibling("m/s.gltf", "/etc/passwd").is_err());
        assert!(resolve_sibling("m/s.gltf", "\\windows\\system32").is_err());
        assert!(resolve_sibling("m/s.gltf", "C:\\windows\\system32").is_err());
        assert!(resolve_sibling("m/s.gltf", "C:drive-relative.bin").is_err());
        assert!(resolve_sibling("m/s.gltf", "http://evil/x").is_err());
        assert!(resolve_sibling("m/s.gltf", "file:/etc/passwd").is_err());
        assert!(resolve_sibling("m/s.gltf", "  ").is_err());
    }
}
