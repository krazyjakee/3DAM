//! Text-search query building: filename tokenization, FTS5 MATCH expression construction, and
//! synonym/alias expansion (semantic-search M1–M3).
//!
//! The v1 catalog searched `filename LIKE '%term%'` — an unindexed full scan with no word matching
//! or ranking. This module turns a user's raw query text into an FTS5 MATCH expression over the
//! `asset_fts` index (M1), optionally widened with curated synonyms so "gun" also surfaces an
//! "ak47" (M3). The same [`tokenize_name`] splitter feeds the `tokens` column the analyse pass
//! writes (M2), so an embedded term like the `ak47` in `ak47_lowpoly.fbx` is a first-class token.

use std::collections::HashMap;
use std::path::Path;

/// The ranking expression for the `asset_fts` index, with **explicit per-column weights**.
///
/// Column order matches the V12 index: `filename, tokens, tags, note, text`. More negative =
/// better, so a larger weight pulls a match toward the top:
/// - `filename` 10 — the user typed a name; the file called that is the answer.
/// - `tokens`    5 — filename-derived sub-tokens (`ak47` inside `ak47_lowpoly.fbx`), the same
///   intent one step removed.
/// - `tags`      4 — curated/accepted labels, deliberate but not what was typed.
/// - `note`      3 — the user's own prose about this asset: authored, but about it rather than
///   naming it.
/// - `text`      1 — extracted body prose: the widest recall and the weakest per-hit evidence.
///
/// These order results *within* a tier. They are deliberately **not** relied on to keep documents
/// off the top — see [`AUTHORED_TIER`] for why that needs more than a weight.
pub(crate) const FTS_RANK: &str = "bm25(asset_fts, 10.0, 5.0, 4.0, 3.0, 1.0)";

/// The **primary** sort key: 0 for a row matching in an *authored* column, 1 for an
/// extracted-body-text-only match. Any filename/token/tag/note hit therefore outranks every
/// text-only hit, always.
///
/// This exists because column weighting alone cannot do the job, which is worth recording so
/// nobody "simplifies" it back. bm25 saturates term frequency (the `k1` term): once a document
/// mentions a word enough times, its score asymptotically approaches the same ceiling a
/// single-token filename match reaches, and no finite weight separates them reliably — measured on
/// the real index, a 200-mention document still beat `kick.wav` at a filename weight of 10, and
/// only flipped once the text weight was pushed below ~0.2. That threshold is a function of the
/// corpus, not a constant, so it would silently stop holding as a library grows. Worse, bm25's IDF
/// term collapses toward zero for a term present in most matching rows, which is precisely the
/// situation in a small or topically-narrow library.
///
/// An explicit tier is categorical instead of statistical: it cannot be defeated by repetition,
/// corpus size, or term distribution. Recall is untouched — the document still matches and still
/// appears, it just appears below the file that is actually named after the query.
///
/// The tier is *authored*, not merely name-ish: a user's `note` (issue #81) joins filename, tokens,
/// and tags on the near side of the line. Someone who typed "client rejected this variant" onto an
/// asset said something deliberate about it, and that should not sort below a 40-page PDF that
/// happens to contain the word "variant" — which is precisely the failure this tier exists to stop.
///
/// `{col1 col2} : (expr)` is FTS5's column-filter syntax; the expression is parenthesised because
/// the filter binds to the term that follows it, not to a whole boolean chain.
pub(crate) const AUTHORED_TIER: &str =
    "CASE WHEN asset.rowid IN (SELECT rowid FROM asset_fts WHERE asset_fts MATCH ?) THEN 0 ELSE 1 END ASC";

/// Wrap a MATCH expression so it only searches the authored columns (see [`AUTHORED_TIER`]).
pub(crate) fn authored_scoped(match_expr: &str) -> String {
    format!("{{filename tokens tags note}} : ({match_expr})")
}

