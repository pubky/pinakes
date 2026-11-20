//! PKARR records through relays, fetched by `Egress` and verified here: a relay can withhold a
//! record, never forge one, because every packet carries its owner's signature; and Pinakes
//! never goes back to a packet older than the newest it remembers for that key.
use super::{
    egress::{Egress, FetchOptions},
    ErrorClass,
};
use anyhow::{Context, Result};
use pkarr::{dns::rdata::RData, PublicKey, SignedPacket, Timestamp};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use url::Url;

/// The relays pubky.app uses.
pub const DEFAULT_RELAYS: [&str; 2] = ["https://pkarr.pubky.app", "https://pkarr.pubky.org"];
/// What relays label their payloads with, beside the general allowlist.
pub const PAYLOAD_TYPES: &[&str] = &[
    "application/pkarr.org/relays#payload",
    "application/octet-stream",
    "",
];
/// A packet's lifetime in the cache: its smallest record TTL, within these bounds (seconds), as
/// pkarr's own client clamps it. "No record" is kept for the minimum.
const MIN_TTL: u32 = 300;
const MAX_TTL: u32 = 86_400;
const CACHE_ENTRIES: usize = 10_000;
/// A relay payload is at most 1,104 bytes (`SignedPacket::MAX_BYTES`); anything much larger is
/// refused before it is read.
const PAYLOAD_CAP: usize = 8 * 1024;
/// Delegations followed from the owner's record, as pkarr's client bounds them.
const MAX_DEPTH: usize = 7;
/// Endpoints read across one resolution.
const MAX_ENDPOINTS: usize = 64;

struct Cached {
    expires: Instant,
    packet: Option<SignedPacket>,
}

#[derive(Default)]
struct State {
    cache: HashMap<String, Cached>,
    /// The newest packet timestamp seen per key, kept apart from the cache (and bounded like it),
    /// so an expired or evicted packet still bars an older one.
    newest: HashMap<String, Timestamp>,
}
pub struct Relays {
    relays: Vec<Url>,
    /// How long one relay may take to answer (`limits.pkarr_relay_timeout_ms`): shorter than a
    /// resolution's own deadline, so a relay that stalls does not hold back another's packet.
    deadline: Duration,
    egress: Egress,
    state: Mutex<State>,
}
/// One relay's answer for a key.
enum Answer {
    Missing,
    Packet(SignedPacket),
    Failed(String),
    /// The relay could not be reached (`super::unreached`): no answer about the key.
    Unreached(String),
    /// A wait, in seconds: Egress's own for the relay host's budget (no request was sent), or
    /// the relay's 429 with its `Retry-After`.
    Wait(u64),
}

/// One HTTPS endpoint of a record: a domain, `.` (the key's own addresses) or another key.
struct Target {
    name: String,
    port: Option<u16>,
}

impl Relays {
    pub fn new(relays: &[String], deadline: Duration, egress: Egress) -> Result<Self> {
        Ok(Self {
            deadline,
            relays: relays
                .iter()
                .map(|r| Url::parse(r).with_context(|| format!("PKARR relay {r}")))
                .collect::<Result<_>>()?,
            egress,
            state: Mutex::default(),
        })
    }

