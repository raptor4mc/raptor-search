-- 002_algorithm_upgrade.sql
--
-- Run this against your Postgres database (fly proxy / psql / whatever you
-- use to reach it). It's written idempotently (IF NOT EXISTS / OR REPLACE)
-- so it's safe to run more than once.
--
-- WHY: previously `pages` only stored `title` + `snippet` (a 200-char,
-- already-stemmed, stopword-stripped string), and `search_vector` could
-- only ever be built from those two columns. That means full-text search
-- was matching against a truncated, mangled fragment of each page instead
-- of the actual page content — a query term that didn't happen to land in
-- the first ~200 filtered characters simply wouldn't match at all, no
-- matter how relevant the page actually was.

-- 1. New columns: real indexable body text + the clean meta description.
ALTER TABLE pages ADD COLUMN IF NOT EXISTS body_text TEXT NOT NULL DEFAULT '';
ALTER TABLE pages ADD COLUMN IF NOT EXISTS meta_description TEXT;

-- 2. Rebuild search_vector to actually use body_text, with title weighted
-- higher than body (Postgres full text search weight tiers: A > B > C > D).
-- IMPORTANT: check what your current search_vector definition looks like
-- first (`\d pages` in psql) — if it's a GENERATED ALWAYS AS STORED column
-- you'll need to drop and recreate it, since Postgres won't let you ALTER
-- a generated column's expression in place:
--
--   ALTER TABLE pages DROP COLUMN search_vector;
--   ALTER TABLE pages ADD COLUMN search_vector tsvector
--     GENERATED ALWAYS AS (
--       setweight(to_tsvector('english', coalesce(title, '')), 'A') ||
--       setweight(to_tsvector('english', coalesce(meta_description, '')), 'B') ||
--       setweight(to_tsvector('english', coalesce(body_text, '')), 'C')
--     ) STORED;
--   CREATE INDEX IF NOT EXISTS pages_search_vector_idx ON pages USING GIN (search_vector);
--
-- If it's instead maintained by a trigger, update the trigger function the
-- same way (same setweight expression, targeting NEW.search_vector).

-- 3. Ranking boost functions used by algorithm::rank::SEARCH_SQL.
-- (Definitions kept in src/algorithm/rank.rs as the source of truth and
-- copied here for convenience — if you change the scoring logic, update
-- both places.)

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

-- 4. inbound_links should already exist (referenced by the old query) but
-- guard it anyway in case this is being run on a fresh schema.
ALTER TABLE pages ADD COLUMN IF NOT EXISTS inbound_links BIGINT NOT NULL DEFAULT 0;
