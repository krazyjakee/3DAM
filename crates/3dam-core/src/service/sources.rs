//! Uploads, source and folder management, asset removal, and the content blocklist.

use crate::*;

impl EmbeddedLibrary {
    pub(crate) async fn upload_impl(
        &self,
        ctx: &AuthContext,
        req: UploadRequest,
        staged: &std::path::Path,
    ) -> Result<UploadOutcome, LibError> {
        ctx.require(Scope::Write)?;
        if !ctx.visibility.allows_source(&req.source) {
            return Err(LibError::NotFound(format!("source {}", req.source)));
        }
        if !ctx.visibility.allows_source_write(&req.source) {
            return Err(LibError::Forbidden(
                "no write access to this source (a write share is required)".into(),
            ));
        }

        let scratch = self.scratch();
        let staged = staged.to_path_buf();
        let events = self.events.clone();
        let secrets = self.secrets.clone();
        let outcome = self
            .db(move |s| upload::run_upload(s, &secrets, &events, req, &staged, &scratch))
            .await?;

        // Ask the background pipeline for a drain. `ingest_one` writes only the cheap tier, and the
        // pipeline's other trigger is a `Scan` job completing — so without this an uploaded asset
        // would carry no embedding and no auto-tags (invisible to `similar`/`dedup`) until some
        // unrelated scan of that source happened to run. `Notify` holds one permit, so a
        // twenty-file drop coalesces into a single follow-up pass rather than twenty.
        if outcome.asset.is_some() {
            self.pipeline_wake.notify_one();
        }
        Ok(outcome)
    }

    pub(crate) async fn list_sources_impl(
        &self,
        ctx: &AuthContext,
    ) -> Result<Vec<SourceInfo>, LibError> {
        let all = self.db(|s| s.list_sources()).await?;
        // An unshared source is absent from the listing (issue #42): filter, don't 403.
        let mut visible: Vec<SourceInfo> = all
            .into_iter()
            .filter(|s| ctx.visibility.allows_source(&s.id))
            .collect();
        self.mark_writable(&mut visible).await;
        for source in &mut visible {
            if !ctx.scopes.has(Scope::Write) {
                source.writable = false;
                source.writable_reason = Some("read-only — write scope is required".into());
            } else if !ctx.visibility.allows_source_write(&source.id) {
                source.writable = false;
                source.writable_reason = Some("read-only — a write share is required".into());
            }
        }
        Ok(visible)
    }

    pub(crate) async fn get_source_impl(
        &self,
        ctx: &AuthContext,
        id: &SourceId,
    ) -> Result<SourceInfo, LibError> {
        if !ctx.visibility.allows_source(id) {
            return Err(LibError::NotFound(format!("source {id}")));
        }
        let sid = *id;
        let mut info = self
            .db(move |s| {
                s.get_source(&sid)?
                    .ok_or_else(|| LibError::NotFound(format!("source {sid}")))
            })
            .await?;
        self.mark_writable(std::slice::from_mut(&mut info)).await;
        if !ctx.scopes.has(Scope::Write) {
            info.writable = false;
            info.writable_reason = Some("read-only — write scope is required".into());
        } else if !ctx.visibility.allows_source_write(&info.id) {
            info.writable = false;
            info.writable_reason = Some("read-only — a write share is required".into());
        }
        Ok(info)
    }

    pub(crate) async fn list_folders_impl(
        &self,
        ctx: &AuthContext,
        req: FolderListing,
    ) -> Result<Vec<FolderEntry>, LibError> {
        // Folder trees enumerate paths without touching assets (issue #42 leak audit): an
        // unreachable source has no tree. A collection-only grant reaches assets, not the tree.
        if !ctx.visibility.allows_source(&req.source) {
            return Err(LibError::NotFound(format!("source {}", req.source)));
        }
        // Normalise the prefix so the derived-tree SQL is well-defined: empty (root) or ending in `/`.
        let prefix = if req.prefix.is_empty() || req.prefix.ends_with('/') {
            req.prefix
        } else {
            format!("{}/", req.prefix)
        };
        let source = req.source;
        self.db(move |s| s.list_folders(&source, &prefix)).await
    }