    /// The owner's verified packet: from the cache while it lives, else the newest one the
    /// relays (asked together, each within `deadline`) serve. `None` when the relays have no
    /// record. Relay-side failures (timeouts, policy refusals, payloads that do not verify) are
    /// `connect`: retried with backoff, then dead-lettered like any failure. When no relay
    /// answered, because each one that failed could not be reached (refused, reset), waited for
    /// its host budget or answered 429, the failure is `super::Unreached` (or that wait alone,
    /// with the longest `Retry-After`), which never dead-letters.
    pub async fn packet(&self, key: &PublicKey) -> Result<Option<SignedPacket>> {
        let z32 = key.to_z32();
        if let Some(cached) = self.state.lock().unwrap().cache.get(&z32) {
            if cached.expires > Instant::now() {
                return Ok(cached.packet.clone());
            }
        }
        let options = FetchOptions {
            accept: Some("application/pkarr.org/relays#payload".into()),
            extra_types: PAYLOAD_TYPES,
            // Whatever the relay says the body is, HTML too.
            body_cap: Some(PAYLOAD_CAP),
            ..Default::default()
        };
        let answers = futures_util::future::join_all(self.relays.iter().map(|relay| {
            let url = format!("{}/{z32}", relay.as_str().trim_end_matches('/'));
            let options = &options;
            async move {
                let asked =
                    tokio::time::timeout(self.deadline, self.egress.fetch_with(&url, options));
                let Ok(answer) = asked.await else {
                    return Answer::Failed(format!(
                        "{relay} did not answer within {} ms",
                        self.deadline.as_millis()
                    ));
                };
                match answer {
                    Ok(fetched) if fetched.status == 404 => Answer::Missing,
                    Ok(fetched) if (200..300).contains(&fetched.status) => {
                        match SignedPacket::from_relay_payload(key, &fetched.body.into()) {
                            Ok(packet) => Answer::Packet(packet),
                            Err(error) => Answer::Failed(format!(
                                "{relay} served a packet that does not verify ({error})"
                            )),
                        }
                    }
                    Ok(fetched) => Answer::Failed(format!("{relay} answered {}", fetched.status)),
                    Err(error) if super::unreached(&error) => {
                        Answer::Unreached(format!("{relay}: {error:#}"))
                    }
                    Err(error) => match error.downcast_ref::<super::RetryLater>() {
                        Some(wait) if matches!(wait.class, "rate_limited" | "http_4xx") => {
                            Answer::Wait(wait.seconds)
                        }
                        _ => Answer::Failed(format!("{relay}: {error:#}")),
                    },
                }
            }
        }))
        .await;
        let (mut failures, mut unreached, mut wait) = (vec![], vec![], None::<u64>);
        let (mut missing, mut best): (bool, Option<SignedPacket>) = (false, None);
        for answer in answers {
            match answer {
                Answer::Missing => missing = true,
                Answer::Failed(error) => failures.push(error),
                Answer::Unreached(error) => unreached.push(error),
                Answer::Wait(seconds) => wait = Some(wait.unwrap_or(0).max(seconds)),
                Answer::Packet(packet) => {
                    if best
                        .as_ref()
                        .is_none_or(|b| packet.timestamp() > b.timestamp())
                    {
                        best = Some(packet);
                    }
                }
            }
        }
        let connect = |message: String| anyhow::Error::new(ErrorClass("connect")).context(message);
        if let Some(packet) = best {
            return self.keep(z32, packet).map(Some).ok_or_else(|| {
                connect("the PKARR relays serve only a packet older than one seen before".into())
            });
        }
        if missing && failures.is_empty() && unreached.is_empty() && wait.is_none() {
            self.store(z32, None, MIN_TTL);
            return Ok(None);
        }
        if failures.is_empty() && unreached.is_empty() {
            if let Some(seconds) = wait {
                // Egress waited for a relay host's budget, or a relay said when to come back:
                // ask again then.
                return Err(super::RetryLater {
                    class: "rate_limited",
                    seconds,
                }
                .into());
            }
            return Err(connect("no PKARR relay configured".into()));
        }
        if failures.is_empty() && !missing {
            // No relay answered about the key: each one could not be reached, or Egress waited.
            return Err(anyhow::Error::new(super::Unreached).context(format!(
                "PKARR relays could not be reached: {}",
                unreached.join("; ")
            )));
        }
        failures.extend(unreached);
        Err(connect(format!(
            "PKARR relays failed: {}",
            failures.join("; ")
        )))
    }

    /// Cache a verified packet for its TTL unless it is older than the newest seen for its key;
    /// then the newer packet, if the cache still holds it, stays in use.
    fn keep(&self, z32: String, packet: SignedPacket) -> Option<SignedPacket> {
        let newest = self.state.lock().unwrap().newest.get(&z32).copied();
        if newest.is_some_and(|t| packet.timestamp() < t) {
            let mut state = self.state.lock().unwrap();
            let cached = state.cache.get_mut(&z32)?;
            let held = cached
                .packet
                .clone()
                .filter(|p| Some(p.timestamp()) >= newest)?;
            cached.expires = Instant::now() + Duration::from_secs(u64::from(MIN_TTL));
            return Some(held);
        }
        {
            let mut state = self.state.lock().unwrap();
            if !state.newest.contains_key(&z32) && state.newest.len() >= CACHE_ENTRIES {
                // Forget the oldest timestamps of keys no longer cached first.
                let victim = state
                    .newest
                    .iter()
                    .min_by_key(|(k, t)| (state.cache.contains_key(*k), **t))
                    .map(|(k, _)| k.clone());
                if let Some(victim) = victim {
                    state.newest.remove(&victim);
                }
            }
            state.newest.insert(z32.clone(), packet.timestamp());
        }
        self.store(z32, Some(packet.clone()), packet.ttl(MIN_TTL, MAX_TTL));
        Some(packet)
    }

