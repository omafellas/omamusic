use anyhow::Result;
use parking_lot::Mutex;
use std::time::{Duration, Instant};

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub trait Http: Send + Sync + 'static {
    fn get(&self, url: &str) -> Result<Response>;
}

pub const USER_AGENT: &str = concat!(
    "omamusic/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/omafellas/omamusic)"
);

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Throttle {
    min_gap: Duration,
    last: Mutex<Option<Instant>>,
}

impl Throttle {
    pub fn new(min_gap: Duration) -> Self {
        Self {
            min_gap,
            last: Mutex::new(None),
        }
    }

    pub fn wait(&self) {
        let mut last = self.last.lock();
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.min_gap {
                std::thread::sleep(self.min_gap - elapsed);
            }
        }
        *last = Some(Instant::now());
    }
}

pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.new_agent(),
        }
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl Http for UreqHttp {
    fn get(&self, url: &str) -> Result<Response> {
        let mut response = self.agent.get(url).call()?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_vec()?;
        Ok(Response { status, body })
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct FakeHttp {
    routes: Vec<(String, u16, Vec<u8>)>,
    requests: Mutex<Vec<String>>,
    delay: Duration,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on(mut self, prefix: &str, status: u16, body: impl Into<Vec<u8>>) -> Self {
        self.routes.push((prefix.to_string(), status, body.into()));
        self
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().clone()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Http for FakeHttp {
    fn get(&self, url: &str) -> Result<Response> {
        self.requests.lock().push(url.to_string());
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let hit = self
            .routes
            .iter()
            .filter(|(prefix, _, _)| url.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _, _)| prefix.len());
        Ok(match hit {
            Some((_, status, body)) => Response {
                status: *status,
                body: body.clone(),
            },
            None => Response {
                status: 404,
                body: Vec::new(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_http_matches_longest_prefix_and_records_requests() {
        let http = FakeHttp::new()
            .on("https://a.example/", 200, b"root".to_vec())
            .on("https://a.example/deep", 404, b"deep".to_vec());
        let r = http.get("https://a.example/deep/er").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (404, b"deep".as_slice()));
        let r = http.get("https://a.example/other").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"root".as_slice()));
        let r = http.get("https://nobody.example/").unwrap();
        assert_eq!(r.status, 404);
        assert!(r.body.is_empty());
        assert_eq!(
            http.requests(),
            [
                "https://a.example/deep/er",
                "https://a.example/other",
                "https://nobody.example/"
            ]
        );
    }

    #[test]
    fn throttle_spaces_calls() {
        let throttle = Throttle::new(Duration::from_millis(50));
        let start = Instant::now();
        throttle.wait();
        throttle.wait();
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn user_agent_names_version_and_contact() {
        assert!(USER_AGENT.contains(env!("CARGO_PKG_VERSION")));
        assert!(USER_AGENT.contains("github.com/omafellas/omamusic"));
        assert!(USER_AGENT.starts_with("omamusic/"));
    }
}
