//! Private notes and account-attributed asset discussions.

use crate::*;

impl EmbeddedLibrary {
    pub(crate) async fn get_note_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<Option<Note>, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        self.db(move |s| s.get_note(&id)).await
    }

    pub(crate) async fn set_note_impl(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        req: NoteRequest,
    ) -> Result<Option<Note>, LibError> {
        self.require_asset_writable(ctx, id).await?;
        // Attribution is best-effort and deliberately loose (see `Note::updated_by`): the signed-in
        // account if there is one, else whatever identity the credential resolved to, else nobody —
        // which is the honest answer for a single-user local library.
        let by = ctx
            .account
            .as_ref()
            .map(|a| a.username.clone())
            .or_else(|| ctx.identity.clone());
        let aid = *id;
        let note = self
            .db(move |s| s.set_note(&aid, &req.body, by.as_deref()))
            .await?;
        let source_id = self.db(move |s| s.asset_source(&aid)).await?;
        reliability::publish_event(
            &self.events,
            LibraryEvent::AssetChanged {
                id: aid,
                source_id,
                kind: ChangeKind::NoteSet,
            },
            "publish note change",
        );
        Ok(note)
    }

    pub(crate) async fn list_comments_impl(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Vec<Comment>, LibError> {
        // Read access to the asset is the whole gate: a message body can quote a path or filename
        // from a source this caller was never meant to reach.
        self.require_asset_visible(ctx, asset).await?;
        let asset = *asset;
        self.db(move |s| s.list_comments(&asset)).await
    }

    pub(crate) async fn post_comment_impl(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
        req: NewComment,
    ) -> Result<Comment, LibError> {
        self.require_asset_visible(ctx, asset).await?;
        let author = require_account(ctx)?;
        let body = req.body.trim().to_string();
        if body.is_empty() {
            return Err(LibError::BadRequest("a message needs a body".into()));
        }
        let aid = *asset;
        let reply_to = req.reply_to;
        let comment = self
            .db(move |s| s.add_comment(&aid, &author, &body, reply_to))
            .await?;
        self.emit_commented(aid).await;
        Ok(comment)
    }

    pub(crate) async fn edit_comment_impl(
        &self,
        ctx: &AuthContext,
        id: &CommentId,
        req: EditComment,
    ) -> Result<Comment, LibError> {
        let cid = *id;
        let existing = self.db(move |s| s.get_comment(&cid)).await?;
        self.require_asset_visible(ctx, &existing.asset).await?;
        let author = require_account(ctx)?;
        // Author only — an admin may *remove* a message (moderation) but never rewrite one, since
        // an edited message still carries its original author's name.
        if existing.author.id != author {
            return Err(LibError::Forbidden(
                "only the author may edit a message".into(),
            ));
        }
        let body = req.body.trim().to_string();
        if body.is_empty() {
            return Err(LibError::BadRequest(
                "a message needs a body (delete it instead)".into(),
            ));
        }
        self.db(move |s| s.edit_comment(&cid, &body)).await?;
        let updated = self.db(move |s| s.get_comment(&cid)).await?;
        self.emit_commented(existing.asset).await;
        Ok(updated)
    }

    pub(crate) async fn delete_comment_impl(
        &self,
        ctx: &AuthContext,
        id: &CommentId,
    ) -> Result<(), LibError> {
        let cid = *id;
        let existing = self.db(move |s| s.get_comment(&cid)).await?;
        self.require_asset_visible(ctx, &existing.asset).await?;
        // The author, or a moderator. Checked against the *scope*, never the role name — `Scope` is
        // the single place a capability gains meaning (tech-spec 10 §4.2).
        let is_author = ctx
            .account
            .as_ref()
            .is_some_and(|a| a.account_id == existing.author.id);
        if !is_author && !ctx.scopes.has(Scope::Admin) {
            return Err(LibError::Forbidden(
                "only the author or an admin may delete a message".into(),
            ));
        }
        self.db(move |s| s.delete_comment(&cid)).await?;
        self.emit_commented(existing.asset).await;
        Ok(())
    }
}