    fn store(&self, z32: String, packet: Option<SignedPacket>, ttl: u32) {
        let mut state = self.state.lock().unwrap();
        let cache = &mut state.cache;
        if cache.len() >= CACHE_ENTRIES && !cache.contains_key(&z32) {
            let now = Instant::now();
            cache.retain(|_, c| c.expires > now);
            if cache.len() >= CACHE_ENTRIES {
                if let Some(soonest) = cache
                    .iter()
                    .min_by_key(|(_, c)| c.expires)
                    .map(|(k, _)| k.clone())
                {
                    cache.remove(&soonest);
                }
            }
        }
        cache.insert(
            z32,
            Cached {
                expires: Instant::now() + Duration::from_secs(u64::from(ttl)),
                packet,
            },
        );
    }

    /// Test hook: every cached packet expires now (`purge`: is forgotten), the newest timestamps
    /// stay.
    #[cfg(test)]
    pub(crate) fn expire(&self, purge: bool) {
        let mut state = self.state.lock().unwrap();
        if purge {
            state.cache.clear();
        }
        for cached in state.cache.values_mut() {
            cached.expires = Instant::now();
        }
    }

    /// The homeserver's public HTTPS (ICANN) endpoint for the owner's `_pubky` record: its
    /// HTTPS records in priority order, a target that is a key followed to that key's own
    /// records, as pkarr resolves endpoints. Key-pinned (raw-public-key TLS) endpoints are never
    /// contacted.
    pub async fn homeserver(&self, owner: &PublicKey) -> Result<(String, Option<u16>)> {
        let packet = self.packet(owner).await?.ok_or_else(|| {
            anyhow::Error::new(ErrorClass("dns")).context("no PKARR record for the owner")
        })?;
        let mut stack = targets(&packet, "_pubky");
        let (mut depth, mut seen, mut pinned) = (1, 0, false);
        while let Some(target) = stack.pop() {
            seen += 1;
            if seen > MAX_ENDPOINTS {
                return Err(ErrorClass("policy").into());
            }
            if let Ok(key) = PublicKey::try_from(target.name.as_str()) {
                if depth >= MAX_DEPTH {
                    break;
                }
                depth += 1;
                if let Some(packet) = self.packet(&key).await? {
                    stack.extend(targets(&packet, &target.name));
                }
            } else if target.name == "." {
                pinned = true;
            } else {
                return Ok((target.name, target.port));
            }
        }
        if pinned {
            return Err(anyhow::Error::new(ErrorClass("connect")).context(
                "the homeserver publishes only key-pinned (raw-public-key TLS) endpoints; Pinakes reaches homeservers over their public HTTPS domain only",
            ));
        }
        Err(anyhow::Error::new(ErrorClass("dns")).context("no homeserver endpoint for the owner"))
    }
}

/// The HTTPS records at `name`, ordered so that `pop` yields the lowest priority first.
fn targets(packet: &SignedPacket, name: &str) -> Vec<Target> {
    let mut records: Vec<_> = packet
        .resource_records(name)
        .filter_map(|record| match &record.rdata {
            RData::HTTPS(https) => Some(&https.0),
            _ => None,
        })
        .collect();
    records.sort_by_key(|svcb| std::cmp::Reverse(svcb.priority));
    records
        .into_iter()
        .filter_map(|svcb| {
            let name = svcb.target.to_string();
            let name = if name.is_empty() { ".".into() } else { name };
            // AliasMode (priority 0, RFC 9460): the target name only, no parameters; an alias to
            // `.` says the service is not available.
            if svcb.priority == 0 {
                return (name != ".").then_some(Target { name, port: None });
            }
            Some(Target {
                name,
                port: svcb.iter_params().find_map(|p| match p {
                    pkarr::dns::rdata::SVCParam::Port(port) => Some(*port).filter(|p| *p > 0),
                    _ => None,
                }),
            })
        })
        .collect()
}
