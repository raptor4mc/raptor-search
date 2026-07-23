// algorithm/mod.rs
//
// Entry point for the ranking/extraction algorithm. Kept as separate files
// per concern so each piece can be reasoned about (and tested) on its own:
//
//   extract.rs   - pulling display-safe visible text, meta description,
//                  main-content detection, boilerplate dedup out of HTML
//   tokenize.rs   - stopword removal + stemming, for indexing ONLY
//   rank.rs       - scoring: ts_rank + inbound links + domain/title/
//                  homepage boosts
//   sitelinks.rs  - grouping same-domain results together for display

pub mod extract;
pub mod metadata;
pub mod rank;
pub mod sitelinks;
pub mod tokenize;
