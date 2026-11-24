use crate::{Trust, open_db};
pub use ai4math_core::{ClaimEngine, WikiStratum};
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, Transaction, TransactionBehavior, params, params_from_iter, types::Value,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

mod sediment;
pub use sediment::{
    SedimentArtifact, SedimentInput, StoredSediment, publish_sediment, read_sediment,
    render_sediment,
};

const DDL: &str = include_str!("../../../schemas/wiki-index.sql");
const STRATA: [WikiStratum; 3] = [
    WikiStratum::Core,
    WikiStratum::Sediment,
    WikiStratum::Graveyard,
];
const HIGH_SCORE: f64 = 0.82;
const MAX_PROVENANCE_DEPTH: u32 = 16;
const MAX_PROVENANCE_EDGES: u32 = 256;
// No dates are required by the Markdown format. Use a stable unknown-date sentinel
// rather than deriving facts from checkout mtimes or the time of index rebuild.
const UNKNOWN_DATE: &str = "1970-01-01T00:00:00Z";

// Superseded versions remain inspectable in Sediment, but Core is the live view.
// Neither positive-recall stratum can contain refutations, regardless of pinning.
fn positive_statuses(stratum: WikiStratum) -> Option<&'static [&'static str]> {
    match stratum {
        WikiStratum::Core => Some(&["active", "unreferenced"]),
        WikiStratum::Sediment => Some(&["active", "unreferenced", "superseded"]),
        WikiStratum::Graveyard => None,
    }
}

pub struct Wiki {
    db: Connection,
}

#[derive(Debug, Clone, Copy)]
pub struct WikiQuery<'a> {
    /// Literal search words (not FTS5 query syntax). Blank text can browse the selected scope.
    pub query: &'a str,
    pub scope: WikiStratum,
    pub engine: Option<ClaimEngine>,
    pub conclusion_head: Option<&'a str>,
    pub hypothesis_heads: &'a [&'a str],
    pub typeclasses: &'a [&'a str],
    pub include_shadow_hint: bool,
    pub limit: u32,
}

impl<'a> WikiQuery<'a> {
    pub fn new(query: &'a str) -> Self {
        Self {
            query,
            scope: WikiStratum::Core,
            engine: None,
            conclusion_head: None,
            hypothesis_heads: &[],
            typeclasses: &[],
            include_shadow_hint: true,
            limit: 20,
        }
    }
}

impl Default for WikiQuery<'_> {
    fn default() -> Self {
        Self::new("")
    }
}

#[derive(Debug, Serialize)]
pub struct WikiSearchResult {
    pub hits: Vec<WikiHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_hint: Option<ShadowHint>,
}

#[derive(Debug, Serialize)]
pub struct WikiHit {
    #[serde(flatten)]
    pub summary: WikiHitSummary,
    pub stratum: WikiStratum,
    pub trust_level: Trust,
    pub status: String,
    pub informal_gloss: String,
    pub formal_signature: Option<String>,
    /// Path relative to the Wiki root, not the runtime database or process cwd.
    pub file_path: String,
}

#[derive(Debug, Serialize)]
pub struct WikiHitSummary {
    pub id: String,
    pub title: String,
    pub engine: ClaimEngine,
    pub verified_at_commit: Option<String>,
    /// Match-confidence heuristic, not a probability or numerical BM25 score.
    pub score: f64,
}