/// Split a filename (or free text) into lowercase search tokens. Splits on non-alphanumeric runs
/// *and* on camelCase / letter⇄digit boundaries, so `AK47_LowPoly.fbx` yields
/// `ak47, ak, 47, low, poly, fbx`. The original run is kept alongside its sub-splits so both an
/// exact `ak47` and a component `47` match. Deduped, order-preserving.
pub fn tokenize_name(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |s: String, out: &mut Vec<String>| {
        if s.len() >= 2 && !out.contains(&s) {
            out.push(s);
        }
    };
    // First split on any non-alphanumeric into raw runs (unicode-aware).
    for run in name.split(|c: char| !c.is_alphanumeric()) {
        if run.is_empty() {
            continue;
        }
        let lower = run.to_lowercase();
        push(lower.clone(), &mut out);
        // Then break each run on camelCase and letter/digit boundaries into sub-tokens.
        for sub in split_boundaries(run) {
            push(sub, &mut out);
        }
    }
    out
}

/// Break one alphanumeric run into sub-tokens at camelCase humps and letter⇄digit transitions.
fn split_boundaries(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    let mut subs: Vec<String> = Vec::new();
    let mut cur = String::new();
    for i in 0..chars.len() {
        let c = chars[i];
        if i > 0 {
            let p = chars[i - 1];
            let hump = p.is_lowercase() && c.is_uppercase(); // fooBar → foo|Bar
            let digit_edge = p.is_alphabetic() != c.is_alphabetic()
                && (p.is_ascii_digit() || c.is_ascii_digit()); // ak|47
            if hump || digit_edge {
                if !cur.is_empty() {
                    subs.push(cur.to_lowercase());
                }
                cur = String::new();
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        subs.push(cur.to_lowercase());
    }
    subs
}

/// A curated set of related-term groups used to widen a query (M3). Expansion is symmetric within a
/// group: searching any member widens to the whole group, so "gun" finds "ak47" and vice-versa.
/// Seeded with a small built-in vocabulary and extended from `synonyms.txt` in the data dir (one
/// comma-separated group per line, `#` comments) so a user can grow it without a rebuild.
#[derive(Clone, Default)]
pub struct SynonymMap {
    /// term → indices of the groups it belongs to.
    index: HashMap<String, Vec<usize>>,
    groups: Vec<Vec<String>>,
}

impl SynonymMap {
    /// The built-in starter vocabulary. Deliberately small and game-asset flavoured; the real depth
    /// comes from the user's `synonyms.txt` and, later, model-backed semantics (M4/M5).
    pub fn builtin() -> Self {
        const GROUPS: &[&str] = &[
            "gun,guns,firearm,firearms,weapon,weapons,rifle,pistol,revolver,handgun,ak47,ak,m4,m16,smg,shotgun,sniper",
            "sword,blade,katana,dagger,knife,machete,melee",
            "car,vehicle,truck,automobile,sedan,suv,van",
            "tree,foliage,plant,bush,shrub,vegetation",
            "rock,stone,boulder,cliff,gravel",
            "brick,bricks,masonry,wall",
            "wood,timber,plank,planks,bark,log",
            "metal,steel,iron,rust,rusty,rusted",
            "grass,turf,lawn,meadow",
            "water,ocean,sea,liquid,fluid,wave",
            "footstep,footsteps,foot,step,steps,walk",
            "explosion,explode,blast,boom,detonation",
            "music,song,track,tune,melody,soundtrack",
            "ambient,ambience,atmosphere,background,drone",
            "voice,vocal,speech,dialogue,dialog",
            "loop,looping,looped,seamless",
            "character,char,hero,npc,person,human,humanoid",
            "building,house,structure,architecture",
        ];
        let mut m = SynonymMap::default();
        for line in GROUPS {
            m.add_group(line);
        }
        m
    }

    /// Built-in vocabulary plus any user groups from `<data_dir>/synonyms.txt` (best-effort: a
    /// missing or malformed file just leaves the built-ins).
    pub fn load(data_dir: &Path) -> Self {
        let mut m = Self::builtin();
        if let Ok(text) = std::fs::read_to_string(data_dir.join("synonyms.txt")) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                m.add_group(line);
            }
        }
        m
    }

    fn add_group(&mut self, csv: &str) {
        let terms: Vec<String> = csv
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if terms.len() < 2 {
            return;
        }
        let gi = self.groups.len();
        for t in &terms {
            self.index.entry(t.clone()).or_default().push(gi);
        }
        self.groups.push(terms);
    }

    /// The widened term set for one query token: the token itself plus every co-grouped term.
    /// Deduped; always contains the input.
    pub fn expand(&self, term: &str) -> Vec<String> {
        let term = term.to_lowercase();
        let mut out = vec![term.clone()];
        if let Some(gis) = self.index.get(&term) {
            for &gi in gis {
                for t in &self.groups[gi] {
                    if !out.contains(t) {
                        out.push(t.clone());
                    }
                }
            }
        }
        out
    }
}

