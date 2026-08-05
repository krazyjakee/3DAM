# ADR 0017 — Facial recognition and a People view: declined

Status: **Declined** · Date: 2026-08-05 · Deciders: 3DAM core
Supersedes: — · Related: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §2 (non-goals), §9, §10,
[MISSION.md](../MISSION.md) §7 and *Who it's for*, [ADR 0004](0004-feature-flags-admin.md) (off ⇒ absent),
[ADR 0006](0006-inference-runtime-candle.md) (model runtime), [ADR 0009](0009-v1-scope-decisions.md)
(where v1 lines get drawn), [ADR 0016](0016-vector-index-backend.md) (the vector surface this would extend),
issues [#83](https://github.com/krazyjakee/3DAM/issues/83) (the epic) and
[#180](https://github.com/krazyjakee/3DAM/issues/180) (this gate)

## Context

Issue #83 proposes detecting faces in image assets, embedding each face as an ArcFace-style
template, clustering the templates by identity, and adding a **People** browsing surface where a
user names a cluster and then filters the library by person. Issue #180 exists because that is a
**go/no-go gate**, and it was filed alone and deliberately: a seven-child decomposition of #83 was
drafted and then *not* filed, because pre-filing implementation issues builds momentum toward "yes"
before the question has been asked.

The use cases are real and should not be strawmanned. A studio's reference library genuinely
contains character reference photos, concept art, photogrammetry source plates, actor and mocap
reference, and texture source photography with people in it. "Show me every plate of this actor" is
a question someone actually asks.

Three things frame the answer.

**The legal weight is the dominant constraint, not a footnote.** A face template used to *identify*
someone is GDPR Art. 9 special-category data; Illinois BIPA covers face geometry regardless of where
the processor sits and carries a private right of action with statutory damages *per violation*
(Texas CUBI and Washington have analogues); and under the EU AI Act, post-hoc remote biometric
identification is an Annex III high-risk category.

**3DAM's architecture is unusually well placed to mitigate — for 3DAM.** Local-first with no
telemetry (MISSION §7) means no controller→processor transfer by default; off-by-default runtime
flags ([ADR 0004](0004-feature-flags-admin.md)) mean the surface can be made to genuinely not exist;
the `asset_tag` `suggested`/`confirmed`/`rejected` lifecycle already models "a machine guess awaiting
human confirmation"; and #83's proposed federation exclusion and real-erasure guarantee are both
well-designed mitigations. None of this is hand-waving. It is also, as §3 below argues, aimed at the
wrong party's exposure.

**The build cost is epic-scale and the architecture does not absorb it quietly.** #83's own finding
holds: `embedding` is keyed `PRIMARY KEY (asset_id, space_id)` in `crates/3dam-store/src/schema.rs`
— one vector per asset per space — and a face is a *region*, with many per image. Faces therefore do
not fit the EmbeddingSpace seam and need their own tables, which at today's schema is a **V27**
migration (#83's body says V9; the schema has moved to V26 since it was written). On top of that:
a `faces` cargo feature and a `models/face/` weights-or-degrade contract following
`crates/3dam-core/src/semantic.rs`, an incremental density-based clustering pass that must never
rearrange already-labelled clusters, a People view with merge/split/reassign/reject, a `person:`
facet, Inspector overlays, a verifiable purge, and a permanent structural federation exclusion.

## Decision

**Decline.** 3DAM does not detect, embed, cluster, store, or search faces as identities. There is no
`FaceRecognition` flag, no `face`/`person` tables, no `faces` feature, and no People view.

Issue #83 is closed as **declined**, with this ADR as the rationale. The seven drafted children are
never filed. PRODUCT_SPEC §2 records this as a standing non-goal and §10 records the question as
settled; no phase in §9 gains a face entry and none is reserved for one.

The reasoning, in the order it carried weight:

1. **The underlying need is largely already served, and the residual delta is the expensive part.**
   "Every plate of this actor" decomposes into organisation problems 3DAM already solves: image
   similarity over the existing embedding space groups photos from the same shoot or subject usefully
   well without ever computing a face template; tags carry the same suggest/confirm/reject lifecycle
   #83 wants to reuse; collections, smart folders (live saved queries), the folder facet, notes, and
   FTS all express "everything of Alice" once a human says so once. What only a face template buys is
   *automatic identity clustering across unrelated sources with no human labelling*. That is a real
   increment — and it is precisely the increment that requires biometric data. We are being asked to
   pay the entire legal and maintenance cost for the last mile.

2. **The effort/impact ratio is recorded on the epic itself, and it is the worst on the board.**
   #83 carries `effort/13` (very large / epic) and `impact/3` (moderate). That does not settle a
   legal question, but it removes the "cheap enough to just build it" argument entirely, and it means
   every other consideration below is being weighed against a modest payoff.

3. **Off-by-default does not neutralise the exposure — it relocates it, onto our users.** Golden
   rule 4 is load-bearing and it works: flag off, and the detector never runs, the tables stay empty,
   the routes 404, the view does not exist. But a flag exists in order to be turned on, and the
   moment a studio turns it on, *the studio* becomes the entity collecting biometric identifiers.
   We would have handed them a pipeline with none of the apparatus that makes one lawful to operate:
   no publicly available retention-and-destruction policy (BIPA §15(a)), no consent capture or
   written-release tracking (§15(b)), no lawful-basis record for GDPR Art. 9. And per MISSION's
   *Who it's for*, our audience is "indie developers, small studios, tech artists, sound designers,
   and modders" — the population least equipped to build that apparatus for themselves. Shipping the
   capability without it is not neutral; building it is a different product.

4. **MIT licensing shields us everywhere else and specifically not here.** MISSION §7 commits to
   MIT, permanently — we give the software away and operate nothing, which is why 3DAM's regulatory
   surface is otherwise close to nil. The EU AI Act's free-and-open-source exemption (Art. 2(12))
   carves out exactly the categories this feature falls into: prohibited practices, high-risk
   systems, and Art. 50 transparency duties. The one structural property that most reduces our
   exposure across the whole product buys nothing for this one capability, and a provider placing a
   high-risk system on the EU market owes risk management, data governance, technical documentation,
   logging, human oversight, accuracy and robustness reporting, conformity assessment, and
   registration. That is a compliance programme, not a feature. **We are not lawyers and this is not
   legal advice** — but the analysis is not close enough that the uncertainty argues for proceeding.

5. **Known accuracy disparity is not answerable with UI framing.** #83 proposes to handle NIST FRVT's
   repeatedly documented demographic accuracy variation by presenting clusters as suggestions rather
   than identifications. That framing is correct, and it is necessary, and it is not sufficient: a
   system that performs reliably worse on some users' subjects performs worse for them whatever verb
   is on the button. We have no evaluation harness for that disparity and no plan to build one.
   Declining is the honest response to a known harm we cannot measure.

6. **It introduces a permanent invariant of unusually bad failure mode.** The federation exclusion is
   the sharp edge: "face spaces are structurally ineligible for fan-out and cross-peer similarity" is
   a rule that every future change to federation must not break — and federation (phase 6, shipped)
   is precisely the area where new query and fan-out paths keep appearing. Each new one becomes a
   place where a bug transfers biometric templates to a third party. 3DAM already carries one global
   invariant of this severity (non-destructive writes, golden rule 3) and pays for it in every
   convert/export change. Acquiring a second, with a worst case measured in statutory damages rather
   than wrong search results, for a moderate-impact feature, is a bad trade.

7. **This is not a scoping oversight to be corrected.** MISSION principle 2 says the computer looks
   at "the waveform, the pixels, the geometry". PRODUCT_SPEC §2 already declines general-purpose DAM
   ambitions, and §9 phase 2b holds video and documents deliberately shallow rather than letting
   adjacent media types pull the product outward. Faces are not a deeper cut of an existing media
   type; they are a different *subject matter* — people — arriving inside an asset manager. Golden
   rule 2 says the spec decides, the spec has never contemplated biometrics, and the correct
   resolution of "the spec does not cover this" is a decision. This is it.

## Consequences

- **#83 is closed as declined**, citing this ADR, and the seven drafted children are never filed.
  The tracker stops carrying a biometrics epic that reads as planned work.
- **PRODUCT_SPEC §2 gains a standing non-goal and §10 records the question as settled.** §9 is
  untouched: no phase gains a face entry, and no phase reserves one. A future reader who wonders why
  finds this ADR from either index.
- **No V27 face/person migration**, and `embedding`'s `PRIMARY KEY (asset_id, space_id)` shape stays
  exactly as it is. #83's architectural finding — that faces cannot live in that table — remains
  true and is now moot rather than a design constraint we have to accommodate.
- **No `faces` cargo feature, no `models/face/` weights convention, no `FaceRecognition` flag.**
  `semantic`'s weights-present-or-degrade contract keeps SigLIP and CLAP as its only tenants, and
  the flag surface does not grow a member whose "off" state carries legal significance.
- **Federation keeps exactly one class of exportable vector** — the declared `space_id` negotiated at
  `advertise()` ([ADR 0009 §5](0009-v1-scope-decisions.md), [ADR 0016](0016-vector-index-backend.md)).
  Cross-peer similarity never acquires a category of space that must never be sent, which means no
  future federation change has to be audited against that rule. This is a simplification we keep by
  not spending it.
- **Person-level organisation is not blocked, just not automated.** Tags with the existing suggestion
  lifecycle, collections, smart folders, and the folder/FTS facets already express "every plate of
  Alice" once a human labels it once, and image similarity gets a user most of the way to the
  labelling. That is the documented answer, not a silent absence.
- **Reversal is cheap.** Nothing is built that would have to be unbuilt, no schema version is
  consumed, no API surface is reserved. If the conditions below change, this ADR is superseded by a
  new one — it does not have to be undone.

## What would reopen this

Deliberately concrete, so that re-raising it is a check against criteria rather than a re-litigation:

- **A named commercial user whose reference library is large enough that the manual paths above
  measurably fail, and who already has their own legal posture** — releases on file, a retention
  policy, a lawful basis. Demand from someone who has solved the consent problem, not a hypothetical
  user who would inherit it from us.
- **Evidence that a detection-only capability serves the actual need**: bounding boxes and a
  "contains identifiable people" signal, with **no template, no clustering, and no identity**, aimed
  at the licence and usage-rights surface ("which of my source photos need a model release before
  this ships") rather than at search. That is a *different* capability with a *different* analysis —
  it creates no biometric identifier — and it would need **its own issue and its own ADR**. It is
  explicitly **not authorised by this one**. It is named here as a pointer, not offered as a
  consolation prize, and it must not be treated as a partial "yes".
- **A material change in the regulatory picture**, or the emergence of a maintained
  consent/retention scaffolding layer that would make handing a small studio a biometric pipeline a
  responsible act rather than a reckless one.

Absent those, the answer stays no.

## Alternatives considered

- **Accept #83 as specified** — flag, structural federation exclusion, verifiable erasure,
  suggestion-not-identification framing. The mitigations are individually well designed and this was
  a genuine option, not a formality. Rejected because they reduce *3DAM's* exposure while leaving the
  *user's* untouched (§3), because MIT + local-first buys nothing under the AI Act's FOSS carve-out
  (§4), and because an `effort/13` / `impact/3` feature cannot pay for a permanent invariant whose
  failure mode is a biometric-data transfer (§6).
- **Accept, but as a source-available cargo feature never enabled in release binaries.** Superficially
  a way to have it both ways. Rejected: an unshipped code path is unmaintained and untested, which is
  its own failure mode, and building the capability still places it on the market for anyone who
  compiles it. It buys deniability, not safety.
- **Defer without deciding** — leave #83 open under "Beyond v1". This is the status quo, and it is
  the worst option available: it keeps a biometrics epic sitting in the tracker as apparently-planned
  work, accumulating exactly the momentum #180 was filed to interrupt. A decision in *either*
  direction was the acceptance criterion.
- **Answer with detection-only instead** — decide here that 3DAM will flag images containing people
  without ever identifying them. Rejected *as an answer to this ADR*: #180 asks a specific question,
  and answering an adjacent, easier one is how scope creep gets laundered through a gate. It is
  recorded above as a reopening condition with a gate of its own, which is where it belongs.