/// The cold-stratum teaser deliberately has no statement, signature, source path or notes.
#[derive(Debug, Default, Serialize)]
pub struct ShadowHint {
    pub sediment_count: usize,
    pub top_sediment: Option<WikiHitSummary>,
    pub graveyard_count: usize,
    pub top_graveyard_id: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct WikiProvenance {
    pub edges: Vec<WikiProvenanceEdge>,
    /// Whether the edge limit omitted any edges within the requested depth.
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct WikiProvenanceEdge {
    pub src_id: String,
    pub dst_id: String,
    pub src_stratum: WikiStratum,
    /// Absent if the destination is not indexed in this Wiki (e.g. an origin claim).
    pub dst_stratum: Option<WikiStratum>,
    pub edge_type: String,
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// Hops needed to encounter this edge from the root, starting at 1.
    pub depth: u32,
}

impl Wiki {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            db: open_db(path.as_ref(), DDL)?,
        })
    }

    /// Replace the disposable index from the Git-tracked Markdown files as one transaction.
    /// Missing strata are empty (Git does not track empty directories). Invalid input leaves
    /// the previously published index intact. This never writes or deletes a Markdown file.
    pub fn rebuild(&mut self, root: impl AsRef<Path>) -> Result<usize> {
        let root = root.as_ref();
        ensure!(
            root.is_dir(),
            "Wiki root is not a directory: {}",
            root.display()
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("DELETE FROM entries_fts; DELETE FROM entries;")?;
        let mut indexed = 0;
        for stratum in STRATA {
            let dir = root.join(stratum.as_str());
            match fs::symlink_metadata(&dir) {
                Ok(meta) => ensure!(
                    meta.is_dir(),
                    "invalid Wiki stratum directory: {}",
                    dir.display()
                ),
                Err(err) if err.kind() == ErrorKind::NotFound => continue,
                Err(err) => return Err(err).with_context(|| format!("inspect {}", dir.display())),
            }
            for path in markdown_files(&dir)? {
                let relative = path
                    .strip_prefix(root)?
                    .to_str()
                    .context("Wiki path is not UTF-8")?;
                (|| -> Result<()> {
                    let source = fs::read_to_string(&path)?;
                    let (entry, body) = frontmatter(&source)?;
                    ensure!(
                        entry.stratum == stratum,
                        "frontmatter stratum does not match the directory"
                    );
                    insert_entry(&tx, &entry, body, relative)
                })()
                .with_context(|| format!("index {}", path.display()))?;
                indexed += 1;
            }
        }
        tx.commit()?;
        Ok(indexed)
    }

    /// Search exactly one stratum. CORE queries can report cold matches without exposing
    /// their text. Graveyard warnings do not depend on how many CORE results match.
    /// The offline search ranks exact symbol filters and literal FTS5 matches without embeddings.
    pub fn search(&self, query: WikiQuery<'_>) -> Result<WikiSearchResult> {
        let search = Search::new(query);
        // Read hits and hints from the same published rebuild, even with another connection.
        let db = self.db.unchecked_transaction()?;
        let core = search.candidates(&db, query.scope, false, false, query.limit.max(1))?;
        let mut hint = ShadowHint::default();
        if query.scope == WikiStratum::Core && query.include_shadow_hint && search.has_terms() {
            if core.high_count < 2 {
                let sediment = search.candidates(&db, WikiStratum::Sediment, true, false, 1)?;
                hint.sediment_count = sediment.high_count;
                hint.top_sediment = sediment.rows.into_iter().next();
            }
            let graveyard = search.candidates(&db, WikiStratum::Graveyard, true, true, 1)?;
            hint.graveyard_count = graveyard.high_count;
            hint.top_graveyard_id = graveyard.rows.into_iter().next().map(|row| row.id);
        }
        let mut hits = Vec::new();
        {
            // Only fetch body-derived fields for explicit hits in the requested stratum.
            let mut details = db.prepare_cached("SELECT trust_level,status,informal_gloss,formal_signature,file_path FROM entries WHERE id=?1")?;
            for summary in core.rows.into_iter().take(query.limit as usize) {
                let (trust, status, informal_gloss, formal_signature, file_path): (
                    String,
                    _,
                    _,
                    _,
                    _,
                ) = details.query_row([&summary.id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?;
                hits.push(WikiHit {
                    summary,
                    stratum: query.scope,
                    trust_level: trust.parse()?,
                    status,
                    informal_gloss,
                    formal_signature,
                    file_path,
                });
            }
        }
        db.commit()?;
        Ok(WikiSearchResult {
            hits,
            shadow_hint: (hint.sediment_count > 0 || hint.graveyard_count > 0).then_some(hint),
        })
    }

    /// Explicitly explore provenance across strata without expanding normal search results.
    /// Follow incoming and outgoing edges breadth-first, preserving each edge's orientation
    /// and emitting it only once. The root may be a Wiki ID or an external endpoint ID.
    /// Accepts at most 16 hops and 256 edges; depth 0 returns no edges.
    pub fn provenance(&self, id: &str, max_depth: u32, limit: u32) -> Result<WikiProvenance> {
        ensure!(
            !id.trim().is_empty(),
            "Wiki provenance root ID cannot be blank"
        );
        ensure!(
            max_depth <= MAX_PROVENANCE_DEPTH,
            "Wiki provenance depth must be at most {MAX_PROVENANCE_DEPTH}"
        );
        ensure!(
            limit <= MAX_PROVENANCE_EDGES,
            "Wiki provenance edge limit must be at most {MAX_PROVENANCE_EDGES}"
        );

        let db = self.db.unchecked_transaction()?;
        let mut result = WikiProvenance::default();
        {
            let mut incident = db.prepare_cached(
                "SELECT e.src_id,e.dst_id,e.edge_type,e.metadata_json,src.stratum,dst.stratum
                FROM entry_edges e JOIN entries src ON src.id=e.src_id
                LEFT JOIN entries dst ON dst.id=e.dst_id
                WHERE e.src_id=?1 OR e.dst_id=?1
                ORDER BY e.src_id,e.dst_id,e.edge_type LIMIT ?2",
            )?;
            let mut visited = BTreeSet::from([id.to_owned()]);
            let mut pending = VecDeque::from([(id.to_owned(), 0)]);
            let mut seen_edges = BTreeSet::new();
            'traverse: while let Some((current, depth)) = pending.pop_front() {
                if depth == max_depth {
                    break;
                }
                // At most limit edges can have been seen before this query. The extra row
                // is enough to find the first omitted edge even when revisiting edges.
                let mut rows = incident.query(params![current, limit + 1])?;
                while let Some(row) = rows.next()? {
                    let src_id: String = row.get(0)?;
                    let dst_id: String = row.get(1)?;
                    let edge_type: String = row.get(2)?;
                    if !seen_edges.insert((src_id.clone(), dst_id.clone(), edge_type.clone())) {
                        continue;
                    }
                    if result.edges.len() == limit as usize {
                        result.truncated = true;
                        break 'traverse;
                    }
                    let neighbor = if src_id == current { &dst_id } else { &src_id };
                    if visited.insert(neighbor.clone()) {
                        pending.push_back((neighbor.clone(), depth + 1));
                    }
                    result.edges.push(WikiProvenanceEdge {
                        src_id,
                        dst_id,
                        src_stratum: row.get::<_, String>(4)?.parse()?,
                        dst_stratum: row
                            .get::<_, Option<String>>(5)?
                            .map(|s| s.parse())
                            .transpose()?,
                        edge_type,
                        metadata: serde_json::from_str(row.get_ref(3)?.as_str()?)
                            .context("invalid indexed Wiki edge metadata")?,
                        depth: depth + 1,
                    });
                }
            }
        }
        db.commit()?;
        Ok(result)
    }
}

#[derive(Deserialize)]
struct Frontmatter {
    id: String,
    stratum: WikiStratum,
    trust_level: Trust,
    engine: ClaimEngine,
    status: String,
    #[serde(default)]
    pinned: bool,
    title: Option<String>,
    informal_gloss: Option<String>,
    formal_signature: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    verified_at_commit: Option<String>,
    origin_branch: Option<String>,
    origin_claim: Option<String>,
    #[serde(default)]
    def_fingerprints: BTreeMap<String, String>,
    compat_status: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    #[serde(default)]
    symbols: Symbols,
    #[serde(default)]
    edges: Vec<Edge>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Symbols {
    conclusion_head: Option<String>,
    hypothesis_heads: Vec<String>,
    typeclasses: Vec<String>,
    constants: Vec<String>,
}

#[derive(Deserialize)]
struct Edge {
    dst_id: String,
    edge_type: String,
    #[serde(default)]
    metadata: BTreeMap<String, serde_json::Value>,
}

fn markdown_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                dirs.push(path);
            } else if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            {
                ensure!(
                    kind.is_file(),
                    "Wiki Markdown entry is not a regular file: {}",
                    path.display()
                );
                files.push(path);
            }
        }
    }
    files.sort_unstable();
    Ok(files)
}

