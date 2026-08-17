//! Fixtures shared by the app modules' tests.

use crate::config::{Config, Profile};
use crate::lakefs::{Client, ObjectStats};

use super::App;

pub(crate) fn test_app() -> App {
    let profile = Profile {
        // Nothing here is fetched; the client only has to exist.
        endpoint: "http://127.0.0.1:1".into(),
        access_key_id: "key".into(),
        secret_access_key: "secret".into(),
        default_repo: None,
        default_ref: None,
        verify_tls: true,
        timeout_secs: 1,
        description: None,
    };
    let client = Client::new(&profile, 500).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    App::new(Config::default(), "test".into(), profile, client, tx)
}

pub(crate) fn stat(path: &str, dir: bool) -> ObjectStats {
    ObjectStats {
        path: path.into(),
        path_type: if dir { "common_prefix" } else { "object" }.into(),
        size_bytes: (!dir).then_some(12),
    }
}
