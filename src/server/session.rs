use std::borrow::Borrow;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use crate::client;
use crate::media;

static SESSIONS: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Session ID.
///
/// This is also passed through the extra data in various places.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub struct Id(Arc<IdInner>);

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct IdInner(String);

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <String as fmt::Display>::fmt(&self.0 .0, f)
    }
}

impl Drop for IdInner {
    fn drop(&mut self) {
        let mut sessions = SESSIONS.lock().unwrap();
        sessions.remove(&self.0);
    }
}

impl Id {
    pub fn as_str(&self) -> &str {
        self.0 .0.as_str()
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for Id {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl<'a> From<&'a str> for Id {
    fn from(s: &'a str) -> Self {
        String::from(s).into()
    }
}

impl From<String> for Id {
    fn from(s: String) -> Self {
        let mut sessions = SESSIONS.lock().unwrap();
        sessions.insert(s.clone());
        Id(Arc::new(IdInner(s)))
    }
}

impl Id {
    /// Create a new random session id.
    pub fn new() -> Self {
        use rand::seq::SliceRandom;

        static SESSION_ID_CHARS: &[u8] = &[
            b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', b'j', b'k', b'l', b'm', b'n',
            b'o', b'p', b'q', b'r', b's', b't', b'u', b'v', b'w', b'x', b'y', b'z', b'A', b'B',
            b'C', b'D', b'E', b'F', b'G', b'H', b'I', b'J', b'K', b'L', b'M', b'N', b'O', b'P',
            b'Q', b'R', b'S', b'T', b'U', b'V', b'W', b'X', b'Y', b'Z', b'0', b'1', b'2', b'3',
            b'4', b'5', b'6', b'7', b'8', b'9', b'-', b'_', b'.', b'+',
        ];

        // Try hard to not create a session id that is already in use
        let mut sessions = SESSIONS.lock().unwrap();
        let mut rng = rand::thread_rng();
        let id = loop {
            let id = String::from_utf8(
                SESSION_ID_CHARS
                    .choose_multiple(&mut rng, 16)
                    .copied()
                    .collect::<Vec<_>>(),
            )
            .unwrap();

            if !sessions.contains(&id) {
                sessions.insert(id.clone());
                break id;
            }
        };

        Id(Arc::new(IdInner(id)))
    }
}

/// Implements session handling and state machine
// State machine at https://tools.ietf.org/html/rfc7826#appendix-B.1
pub struct Session {
    /// Media for this session.
    ///
    /// Also stores the session id.
    pub(super) media: media::Controller<media::controller::Server>,

    /// Client for this session if there is one currently.
    pub(super) client_id: Option<client::Id>,

    /// Presentation URI for this session.
    pub(super) presentation_uri: super::PresentationURI,

    /// Last time this session was active.
    pub(super) last_active: std::time::Instant,
}