fn frontmatter(source: &str) -> Result<(Frontmatter, &str)> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut lines = source.split_inclusive('\n');
    let opening = lines.next().context("empty Wiki Markdown entry")?;
    ensure!(
        opening.trim_end() == "---",
        "Wiki entry must begin with YAML frontmatter"
    );
    let mut end = opening.len();
    for line in lines {
        if line.trim_end() == "---" {
            let entry = serde_yaml::from_str(&source[opening.len()..end])
                .context("invalid Wiki frontmatter")?;
            return Ok((entry, &source[end + line.len()..]));
        }
        end += line.len();
    }
    anyhow::bail!("unterminated Wiki frontmatter")
}

#[derive(Default)]
struct Body {
    title: Option<String>,
    informal: String,
    artifact: String,
    notes: String,
}

impl Body {
    // Recognize the documented H2 sections and an optional H1 title; fenced code
    // may contain Lean/Agda comments that look like Markdown headings.
    fn parse(markdown: &str) -> Self {
        #[derive(Clone, Copy)]
        enum Section {
            Informal,
            Artifact,
            Notes,
        }
        let mut body = Self::default();
        let mut section = Section::Notes;
        let mut fence: Option<(char, usize)> = None;
        for line in markdown.split_inclusive('\n') {
            let text = line.trim();
            if let Some(marker @ ('`' | '~')) = text.chars().next() {
                let n = text.chars().take_while(|&ch| ch == marker).count();
                if n >= 3 {
                    if let Some((open, min)) = fence {
                        if marker == open && n >= min && text[n..].trim().is_empty() {
                            fence = None;
                        }
                    } else {
                        fence = Some((marker, n));
                    }
                }
            } else if fence.is_none() {
                if let Some(title) = text.strip_prefix("# ") {
                    if body.title.is_none() && !title.trim().is_empty() {
                        body.title = Some(title.trim().to_owned());
                    }
                    section = Section::Notes;
                    continue;
                }
                if let Some(heading) = text.strip_prefix("## ") {
                    section = match heading.trim().to_ascii_lowercase().as_str() {
                        "informal statement" => Section::Informal,
                        "formal artifact" => Section::Artifact,
                        _ => Section::Notes,
                    };
                    continue;
                }
            }
            match section {
                Section::Informal => &mut body.informal,
                Section::Artifact => &mut body.artifact,
                Section::Notes => &mut body.notes,
            }
            .push_str(line);
        }
        body
    }
}