    pub(crate) async fn add_source_impl(
        &self,
        ctx: &AuthContext,
        req: AddSource,
    ) -> Result<SourceId, LibError> {
        Self::require_full_visibility(ctx, "adding a source")?;
        let opts = dam_sources::ConnOptions {
            username: req.options.username.clone(),
            password: req.options.password.clone(),
            private_key: req.options.private_key.clone(),
            passphrase: req.options.passphrase.clone(),
            domain: req.options.domain.clone(),
            port: req.options.port,
        };
        let mut conn = dam_sources::SourceConnection::parse(req.kind.as_str(), &req.uri, &opts)?;

        // Local FS: normalise + existence-check up front (fail early on a typo). Remote sources are
        // allowed to be offline at add-time — the scan surfaces reachability, fail-soft (§3).
        let default_name: String;
        if let dam_sources::SourceConnection::LocalFs { root } = &mut conn {
            let path = PathBuf::from(&*root);
            if !path.exists() {
                return Err(LibError::BadRequest(format!("path does not exist: {root}")));
            }
            let canon = path
                .canonicalize()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| root.clone());
            default_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| canon.clone());
            *root = canon;
        } else {
            default_name = conn.display_uri();
        }

        // Federated peer: handshake up front (phase 6, issue #39) — verify the peer serves
        // federation and speaks a compatible protocol version, so a typo'd endpoint or a
        // flag-off peer fails here with a clear message, not silently at first query.
        if let dam_sources::SourceConnection::Federated(cfg) = &conn {
            let client = federation::connect_peer(&cfg.endpoint, cfg.token.clone()).await?;
            let ad = tokio::time::timeout(federation::ADD_HANDSHAKE_TIMEOUT, client.advertise())
                .await
                .map_err(|_| {
                    LibError::BadRequest(format!(
                        "peer {} did not answer advertise in time",
                        cfg.endpoint
                    ))
                })?
                .map_err(|e| match e {
                    LibError::NotFound(_) => LibError::BadRequest(format!(
                        "peer {} is not serving federation — enable its 'federation' flag",
                        cfg.endpoint
                    )),
                    e => LibError::BadRequest(format!("peer {} unreachable: {e}", cfg.endpoint)),
                })?;
            if !dam_api::protocol_compatible(
                &ad.protocol_version,
                dam_api::FEDERATION_PROTOCOL_VERSION,
            ) {
                return Err(LibError::BadRequest(format!(
                    "peer {} speaks federation protocol {} but this build speaks {}",
                    cfg.endpoint,
                    ad.protocol_version,
                    dam_api::FEDERATION_PROTOCOL_VERSION
                )));
            }
        }

        let name = req.name.clone().unwrap_or(default_name);
        let watch = req.options.watch;
        let is_federated = matches!(conn, dam_sources::SourceConnection::Federated(_));
        let id = SourceId::new();
        let credential = conn.take_credentials();
        let credential_ref = credential
            .as_ref()
            .map(|_| credentials::SecretVault::reference(&id));
        let secrets = self.secrets.clone();
        let stored_ref = credential_ref.clone();
        self.db(move |s| {
            // Commit the redacted row + deterministic opaque ref first. If the secret write then
            // fails, the row still tracks any provider-side partial success; cleanup deletes the
            // credential before the row, so no failure ordering can create an untracked secret.
            s.add_source_with_auth(id, &conn, &name, watch, stored_ref.as_deref())?;
            if let (Some(reference), Some(material)) = (stored_ref.as_deref(), credential.as_ref())
            {
                if let Err(error) = secrets.put(reference, material) {
                    // If deletion itself cannot be confirmed, retain the row/reference. It will
                    // list as locked/unavailable and remains recoverable; removing the row here
                    // would turn a possibly committed provider entry into an orphan.
                    if secrets.delete(reference).is_ok() {
                        if let Err(cleanup) = s.remove_source(&id, false) {
                            tracing::error!(source = %id, error = %cleanup, "source rollback after credential failure failed");
                        }
                    }
                    return Err(error);
                }
            }
            Ok(())
        })
        .await?;
        // Start watching immediately if requested (tech-spec 07 §3.1).
        if watch {
            self.watchers.ensure(id);
        }
        if is_federated {
            self.fed.invalidate().await; // participate in the very next query
        }
        Ok(id)
    }

    pub(crate) async fn remove_source_impl(
        &self,
        ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError> {
        Self::require_full_visibility(ctx, "removing a source")?;
        let id = *id;
        let secrets = self.secrets.clone();
        self.db(move |s| {
            s.remove_source(&id, req.keep_metadata)?;
            if !req.keep_metadata {
                credentials::cleanup_pending_credentials(s, &secrets)?;
            }
            Ok(())
        })
        .await?;
        self.fed.invalidate().await;
        // Peer derivatives are owner-scoped. Once the source is gone they must not linger until
        // their TTL; lifecycle cleanup is an explicit bounded-pool maintenance walk.
        let cache = self.cache.clone();
        let peer_dir = self
            .data_dir
            .join("cache")
            .join("peer")
            .join(id.to_string());
        self.run_bg(move |_| {
            cache.remove_tree(&peer_dir);
            Ok(())
        })
        .await?;
        Ok(())
    }

    // ── remove / blocklist (issue #21) ───────────────────────────────────────

    pub(crate) async fn remove_asset_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        req: RemoveAsset,
    ) -> Result<(), LibError> {
        self.require_asset_writable(ctx, id).await?;
        // Blocklisting is a library-wide policy (it affects every future scan), not a per-asset
        // edit — reserved for unrestricted identities.
        if req.block {
            Self::require_full_visibility(ctx, "blocklisting content")?;
        }
        let id = *id;
        // Capture the attribution *before* the delete — afterwards the row is gone and the event
        // could never be matched against a subscriber's ceiling (issue #42).
        let source_id = self.db(move |s| s.asset_source(&id)).await?;
        self.db(move |s| s.remove_asset(&id, req.block)).await?;
        // Live update: drop the row from every open grid/inspector (mirrors AssetAdded on scan).
        reliability::publish_event(
            &self.events,
            LibraryEvent::AssetRemoved { id, source_id },
            "publish removed asset",
        );
        Ok(())
    }

    pub(crate) async fn list_blocklist_impl(
        &self,
        ctx: &AuthContext,
    ) -> Result<Vec<BlockEntry>, LibError> {
        // Blocked hashes describe library-wide content a restricted caller may not reach — the
        // list is simply empty for them (absent, not forbidden).
        if !ctx.visibility.is_full() {
            return Ok(Vec::new());
        }
        self.db(|s| s.list_blocklist()).await
    }

    pub(crate) async fn unblock_impl(
        &self,
        ctx: &AuthContext,
        hash: &ContentHash,
    ) -> Result<(), LibError> {
        Self::require_full_visibility(ctx, "editing the blocklist")?;
        let hash = *hash;
        self.db(move |s| s.unblock(&hash)).await
    }
}