/// Build an FTS5 MATCH expression from raw query text, widened by `syn` (M1 + M3). Each query token
/// becomes an OR-group of quoted prefix terms (the token + its synonyms); the groups are AND-ed, so
/// `"AK47 metal"` requires a token from each group but "gun" alone expands to the whole weapon set.
/// Returns `None` when the text yields no usable term (all punctuation) — the caller then falls back
/// to a `LIKE` scan.
pub fn fts_match_expr(text: &str, syn: &SynonymMap) -> Option<String> {
    // Use the raw whitespace/punctuation split for the *query* (not the camelCase splitter): a user
    // typing "ak47" wants the whole token, and prefix matching handles the rest.
    let tokens: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let groups: Vec<String> = tokens
        .iter()
        .map(|tok| {
            let terms: Vec<String> = syn.expand(tok).iter().map(|t| prefix_term(t)).collect();
            if terms.len() == 1 {
                terms.into_iter().next().unwrap()
            } else {
                format!("({})", terms.join(" OR "))
            }
        })
        .collect();
    Some(groups.join(" AND "))
}

/// One FTS5 prefix term: the token quoted (so digits/keywords never confuse the parser) then `*`.
/// Any embedded double-quote is doubled per FTS5 string rules.
fn prefix_term(tok: &str) -> String {
    format!("\"{}\"*", tok.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_filename_parts() {
        let t = tokenize_name("AK47_LowPoly.fbx");
        assert!(t.contains(&"ak47".to_string()), "keeps whole run: {t:?}");
        assert!(t.contains(&"ak".to_string()), "splits letter/digit: {t:?}");
        assert!(t.contains(&"47".to_string()));
        assert!(t.contains(&"low".to_string()), "splits camelCase: {t:?}");
        assert!(t.contains(&"poly".to_string()));
        assert!(t.contains(&"fbx".to_string()));
    }

    #[test]
    fn synonyms_are_symmetric() {
        let syn = SynonymMap::builtin();
        let from_gun = syn.expand("gun");
        assert!(
            from_gun.contains(&"ak47".to_string()),
            "gun→ak47: {from_gun:?}"
        );
        let from_ak = syn.expand("ak47");
        assert!(
            from_ak.contains(&"gun".to_string()),
            "ak47→gun: {from_ak:?}"
        );
    }

    #[test]
    fn match_expr_expands_and_quotes() {
        let syn = SynonymMap::builtin();
        let expr = fts_match_expr("gun", &syn).unwrap();
        assert!(expr.contains("\"gun\"*"));
        assert!(expr.contains("\"ak47\"*"), "expanded: {expr}");
        assert!(expr.starts_with('(') && expr.contains(" OR "));
        // Multi-token AND.
        let expr2 = fts_match_expr("metal barrel", &syn).unwrap();
        assert!(expr2.contains(" AND "), "{expr2}");
        // Punctuation-only → no expression.
        assert!(fts_match_expr("...", &syn).is_none());
    }
}
