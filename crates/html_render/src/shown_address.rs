use url::Url;

/// What the address bar reads for a page that is at `current`.
///
/// A document the editor shows is written to a temporary file for the engine to
/// load, and the name of that file means nothing to the reader, who knows the
/// document by where it lives. While the page is at one of `own`, the bar reads
/// `shown_as` followed by whatever came after the file's name -- a query, a
/// fragment -- and a document with no path of its own reads as nothing. A page
/// anywhere else is shown as it is.
pub(crate) fn shown_address(current: &Url, own: &[Url], shown_as: Option<&Url>) -> Option<Url> {
    if !own.iter().any(|own| same_file(current, own)) {
        return Some(current.clone());
    }
    let mut shown = shown_as?.clone();
    shown.set_query(current.query());
    shown.set_fragment(current.fragment());
    Some(shown)
}

/// Whether two addresses are the same file, whatever follows its name.
fn same_file(first: &Url, second: &Url) -> bool {
    let without_the_tail = |address: &Url| {
        let mut address = address.clone();
        address.set_query(None);
        address.set_fragment(None);
        address
    };
    without_the_tail(first) == without_the_tail(second)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).expect("a url in a test")
    }

    const OWN: &str = "file:///tmp/.tmpXgBsCm/page.html";
    const SHOWN: &str = "file:///home/reader/docs/backlog.html";

    #[test]
    fn the_temporary_file_reads_as_the_documents_own_path() {
        let shown = shown_address(&url(OWN), &[url(OWN)], Some(&url(SHOWN)));
        assert_eq!(shown, Some(url(SHOWN)));
    }

    #[test]
    fn what_follows_the_file_name_is_kept() {
        let shown = shown_address(
            &url("file:///tmp/.tmpXgBsCm/page.html?tab=2#backlog"),
            &[url(OWN)],
            Some(&url(SHOWN)),
        );
        assert_eq!(
            shown,
            Some(url("file:///home/reader/docs/backlog.html?tab=2#backlog"))
        );
    }

    #[test]
    fn a_document_with_no_path_of_its_own_reads_as_nothing() {
        assert_eq!(shown_address(&url(OWN), &[url(OWN)], None), None);
    }

    #[test]
    fn the_document_before_the_new_one_has_arrived_reads_the_same() {
        let before = url("file:///tmp/.tmpAAAAAA/page.html");
        let shown = shown_address(&before, &[url(OWN), before.clone()], Some(&url(SHOWN)));
        assert_eq!(shown, Some(url(SHOWN)));
    }

    #[test]
    fn any_other_page_is_shown_as_it_is() {
        let elsewhere = url("https://example.com/a?b=1#c");
        assert_eq!(
            shown_address(&elsewhere, &[url(OWN)], Some(&url(SHOWN))),
            Some(elsewhere.clone())
        );
        assert_eq!(shown_address(&elsewhere, &[], None), Some(elsewhere));
    }

    #[test]
    fn the_same_path_on_another_port_is_another_page() {
        let own = url("http://localhost:1/page.html");
        let other = url("http://localhost:2/page.html#top");
        assert_eq!(
            shown_address(&other, &[own], Some(&url(SHOWN))),
            Some(other)
        );
    }

    #[test]
    fn a_file_with_the_same_name_in_another_directory_is_another_page() {
        let other = url("file:///home/reader/page.html");
        assert_eq!(
            shown_address(&other, &[url(OWN)], Some(&url(SHOWN))),
            Some(other)
        );
    }
}
