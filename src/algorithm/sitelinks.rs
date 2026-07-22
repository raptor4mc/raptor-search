// algorithm/sitelinks.rs
//
// Takes the flat, already-ranked list of rows from the DB and folds
// subsequent same-domain hits into "sitelinks" nested under the first
// (highest-scoring) hit for that domain — the Proton screenshot: Logga in,
// Proton Mail: Sign-in, Proton Mail, etc. all nested under the main
// proton.me result instead of listed as separate flat rows.

use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct ResultItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Serialize, Debug)]
pub struct GroupedResult {
    #[serde(flatten)]
    pub item: ResultItem,
    pub sitelinks: Vec<ResultItem>,
}

fn registrable_domain(url: &str) -> String {
    let without_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = without_scheme.split('/').next().unwrap_or(without_scheme);
    host.trim_start_matches("www.").to_lowercase()
}

/// `rows` must already be sorted by score DESC. `max_results` caps how many
/// top-level groups are returned; `max_sitelinks` caps sitelinks per group.
pub fn group_by_domain(
    rows: Vec<ResultItem>,
    max_results: usize,
    max_sitelinks: usize,
) -> Vec<GroupedResult> {
    let mut consumed = vec![false; rows.len()];
    let mut out: Vec<GroupedResult> = Vec::new();

    for i in 0..rows.len() {
        if consumed[i] || out.len() >= max_results {
            continue;
        }
        consumed[i] = true;
        let domain = registrable_domain(&rows[i].url);

        let mut sitelinks = Vec::new();
        for j in (i + 1)..rows.len() {
            if consumed[j] || sitelinks.len() >= max_sitelinks {
                continue;
            }
            if registrable_domain(&rows[j].url) == domain {
                sitelinks.push(rows[j].clone());
                consumed[j] = true;
            }
        }

        out.push(GroupedResult {
            item: rows[i].clone(),
            sitelinks,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(url: &str) -> ResultItem {
        ResultItem { title: url.to_string(), url: url.to_string(), snippet: String::new() }
    }

    #[test]
    fn groups_same_domain_as_sitelinks() {
        let rows = vec![
            item("https://proton.me"),
            item("https://proton.me/mail/login"),
            item("https://discord.com"),
            item("https://proton.me/pricing"),
        ];
        let grouped = group_by_domain(rows, 10, 5);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].item.url, "https://proton.me");
        assert_eq!(grouped[0].sitelinks.len(), 2);
        assert_eq!(grouped[1].item.url, "https://discord.com");
    }

    #[test]
    fn respects_max_sitelinks() {
        let rows = vec![
            item("https://proton.me"),
            item("https://proton.me/a"),
            item("https://proton.me/b"),
            item("https://proton.me/c"),
        ];
        let grouped = group_by_domain(rows, 10, 2);
        assert_eq!(grouped[0].sitelinks.len(), 2);
    }
}