fn insert_entry(
    tx: &Transaction<'_>,
    entry: &Frontmatter,
    markdown: &str,
    relative: &str,
) -> Result<()> {
    ensure!(!entry.id.trim().is_empty(), "Wiki entry ID cannot be blank");
    if let Some(statuses) = positive_statuses(entry.stratum) {
        ensure!(
            entry.trust_level != Trust::L2b && statuses.contains(&entry.status.as_str()),
            "Wiki entry {} with trust {} and status {} is not valid in stratum {}",
            entry.id,
            entry.trust_level.as_str(),
            entry.status,
            entry.stratum.as_str(),
        );
    }
    let body = Body::parse(markdown);
    let title = entry
        .title
        .as_deref()
        .or(body.title.as_deref())
        .unwrap_or(&entry.id);
    let gloss = entry
        .informal_gloss
        .as_deref()
        .unwrap_or(body.informal.trim());
    let created = entry.created_at.as_deref().unwrap_or(UNKNOWN_DATE);
    let updated = entry.updated_at.as_deref().unwrap_or(created);
    tx.execute("INSERT INTO entries (id,stratum,trust_level,engine,status,pinned,title,informal_gloss,
        formal_signature,tags_json,verified_at_commit,origin_branch,origin_claim,def_fingerprints_json,
        compat_status,created_at,updated_at,file_path)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        params![entry.id,entry.stratum.as_str(),entry.trust_level.as_str(),entry.engine.as_str(),entry.status,
            entry.pinned,title,gloss,entry.formal_signature,serde_json::to_string(&entry.tags)?,entry.verified_at_commit,
            entry.origin_branch,entry.origin_claim,serde_json::to_string(&entry.def_fingerprints)?,
            entry.compat_status.as_deref().unwrap_or("unknown"),created,updated,relative])?;
    tx.execute("INSERT INTO entries_fts (entry_id,stratum,title,informal_gloss,tags,formal_signature,notes)
        VALUES (?1,?2,?3,?4,?5,?6,?7)", params![entry.id,entry.stratum.as_str(),title,gloss,entry.tags.join(" "),
            entry.formal_signature,format!("{}\n{}",body.artifact,body.notes)])?;

    let mut symbols = tx.prepare_cached(
        "INSERT INTO entry_symbols (entry_id,role,symbol) VALUES (?1,?2,?3)
        ON CONFLICT(entry_id,role,symbol) DO NOTHING",
    )?;
    for (role, values) in [
        ("conclusion_head", entry.symbols.conclusion_head.as_slice()),
        ("hypothesis_head", &entry.symbols.hypothesis_heads),
        ("typeclass", &entry.symbols.typeclasses),
        ("const", &entry.symbols.constants),
    ] {
        for symbol in values {
            ensure!(!symbol.trim().is_empty(), "blank Wiki {role} symbol");
            symbols.execute(params![entry.id, role, symbol])?;
        }
    }
    let mut edges = tx.prepare_cached(
        "INSERT INTO entry_edges (src_id,dst_id,edge_type,metadata_json) VALUES (?1,?2,?3,?4)",
    )?;
    for edge in &entry.edges {
        ensure!(
            !edge.dst_id.trim().is_empty(),
            "blank Wiki edge destination"
        );
        edges.execute(params![
            entry.id,
            edge.dst_id,
            edge.edge_type,
            serde_json::to_string(&edge.metadata)?
        ])?;
    }
    if let Some(claim) = &entry.origin_claim
        && !entry
            .edges
            .iter()
            .any(|edge| edge.edge_type == "born_from_claim" && edge.dst_id == *claim)
    {
        edges.execute(params![entry.id, claim, "born_from_claim", "{}"])?;
    }
    Ok(())
}

