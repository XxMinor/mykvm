use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn requests() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static REQUESTS: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    REQUESTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn expect_paste(peer_key: &str) -> String {
    let token = crate::random_hex(16);
    let mut requests = requests().lock().unwrap_or_else(|e| e.into_inner());
    requests.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(1800));
    if requests.len() >= 64 {
        requests.clear();
    }
    requests.insert(token.clone(), (peer_key.into(), Instant::now()));
    token
}

pub(crate) fn paste_expected(token: &str, key: &str) -> bool {
    requests()
        .lock()
        .map(|requests| {
            requests.get(token).is_some_and(|(expected, time)| {
                expected == key && time.elapsed() < Duration::from_secs(1800)
            })
        })
        .unwrap_or(false)
}

pub(crate) fn consume_paste(token: &str, key: &str) -> bool {
    let Ok(mut requests) = requests().lock() else {
        return false;
    };
    if !requests.get(token).is_some_and(|(expected, time)| {
        expected == key && time.elapsed() < Duration::from_secs(1800)
    }) {
        return false;
    }
    requests.remove(token);
    true
}

fn receipts() -> &'static Mutex<HashMap<(String, String), (PathBuf, Instant)>> {
    static RECEIPTS: OnceLock<Mutex<HashMap<(String, String), (PathBuf, Instant)>>> =
        OnceLock::new();
    RECEIPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn remember_file(origin: &str, id: &str, path: PathBuf) {
    let mut receipts = receipts().lock().unwrap_or_else(|e| e.into_inner());
    receipts.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(1800));
    if receipts.len() >= 256 {
        receipts.clear();
    }
    receipts.insert((origin.into(), id.into()), (path, Instant::now()));
}

pub(crate) fn take_files(origin: &str, ids: &[String]) -> Option<Vec<String>> {
    if ids.is_empty() || ids.len() > 64 {
        return None;
    }
    if ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len() {
        return None;
    }
    let mut receipts = receipts().lock().ok()?;
    if ids.iter().any(|id| {
        !receipts
            .get(&(origin.into(), id.clone()))
            .is_some_and(|(_, time)| time.elapsed() < Duration::from_secs(1800))
    }) {
        return None;
    }
    Some(
        ids.iter()
            .filter_map(|id| {
                receipts
                    .remove(&(origin.into(), id.clone()))
                    .map(|(path, _)| path.to_string_lossy().into_owned())
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_paste_reply_is_bound_to_the_requested_peer_and_is_single_use() {
        let token = expect_paste("expected-key");
        assert!(!consume_paste(&token, "other-key"));
        assert!(consume_paste(&token, "expected-key"));
        assert!(!consume_paste(&token, "expected-key"));
    }
    #[test]
    fn file_clipboard_cannot_reference_another_peers_download() {
        remember_file("peer-a", "receipt-a", PathBuf::from("example.txt"));
        let ids = vec!["receipt-a".into()];
        assert!(take_files("peer-b", &ids).is_none());
        assert_eq!(take_files("peer-a", &ids).unwrap(), vec!["example.txt"]);
        assert!(take_files("peer-a", &ids).is_none());
    }
}
