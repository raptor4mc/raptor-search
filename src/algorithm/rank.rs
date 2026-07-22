// algorithm/rank.rs
//
// Scoring. Before this file, ranking was purely:
//     ts_rank(search_vector, query) * log(2 + inbound_links)
// which has no notion of "the query IS the name of this site" — so a page
// that mentions a brand once in passing can outrank that brand's own
// homepage, as long as it has slightly denser keyword usage. That's the
// Discord-vs-Rust-meetup problem from the screenshots.
//
// We fix this with two multiplicative boosts computed in SQL:
//   - domain_boost: query text matches the registrable host almost exactly
//     (e.g. query "discord" against host "discord.com")
//   - title_boost:  query text appears in the title, weighted higher when
//     it's at/near the start of the title (closer to how a person reads it)
//
// Both are cheap CASE WHEN expressions so they run inside the same query
// instead of requiring a second round trip.

/// Full search query text. Bind order: $1 = raw query string (used for
/// ts_rank + boosts), $2 = limit, $3 = offset.
///
/// Requires the `pages` table to have: title, url, snippet, search_vector,
/// inbound_links (all already present) — no schema change needed for this
/// part specifically (see migrations/002_algorithm_upgrade.sql for the
/// separate body-text indexing fix).
pub const SEARCH_SQL: &str = r#"
SELECT title, url, snippet,
    (
        ts_rank(search_vector, websearch_to_tsquery('english', $1))
        * log(2 + inbound_links)
        * domain_match_boost(url, $1)
        * title_match_boost(title, $1)
        * homepage_boost(url)
    )::double precision AS score
FROM pages
WHERE search_vector @@ websearch_to_tsquery('english', $1)
ORDER BY score DESC
LIMIT $2 OFFSET $3
"#;

/// SQL function definitions. Run once via migration — kept here (rather
/// than buried in a .sql file with no comments) so the scoring logic and
/// its rationale live next to each other.
pub const SCORING_FUNCTIONS_SQL: &str = r#"
-- Boost when the query is essentially the site's own name, e.g. searching
-- "discord" should strongly favor discord.com over pages that merely
-- mention Discord. Compares query against the host with TLD stripped.
CREATE OR REPLACE FUNCTION domain_match_boost(page_url TEXT, query TEXT)
RETURNS DOUBLE PRECISION AS $$
DECLARE
    host TEXT;
    bare_host TEXT;
    q TEXT := lower(trim(query));
BEGIN
    host := lower(regexp_replace(page_url, '^https?://(www\.)?([^/]+).*$', '\2'));
    bare_host := regexp_replace(host, '\.[a-z]{2,}$', '');
    IF host = q OR bare_host = q THEN
        RETURN 4.0;
    ELSIF host LIKE ('%' || q || '%') THEN
        RETURN 1.8;
    ELSE
        RETURN 1.0;
    END IF;
END;
$$ LANGUAGE plpgsql IMMUTABLE;

-- Boost when the query text appears in the title, extra weight if it's the
-- leading words (how a person actually judges title relevance).
CREATE OR REPLACE FUNCTION title_match_boost(page_title TEXT, query TEXT)
RETURNS DOUBLE PRECISION AS $$
DECLARE
    t TEXT := lower(trim(page_title));
    q TEXT := lower(trim(query));
BEGIN
    IF t LIKE (q || '%') THEN
        RETURN 2.0;
    ELSIF t LIKE ('%' || q || '%') THEN
        RETURN 1.4;
    ELSE
        RETURN 1.0;
    END IF;
END;
$$ LANGUAGE plpgsql IMMUTABLE;

-- Mild boost for a site's homepage over a deep subpage, all else equal —
-- when someone searches a brand name they usually want the front door.
CREATE OR REPLACE FUNCTION homepage_boost(page_url TEXT)
RETURNS DOUBLE PRECISION AS $$
BEGIN
    IF page_url ~ '^https?://(www\.)?[^/]+/?$' THEN
        RETURN 1.3;
    ELSE
        RETURN 1.0;
    END IF;
END;
$$ LANGUAGE plpgsql IMMUTABLE;
"#;