struct Search<'a> {
    fts: String,
    symbols: BTreeSet<(&'static str, &'a str)>,
    query: WikiQuery<'a>,
}

struct Candidates {
    rows: Vec<WikiHitSummary>,
    high_count: usize,
}

impl<'a> Search<'a> {
    fn new(query: WikiQuery<'a>) -> Self {
        // Convert text to quoted literal tokens. Operator characters and words cannot
        // introduce FTS syntax; namespaces such as LE.le match unicode61 tokenization.
        let mut seen = BTreeSet::new();
        let fts = query
            .query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty() && seen.insert(*word))
            .map(|word| format!("\"{word}\""))
            .collect::<Vec<_>>()
            .join(" AND ");
        let symbols = query
            .conclusion_head
            .into_iter()
            .map(|s| ("conclusion_head", s))
            .chain(
                query
                    .hypothesis_heads
                    .iter()
                    .map(|&s| ("hypothesis_head", s)),
            )
            .chain(query.typeclasses.iter().map(|&s| ("typeclass", s)))
            .collect();
        Self {
            fts,
            symbols,
            query,
        }
    }

    fn has_terms(&self) -> bool {
        !self.fts.is_empty() || !self.symbols.is_empty()
    }

    fn candidates(
        &self,
        db: &Connection,
        stratum: WikiStratum,
        only_high: bool,
        refuted_only: bool,
        limit: u32,
    ) -> Result<Candidates> {
        let mut result = Candidates {
            rows: Vec::new(),
            high_count: 0,
        };
        if !self.has_terms() && !self.query.query.trim().is_empty() {
            return Ok(result);
        }
        let mut args = Vec::<Value>::new();
        let allowed_statuses = positive_statuses(stratum);
        let stratum = stratum.as_str();
        let mut sql = if self.fts.is_empty() {
            String::from(
                "WITH lexical(entry_id,rank) AS (SELECT NULL,NULL WHERE 0), strong(entry_id) AS (SELECT NULL WHERE 0)",
            )
        } else {
            args.extend([
                self.fts.clone().into(),
                stratum.to_owned().into(),
                format!(
                    "{{title informal_gloss tags formal_signature}} : ({})",
                    self.fts
                )
                .into(),
                stratum.to_owned().into(),
            ]);
            // Materialize BM25 at the FTS cursor (rather than evaluating it on outer-join rows).
            String::from("WITH lexical AS MATERIALIZED (
                SELECT entry_id,bm25(entries_fts,0,0,4,2,2,2,1) AS rank FROM entries_fts WHERE entries_fts MATCH ? AND stratum=?
            ), strong AS (SELECT entry_id FROM entries_fts WHERE entries_fts MATCH ? AND stratum=?)")
        };
        let symbolic = if self.query.conclusion_head.is_some() {
            0.95
        } else if self.symbols.len() >= 2 {
            0.85
        } else if self.symbols.is_empty() {
            0.0
        } else {
            0.65
        };
        let combined = if self.symbols.is_empty() { 0.85 } else { 1.0 };
        sql.push_str(", matches AS (SELECT e.id,e.title,e.engine,e.verified_at_commit,l.rank,
            CASE WHEN s.entry_id IS NOT NULL THEN ? WHEN l.entry_id IS NOT NULL THEN max(?,0.65) ELSE ? END AS score
            FROM entries e LEFT JOIN lexical l ON l.entry_id=e.id LEFT JOIN strong s ON s.entry_id=e.id WHERE e.stratum=?");
        args.extend([
            combined.into(),
            symbolic.into(),
            symbolic.into(),
            stratum.to_owned().into(),
        ]);
        if let Some(engine) = self.query.engine {
            sql.push_str(" AND e.engine=?");
            args.push(engine.as_str().to_owned().into());
        }
        // Old or manually edited indexes may predate the rebuild-time checks.
        // Apply the same eligibility rule before ranking or counting any matches.
        if let Some(statuses) = allowed_statuses {
            sql.push_str(" AND e.trust_level<>? AND e.status IN (");
            args.push(Trust::L2b.as_str().to_owned().into());
            for (i, status) in statuses.iter().enumerate() {
                sql.push_str(if i == 0 { "?" } else { ",?" });
                args.push((*status).to_owned().into());
            }
            sql.push(')');
        }
        if refuted_only {
            sql.push_str(" AND e.trust_level=?");
            args.push(Trust::L2b.as_str().to_owned().into());
        }
        if !self.fts.is_empty() && self.symbols.is_empty() {
            sql.push_str(" AND l.entry_id IS NOT NULL");
        }
        for &(role, symbol) in &self.symbols {
            sql.push_str(" AND EXISTS (SELECT 1 FROM entry_symbols WHERE entry_id=e.id AND role=? AND symbol=?)");
            args.extend([role.to_owned().into(), symbol.to_owned().into()]);
        }
        sql.push_str(
            ") SELECT id,title,engine,verified_at_commit,score,sum(score > ?) OVER () FROM matches",
        );
        args.push(HIGH_SCORE.into());
        if only_high {
            sql.push_str(" WHERE score > ?");
            args.push(HIGH_SCORE.into());
        }
        sql.push_str(" ORDER BY score DESC,rank IS NULL,rank,id LIMIT ?");
        args.push(i64::from(limit).into());
        let mut stmt = db.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args))?;
        while let Some(row) = rows.next()? {
            result.high_count = row.get(5)?;
            result.rows.push(WikiHitSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                engine: row.get::<_, String>(2)?.parse()?,
                verified_at_commit: row.get(3)?,
                score: row.get(4)?,
            });
        }
        Ok(result)
    }
}
