//! Bitcoin blocks, transactions and addresses, and Lightning nodes and channels, read from the
//! mempool.space API by the id in the link.
//!
//! Every link of one object, on mempool.space (with or without a language prefix) or on another
//! block explorer of [`EXPLORERS`], is an alias of one identifier: `btc:block/{hash}`,
//! `btc:tx/{txid}`, `btc:address/{address}`, `ln:node/{key}` and `ln:channel/{decimal id}`. Test
//! networks are separate objects with their own prefixes (`btc-testnet4:tx/…`,
//! `ln-signet:node/…`). A block link by height is an alias of the block's hash once the block is
//! buried six deep in the best chain (`settled`). The card is always read from mempool.space
//! (Esplora as the fallback), whichever explorer the link is on.
//!
//! No schema.org type fits these objects, so the cards use local types.
use super::{
    kit::{
        body, decimal, fetch, float, host_and_segments, int, json, plain, text, time, unix, Card,
        Request,
    },
    Budget, Example, Source,
};
use crate::{
    model::{Related, ResourceMeta},
    resolver::egress::Fetched,
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SITE: &str = "mempool.space";
const VIA: &str = "mempool";
/// How deep a block must be before its height names it for good.
pub const SETTLED_DEPTH: u64 = 6;
/// How many entries of one `rel` a card's `related` list keeps: a transaction's inputs and
/// outputs, an address's transactions, a node's channels and peers.
pub const RELATED_EACH: usize = 10;
/// When a card with a part not there yet (a block's next block, a transaction's block) or not
/// read (its request was put off; a block's pool, fees and reward, which Esplora's record lacks)
/// is read again, at the latest.
const READ_AGAIN_SECS: i64 = 3600;
/// How long the requests for a card's `related` parts may take, from the start of its read: the
/// rest of the official route's limit (`resolver::cards`, 14 s) stays for the card's own record
/// and the Esplora fallback.
const PARTS_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// A Bitcoin network. Each one is a separate set of objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
    Testnet4,
    Signet,
}
impl Network {
    /// The path prefix on mempool.space (and in its API).
    pub fn path(self) -> &'static str {
        match self {
            Self::Mainnet => "",
            Self::Testnet => "/testnet",
            Self::Testnet4 => "/testnet4",
            Self::Signet => "/signet",
        }
    }
    /// The namespace of on-chain identifiers.
    fn chain(self) -> &'static str {
        match self {
            Self::Mainnet => "btc",
            Self::Testnet => "btc-testnet",
            Self::Testnet4 => "btc-testnet4",
            Self::Signet => "btc-signet",
        }
    }
    /// The namespace of Lightning identifiers.
    fn lightning(self) -> &'static str {
        match self {
            Self::Mainnet => "ln",
            Self::Testnet => "ln-testnet",
            Self::Testnet4 => "ln-testnet4",
            Self::Signet => "ln-signet",
        }
    }
}

/// A block, by hash or by height.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockRef {
    Hash(String),
    Height(u64),
}

/// The object a link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    Block(BlockRef),
    Tx(String),
    Address(String),
    Node(String),
    /// The decimal short channel id.
    Channel(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub network: Network,
    pub object: Object,
}

/// 64 hex digits in lower case: a block hash or a transaction id.
pub fn hash(value: &str) -> Option<String> {
    (value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

/// A Lightning node's public key: 33 bytes in hex, compressed (`02`/`03`), in lower case.
pub fn node_key(value: &str) -> Option<String> {
    let value = value.to_ascii_lowercase();
    (value.len() == 66
        && (value.starts_with("02") || value.starts_with("03"))
        && value.bytes().all(|b| b.is_ascii_hexdigit()))
    .then_some(value)
}

/// A channel id: the decimal form, or the short form `{block}x{tx}x{output}`.
pub fn channel_id(value: &str) -> Option<u64> {
    if plain(value, 20, |c| c.is_ascii_digit()) {
        return value.parse().ok().filter(|id| *id > 0);
    }
    let parts: Vec<&str> = value.split('x').collect();
    let [block, tx, output] = parts.as_slice() else {
        return None;
    };
    let number = |s: &str, max: u64| decimal::<u64>(s, 8).filter(|n| *n <= max);
    let (block, tx, output) = (
        number(block, 0xFF_FFFF)?,
        number(tx, 0xFF_FFFF)?,
        number(output, 0xFFFF)?,
    );
    Some(block << 40 | tx << 16 | output).filter(|id| *id > 0)
}

/// The short form of a decimal channel id.
pub fn short_channel_id(id: u64) -> String {
    format!("{}x{}x{}", id >> 40, (id >> 16) & 0xFF_FFFF, id & 0xFFFF)
}

const BECH32: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn bech32_polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut chk = 1u32;
    for v in values {
        let top = chk >> 25;
        chk = (chk & 0x1ff_ffff) << 5 ^ u32::from(*v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

/// A segwit address (bech32 or bech32m) with a valid checksum, in lower case.
fn segwit_address(value: &str, hrp: &str) -> Option<String> {
    if value.bytes().any(|b| b.is_ascii_lowercase())
        && value.bytes().any(|b| b.is_ascii_uppercase())
    {
        return None;
    }
    let value = value.to_ascii_lowercase();
    let data = value.strip_prefix(hrp)?.strip_prefix('1')?;
    if !(8..=87).contains(&data.len()) {
        return None;
    }
    let data: Vec<u8> = data
        .chars()
        .map(|c| BECH32.find(c).map(|i| i as u8))
        .collect::<Option<_>>()?;
    let mut values: Vec<u8> = hrp.bytes().map(|b| b >> 5).collect();
    values.push(0);
    values.extend(hrp.bytes().map(|b| b & 31));
    values.extend(&data);
    let version = data[0];
    let expected = match version {
        0 => 1,
        1..=16 => 0x2bc8_30a3,
        _ => return None,
    };
    // The witness program: the 5-bit groups between the version and the 6-group checksum, as
    // bytes, with at most 4 zero bits of padding. Version 0 has 20 or 32 bytes; the others 2 to 40.
    let groups = &data[1..data.len() - 6];
    let bits = groups.len() * 5;
    let padding = bits % 8;
    let last = u32::from(*groups.last()?);
    let program = bits / 8;
    let length_ok = match version {
        0 => matches!(program, 20 | 32),
        _ => (2..=40).contains(&program),
    };
    (bech32_polymod(&values) == expected
        && length_ok
        && padding <= 4
        && last & ((1 << padding) - 1) == 0)
        .then_some(value)
}

const BASE58: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// A legacy address with a valid Base58Check checksum and one of the given version bytes.
fn base58_address(value: &str, versions: &[u8]) -> Option<String> {
    if !(25..=35).contains(&value.len()) {
        return None;
    }
    let mut bytes: Vec<u8> = vec![];
    for c in value.chars() {
        let mut carry = BASE58.find(c)? as u32;
        for b in bytes.iter_mut().rev() {
            carry += u32::from(*b) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let zeros = value.chars().take_while(|c| *c == '1').count();
    let mut decoded = vec![0u8; zeros];
    decoded.extend(bytes);
    if decoded.len() != 25 || !versions.contains(&decoded[0]) {
        return None;
    }
    let check = Sha256::digest(Sha256::digest(&decoded[..21]));
    (check[..4] == decoded[21..]).then(|| value.to_owned())
}

/// A valid address of the network, normalized (bech32 in lower case).
pub fn address(value: &str, network: Network) -> Option<String> {
    let (hrp, versions): (&str, &[u8]) = match network {
        Network::Mainnet => ("bc", &[0x00, 0x05]),
        _ => ("tb", &[0x6f, 0xc4]),
    };
    segwit_address(value, hrp).or_else(|| base58_address(value, versions))
}

/// What a path of an explorer names: its `{id}` read as one kind of object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A block by hash or by height.
    Block,
    /// A block by hash only.
    Hash,
    /// A block by height only.
    Height,
    Tx,
    Address,
    Node,
    Channel,
}

/// A block height in a link: up to 8 digits.
fn height(id: &str) -> Option<u64> {
    decimal(id, 8)
}

fn object(kind: Kind, id: &str, network: Network) -> Option<Object> {
    Some(match kind {
        Kind::Block => Object::Block(match hash(id) {
            Some(h) => BlockRef::Hash(h),
            None => BlockRef::Height(height(id)?),
        }),
        Kind::Hash => Object::Block(BlockRef::Hash(hash(id)?)),
        Kind::Height => Object::Block(BlockRef::Height(height(id)?)),
        Kind::Tx => Object::Tx(hash(id)?),
        Kind::Address => Object::Address(address(id, network)?),
        Kind::Node => Object::Node(node_key(id)?),
        Kind::Channel => Object::Channel(channel_id(id)?),
    })
}

/// A language prefix of a mempool site, e.g. `de` or `zh`.
fn language(segment: &str) -> bool {
    segment.len() == 2 && segment.bytes().all(|b| b.is_ascii_lowercase()) && segment != "tx"
}

/// A block explorer: where its pages of each object are. This one table drives both directions:
/// [`from_uri`] reads every path of every row as the object it names, and a card's Also on
/// ([`links`]) takes each listed row's page of the object.
pub struct Explorer {
    /// The site's name as a card shows it.
    pub site: &'static str,
    /// Its hosts in lower case; the first writes its links. A leading `www.` is ignored when
    /// reading, as everywhere.
    hosts: &'static [&'static str],
    /// Whether a card lists it in Also on. Sites that refuse a plain client (bot walls), are
    /// down or copy another listed site (a mempool instance) are only read.
    listed: bool,
    /// Whether a two-letter language may lead its paths (`/de/tx/…`), as on mempool sites.
    languages: bool,
    /// The networks it shows and each one's path prefix, `{net}` in its paths (empty for none);
    /// a network's first prefix writes its links.
    networks: &'static [(Network, &'static str)],
    /// Its paths, `{net}` the network's prefix and `{id}` the object's id; the first path of a
    /// kind writes its links.
    paths: &'static [(Kind, &'static str)],
}

/// Pages and API URLs of a mempool site.
const MEMPOOL_PATHS: &[(Kind, &str)] = &[
    (Kind::Block, "{net}/block/{id}"),
    (Kind::Tx, "{net}/tx/{id}"),
    (Kind::Address, "{net}/address/{id}"),
    (Kind::Node, "{net}/lightning/node/{id}"),
    (Kind::Channel, "{net}/lightning/channel/{id}"),
    (Kind::Hash, "{net}/api/v1/block/{id}"),
    (Kind::Hash, "{net}/api/block/{id}"),
    (Kind::Height, "{net}/api/block-height/{id}"),
    (Kind::Tx, "{net}/api/tx/{id}"),
    (Kind::Address, "{net}/api/address/{id}"),
    (Kind::Node, "{net}/api/v1/lightning/nodes/{id}"),
    (Kind::Channel, "{net}/api/v1/lightning/channels/{id}"),
];
/// The on-chain pages of a mempool site.
const MEMPOOL_PAGES: &[(Kind, &str)] = &[
    (Kind::Block, "{net}/block/{id}"),
    (Kind::Tx, "{net}/tx/{id}"),
    (Kind::Address, "{net}/address/{id}"),
];
/// The pages of an Esplora site (Blockstream's explorer and its copies).
const ESPLORA_PAGES: &[(Kind, &str)] = &[
    (Kind::Hash, "{net}/block/{id}"),
    (Kind::Height, "{net}/block-height/{id}"),
    (Kind::Tx, "{net}/tx/{id}"),
    (Kind::Address, "{net}/address/{id}"),
];
/// Blockchain.com's explorer pages, then its older paths, which redirect to them.
const BLOCKCHAIN_COM_PAGES: &[(Kind, &str)] = &[
    (Kind::Block, "explorer/blocks/{net}/{id}"),
    (Kind::Tx, "explorer/transactions/{net}/{id}"),
    (Kind::Address, "explorer/addresses/{net}/{id}"),
    (Kind::Block, "{net}/block/{id}"),
    (Kind::Tx, "{net}/tx/{id}"),
    (Kind::Address, "{net}/address/{id}"),
];
/// `/block/{hash or height}`, `/transaction/{txid}` and `/address/{address}` under a prefix.
const TRANSACTION_PAGES: &[(Kind, &str)] = &[
    (Kind::Block, "{net}/block/{id}"),
    (Kind::Tx, "{net}/transaction/{id}"),
    (Kind::Address, "{net}/address/{id}"),
];

const MAINNET: &[(Network, &str)] = &[(Network::Mainnet, "")];
const MEMPOOL_NETWORKS: &[(Network, &str)] = &[
    (Network::Mainnet, ""),
    (Network::Testnet, "testnet"),
    (Network::Testnet4, "testnet4"),
    (Network::Signet, "signet"),
];

/// mempool.space, the site the cards are read from: their `page_url`.
const MEMPOOL_SPACE: Explorer = Explorer {
    site: SITE,
    hosts: &["mempool.space"],
    listed: false,
    languages: true,
    networks: MEMPOOL_NETWORKS,
    paths: MEMPOOL_PATHS,
};

/// The explorers whose links name the objects, mempool.space first. docs/sources/mempool.md
/// says how each form was checked: on the live site where it shows the object, as a route
/// where its pages render client-side, on archived captures for the sites that refuse a plain
/// client. A network whose pages show nothing that names it (Blockchain.com's `btc-testnet`)
/// is left out. Other chains' paths on the same sites (`/liquid/`, `/bch/`, `/bitcoin-cash/`,
/// `/ltc/`, …) match no row.
pub const EXPLORERS: &[Explorer] = &[
    MEMPOOL_SPACE,
    Explorer {
        site: "Blockstream Explorer",
        hosts: &["blockstream.info"],
        listed: true,
        languages: false,
        networks: MEMPOOL_NETWORKS,
        paths: ESPLORA_PAGES,
    },
    Explorer {
        site: "mempool.emzy.de",
        hosts: &["mempool.emzy.de"],
        listed: false,
        languages: true,
        // Its testnet answers 502.
        networks: &[
            (Network::Mainnet, ""),
            (Network::Testnet4, "testnet4"),
            (Network::Signet, "signet"),
        ],
        paths: MEMPOOL_PAGES,
    },
    Explorer {
        site: "BTCScan",
        hosts: &["btcscan.org"],
        listed: true,
        languages: false,
        networks: MAINNET,
        paths: ESPLORA_PAGES,
    },
    Explorer {
        site: "Blockchain.com",
        hosts: &["www.blockchain.com"],
        listed: true,
        languages: false,
        networks: &[(Network::Mainnet, "btc")],
        paths: BLOCKCHAIN_COM_PAGES,
    },
    // Blockchain.com's older host: each path redirects to its explorer page.
    Explorer {
        site: "Blockchain.com",
        hosts: &["blockchain.info"],
        listed: false,
        languages: false,
        networks: MAINNET,
        paths: &[
            (Kind::Block, "block/{id}"),
            (Kind::Height, "block-height/{id}"),
            (Kind::Tx, "tx/{id}"),
            (Kind::Address, "address/{id}"),
        ],
    },
    Explorer {
        site: "OKLink",
        hosts: &["www.oklink.com"],
        listed: true,
        languages: false,
        // `/btc/…` redirects to `/bitcoin/…`.
        networks: &[(Network::Mainnet, "bitcoin"), (Network::Mainnet, "btc")],
        paths: &[
            (Kind::Block, "{net}/block/{id}"),
            (Kind::Tx, "{net}/tx/{id}"),
            (Kind::Address, "{net}/address/{id}"),
        ],
    },
    Explorer {
        site: "Blockchair",
        hosts: &["blockchair.com"],
        listed: false,
        languages: false,
        networks: &[(Network::Mainnet, "bitcoin")],
        paths: TRANSACTION_PAGES,
    },
    Explorer {
        site: "Blockchair",
        hosts: &["blockchair.com"],
        listed: false,
        languages: false,
        networks: &[(Network::Testnet, "bitcoin/testnet")],
        paths: &[
            (Kind::Block, "{net}/block/{id}"),
            (Kind::Tx, "{net}/transaction/{id}"),
        ],
    },
    Explorer {
        site: "BlockCypher",
        hosts: &["live.blockcypher.com"],
        listed: false,
        languages: false,
        networks: &[(Network::Mainnet, "btc")],
        paths: &[
            (Kind::Hash, "{net}/block/{id}"),
            (Kind::Tx, "{net}/tx/{id}"),
            (Kind::Address, "{net}/address/{id}"),
        ],
    },
    Explorer {
        site: "BlockCypher",
        hosts: &["live.blockcypher.com"],
        listed: false,
        languages: false,
        networks: &[(Network::Testnet, "btc-testnet")],
        paths: &[
            (Kind::Hash, "{net}/block/{id}"),
            (Kind::Tx, "{net}/tx/{id}"),
        ],
    },
    Explorer {
        site: "Bitcoin Explorer",
        hosts: &["bitcoinexplorer.org"],
        listed: false,
        languages: false,
        networks: MAINNET,
        paths: ESPLORA_PAGES,
    },
    Explorer {
        site: "3xpl",
        hosts: &["3xpl.com"],
        listed: false,
        languages: false,
        networks: &[(Network::Mainnet, "bitcoin")],
        paths: TRANSACTION_PAGES,
    },
    Explorer {
        site: "1ML",
        hosts: &["1ml.com"],
        listed: true,
        languages: false,
        networks: MAINNET,
        paths: &[(Kind::Node, "node/{id}"), (Kind::Channel, "channel/{id}")],
    },
    Explorer {
        site: "Amboss",
        hosts: &["amboss.space"],
        listed: true,
        languages: false,
        networks: MAINNET,
        paths: &[(Kind::Node, "node/{id}"), (Kind::Channel, "edge/{id}")],
    },
    Explorer {
        site: "Lightning Network+",
        hosts: &["lightningnetwork.plus"],
        listed: true,
        languages: false,
        networks: MAINNET,
        paths: &[(Kind::Node, "nodes/{id}")],
    },
];

/// A path template's segments on a network prefix: `{net}` stands for the prefix's segments
/// (none when it is empty), `{id}` stays.
fn expand<'a>(template: &'a str, prefix: &'a str) -> Vec<&'a str> {
    let mut out = vec![];
    for segment in template.split('/') {
        if segment == "{net}" {
            out.extend(prefix.split('/').filter(|s| !s.is_empty()));
        } else {
            out.push(segment);
        }
    }
    out
}

/// The `{id}` of a path whose segments are the template's on the prefix, exactly.
fn matched<'a>(template: &str, prefix: &str, segments: &[&'a str]) -> Option<&'a str> {
    let want = expand(template, prefix);
    if want.len() != segments.len() {
        return None;
    }
    let mut id = None;
    for (w, s) in want.iter().zip(segments) {
        match *w {
            "{id}" => id = Some(*s),
            w if w == *s => {}
            _ => return None,
        }
    }
    id
}

impl Explorer {
    fn on(&self, host: &str) -> bool {
        self.hosts
            .iter()
            .any(|h| h.strip_prefix("www.").unwrap_or(h) == host)
    }

    /// What a path on this explorer names: the first network and path it matches whose id is
    /// valid there. No Lightning object is on testnet4.
    fn read(&self, segments: &[&str]) -> Option<Target> {
        let segments = match segments {
            [l, rest @ ..] if self.languages && language(l) => rest,
            all => all,
        };
        self.networks.iter().find_map(|(network, prefix)| {
            self.paths.iter().find_map(|(kind, template)| {
                let object = object(*kind, matched(template, prefix, segments)?, *network)?;
                let lightning = matches!(object, Object::Node(_) | Object::Channel(_));
                (!(lightning && *network == Network::Testnet4)).then_some(Target {
                    network: *network,
                    object,
                })
            })
        })
    }

    /// The object's page here, or `None` when this explorer doesn't show its kind on its
    /// network.
    pub fn page(&self, target: &Target) -> Option<String> {
        let prefix = self.networks.iter().find(|(n, _)| *n == target.network)?.1;
        let (kinds, id): (&[Kind], String) = match &target.object {
            Object::Block(BlockRef::Hash(h)) => (&[Kind::Block, Kind::Hash], h.clone()),
            Object::Block(BlockRef::Height(h)) => (&[Kind::Block, Kind::Height], h.to_string()),
            Object::Tx(id) => (&[Kind::Tx], id.clone()),
            Object::Address(a) => (&[Kind::Address], a.clone()),
            Object::Node(k) => (&[Kind::Node], k.clone()),
            Object::Channel(c) => (&[Kind::Channel], c.to_string()),
        };
        let template = self.paths.iter().find(|(k, _)| kinds.contains(k))?.1;
        let path: Vec<&str> = expand(template, prefix)
            .into_iter()
            .map(|s| if s == "{id}" { id.as_str() } else { s })
            .collect();
        Some(format!("https://{}/{}", self.hosts[0], path.join("/")))
    }
}

/// What a link names: a page or API URL on an explorer of [`EXPLORERS`] (with an optional
/// language and network prefix where the explorer has them), or a `bitcoin:` URI, which names a
/// mainnet address.
pub fn from_uri(uri: &str) -> Option<Target> {
    if let Some(rest) = uri
        .get(..8)
        .filter(|scheme| scheme.eq_ignore_ascii_case("bitcoin:"))
        .map(|_| &uri[8..])
    {
        let addr = rest.split(['?', '#']).next()?;
        return Some(Target {
            network: Network::Mainnet,
            object: Object::Address(address(addr, Network::Mainnet)?),
        });
    }
    let (host, segments) = host_and_segments(uri)?;
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    EXPLORERS
        .iter()
        .filter(|e| e.on(&host))
        .find_map(|e| e.read(&segments))
}

/// The object's pages on the other explorers a card lists in Also on, in the table's order, as
/// `(site, page)`: each reads back ([`from_uri`]) as the object.
pub fn links(target: &Target) -> Vec<(&'static str, String)> {
    EXPLORERS
        .iter()
        .filter(|e| e.listed)
        .filter_map(|e| Some((e.site, e.page(target)?)))
        .collect()
}

/// The identifier every link form of the object is an alias of, or `None` for a block by height
/// (its hash comes from `height_request`).
pub fn identifier(target: &Target) -> Option<String> {
    let (chain, ln) = (target.network.chain(), target.network.lightning());
    Some(match &target.object {
        Object::Block(BlockRef::Hash(h)) => format!("{chain}:block/{h}"),
        Object::Block(BlockRef::Height(_)) => return None,
        Object::Tx(id) => format!("{chain}:tx/{id}"),
        Object::Address(a) => format!("{chain}:address/{a}"),
        Object::Node(k) => format!("{ln}:node/{k}"),
        Object::Channel(id) => format!("{ln}:channel/{id}"),
    })
}

/// The object's page on mempool.space.
pub fn page(target: &Target) -> String {
    MEMPOOL_SPACE
        .page(target)
        .expect("mempool.space shows every kind of object on every network")
}

/// Where a request goes: mempool.space, or the Esplora API at blockstream.info, the fallback
/// reader for on-chain objects on every network (`esplora_base`); Lightning objects have no
/// fallback. Esplora's records have the shapes of mempool.space's, so
/// the same parsers read them, with the same roots and pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Mempool,
    Esplora,
}

/// The Esplora API base at blockstream.info for a network the fallback reads: mainnet, testnet,
/// testnet4 and signet.
fn esplora_base(network: Network) -> Option<&'static str> {
    match network {
        Network::Mainnet => Some("https://blockstream.info/api"),
        Network::Testnet => Some("https://blockstream.info/testnet/api"),
        Network::Testnet4 => Some("https://blockstream.info/testnet4/api"),
        Network::Signet => Some("https://blockstream.info/signet/api"),
    }
}

fn api(api: Api, network: Network, path: &str) -> Option<Request> {
    let base = match api {
        Api::Mempool => format!("https://mempool.space{}/api", network.path()),
        Api::Esplora => esplora_base(network)?.to_owned(),
    };
    Request::get(&format!("{base}/{path}"))
}

/// The API record of the object; a block by height first needs `height_request`. Esplora reads
/// on-chain objects only.
pub fn request(to: Api, target: &Target) -> Option<Request> {
    let n = target.network;
    match (&target.object, to) {
        (Object::Block(BlockRef::Hash(h)), Api::Mempool) => api(to, n, &format!("v1/block/{h}")),
        (Object::Block(BlockRef::Hash(h)), Api::Esplora) => api(to, n, &format!("block/{h}")),
        (Object::Block(BlockRef::Height(h)), _) => height_request(to, n, *h),
        // A transaction lists every input and output: a consolidation's record passes the
        // egress default.
        (Object::Tx(id), _) => api(to, n, &format!("tx/{id}")).map(|r| r.cap(TXS_CAP)),
        (Object::Address(a), _) => api(to, n, &format!("address/{a}")),
        (Object::Node(k), Api::Mempool) => api(to, n, &format!("v1/lightning/nodes/{k}")),
        (Object::Channel(id), Api::Mempool) => api(to, n, &format!("v1/lightning/channels/{id}")),
        (Object::Node(_) | Object::Channel(_), Api::Esplora) => None,
    }
}

const PLAIN: &[&str] = &["text/plain"];

/// The hash of the best-chain block at a height (plain text).
pub fn height_request(to: Api, network: Network, height: u64) -> Option<Request> {
    Some(api(to, network, &format!("block-height/{height}"))?.admit(PLAIN))
}
/// Whether a block is in the best chain, and its height.
pub fn status_request(to: Api, network: Network, hash: &str) -> Option<Request> {
    api(to, network, &format!("block/{}/status", self::hash(hash)?))
}
/// The height of the best chain's tip (plain text).
pub fn tip_request(to: Api, network: Network) -> Option<Request> {
    Some(api(to, network, "blocks/tip/height")?.admit(PLAIN))
}

/// The block hash at a height, or `None` when the chain is not that long yet.
pub fn parse_height(status: u16, raw: &[u8]) -> Result<Option<String>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    hash(std::str::from_utf8(raw)?.trim())
        .map(Some)
        .context("not a block hash")
}

/// The Lightning graph's search for a node key: the check `node_listed` makes when the node's
/// own record fails.
pub fn search_request(network: Network, key: &str) -> Option<Request> {
    api(
        Api::Mempool,
        network,
        &format!("v1/lightning/search?searchText={}", node_key(key)?),
    )
}

/// Whether the search lists a node with exactly this key.
pub fn node_listed(status: u16, raw: &[u8], key: &str) -> Result<bool> {
    let v = json(status, raw, &[])?.context("no answer")?;
    let nodes = v["nodes"].as_array().context("no nodes")?;
    Ok(nodes
        .iter()
        .any(|n| n["public_key"].as_str().and_then(node_key).as_deref() == Some(key)))
}

/// The tip height.
pub fn parse_tip(status: u16, raw: &[u8]) -> Result<u64> {
    let raw = body(status, raw, &[])?.context("no tip")?;
    Ok(std::str::from_utf8(raw)?.trim().parse()?)
}

/// Whether a block's height names it for good: it is in the best chain and at least
/// `SETTLED_DEPTH` blocks deep (its own block counts as one).
pub fn settled(status: u16, raw: &[u8], tip: u64) -> Result<bool> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(false);
    };
    let height = v["height"].as_u64().context("no height")?;
    Ok(v["in_best_chain"].as_bool() == Some(true) && deep(height, tip))
}

/// Whether a best-chain block at `height` is `SETTLED_DEPTH` blocks deep at `tip` (its own
/// block counts as one).
fn deep(height: u64, tip: u64) -> bool {
    tip.saturating_add(1) >= height.saturating_add(SETTLED_DEPTH)
}

/// A block's place in the chain (`parse_status`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// It is in the best chain.
    pub best: bool,
    /// The best chain's next block (its hash) and that block's height.
    pub next: Option<(String, u64)>,
}

/// A block's place in the chain, from its status: whether it is in the best chain, and the best
/// chain's next block (`next_best`) with its height. `None` when the API knows no such block; an
/// error for a `next_best` that is not a block hash, or one without the block's height.
pub fn parse_status(status: u16, raw: &[u8]) -> Result<Option<Place>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let best = v["in_best_chain"].as_bool() == Some(true);
    let next = match &v["next_best"] {
        Value::Null => None,
        next => Some((
            next.as_str()
                .and_then(hash)
                .context("next_best is not a block hash")?,
            v["height"]
                .as_u64()
                .and_then(|h| h.checked_add(1))
                .context("no height")?,
        )),
    };
    Ok(Some(Place {
        best,
        next: next.filter(|_| best),
    }))
}

/// The id of a block's first transaction, its coinbase (plain text).
pub fn coinbase_request(to: Api, network: Network, hash: &str) -> Option<Request> {
    Some(api(to, network, &format!("block/{}/txid/0", self::hash(hash)?))?.admit(PLAIN))
}

/// A transaction id answered as plain text.
pub fn parse_txid(status: u16, raw: &[u8]) -> Result<String> {
    let raw = body(status, raw, &[])?.context("no transaction id")?;
    hash(std::str::from_utf8(raw)?.trim()).context("not a transaction id")
}

/// The largest transaction record (`request`) and list of an address's transactions
/// (`txs_request`) read: a few dozen whole transactions, or one with a few thousand inputs, which
/// an exchange's batch payouts and consolidations can take past the egress default.
pub const TXS_CAP: usize = 4 * crate::resolver::egress::BODY_CAP;

/// An address's latest transactions, those in the mempool first, then the confirmed ones,
/// newest first (mempool.space and Esplora list a few dozen whole; one request of at most
/// `TXS_CAP`).
pub fn txs_request(to: Api, network: Network, addr: &str) -> Option<Request> {
    Some(
        api(
            to,
            network,
            &format!("address/{}/txs", address(addr, network)?),
        )?
        .cap(TXS_CAP),
    )
}

/// The first `RELATED_EACH` transactions of an address's list (`txs_request`), in its order:
/// each `tx` with what it moved into (positive) or out of (negative) the address, and its block
/// time once confirmed.
pub fn parse_address_txs(
    status: u16,
    raw: &[u8],
    network: Network,
    addr: &str,
) -> Result<Vec<Related>> {
    let v = json(status, raw, &[])?.context("no transactions")?;
    let txs = v.as_array().context("not a list of transactions")?;
    let mut out = vec![];
    for tx in txs {
        let (Some(txid), Some(vin), Some(vout)) = (
            tx["txid"].as_str().and_then(hash),
            tx["vin"].as_array(),
            tx["vout"].as_array(),
        ) else {
            continue;
        };
        let received = paid_to(vout.iter(), network, addr);
        let sent = paid_to(vin.iter().map(|i| &i["prevout"]), network, addr);
        let mut r = Related::new("tx", root(network, Object::Tx(txid)));
        r.amount_sat = Some(received.saturating_sub(sent));
        let status = &tx["status"];
        r.at = if status["confirmed"].as_bool() == Some(true) {
            unix(&status["block_time"])
        } else {
            None
        };
        out.push(r);
        if out.len() == RELATED_EACH {
            break;
        }
    }
    Ok(out)
}

/// The value of the coins among `outs` (outputs, or the coins inputs spend) paid to `addr`.
fn paid_to<'a>(outs: impl Iterator<Item = &'a Value>, network: Network, addr: &str) -> i64 {
    outs.filter(|o| {
        o["scriptpubkey_address"]
            .as_str()
            .and_then(|a| address(a, network))
            .as_deref()
            == Some(addr)
    })
    .filter_map(|o| o["value"].as_u64().and_then(|v| i64::try_from(v).ok()))
    .fold(0i64, i64::saturating_add)
}

/// A node's open channels on mempool.space: its first page of them, the biggest by capacity
/// (10).
pub fn channels_request(network: Network, key: &str) -> Option<Request> {
    api(
        Api::Mempool,
        network,
        &format!(
            "v1/lightning/channels?public_key={}&status=open",
            node_key(key)?
        ),
    )
}

/// A node's channels (`channel`: the channel's root, its capacity and the peer's alias) and
/// their peers (`peer`: the node's root and alias, each once), the biggest channel first, at
/// most `RELATED_EACH` of each.
pub fn parse_channels(
    status: u16,
    raw: &[u8],
    network: Network,
    key: &str,
) -> Result<(Vec<Related>, Vec<Related>)> {
    let v = json(status, raw, &[])?.context("no channels")?;
    let list = v.as_array().context("not a list of channels")?;
    let mut channels = vec![];
    for c in list {
        let id = match &c["id"] {
            Value::String(s) => s.parse::<u64>().ok(),
            n => n.as_u64(),
        };
        let Some(id) = id.filter(|id| *id > 0) else {
            continue;
        };
        let peer = c["node"]["public_key"]
            .as_str()
            .and_then(node_key)
            .filter(|p| p != key);
        let mut r = Related::new("channel", root(network, Object::Channel(id)));
        r.name = text(&c["node"]["alias"]);
        r.amount_sat = c["capacity"].as_u64().and_then(|v| i64::try_from(v).ok());
        channels.push((r, peer));
    }
    // Biggest first; the API's order among equals.
    channels.sort_by_key(|(r, _)| std::cmp::Reverse(r.amount_sat));
    channels.truncate(RELATED_EACH);
    let mut peers: Vec<Related> = vec![];
    for (channel, peer) in &channels {
        let Some(peer) = peer else { continue };
        let uri = root(network, Object::Node(peer.clone()));
        if !peers.iter().any(|p| p.uri == uri) {
            let mut p = Related::new("peer", uri);
            p.name.clone_from(&channel.name);
            peers.push(p);
        }
    }
    Ok((channels.into_iter().map(|(r, _)| r).collect(), peers))
}

fn on(network: Network, object: Object) -> Target {
    Target { network, object }
}

/// The root of an object on a network (`None` only for a block by height).
fn root(network: Network, object: Object) -> Option<String> {
    identifier(&on(network, object))
}

/// A block's entry in a card's `related` list, named by its height when it is known.
fn block_entry(rel: &str, network: Network, hash: &str, height: Option<u64>) -> Related {
    let mut r = Related::new(
        rel,
        root(network, Object::Block(BlockRef::Hash(hash.into()))),
    );
    r.name = height.map(|h| format!("Block {}", grouped(h)));
    r
}

/// The first `RELATED_EACH` of `entries` into the card's `related` list, and `total` as their
/// `rel`'s count when the list holds fewer.
fn relate_first(card: &mut Card, rel: &str, entries: Vec<Related>, total: u64) {
    let listed = entries.len().min(RELATED_EACH);
    card.related.extend(entries.into_iter().take(RELATED_EACH));
    if total > listed as u64 {
        card.related_total.insert(rel.into(), total);
    }
}

/// `entries` of one `rel`, the largest amount first (in the API's order among equals), as
/// `relate_first` takes them, with all of them as the count.
fn relate_largest(card: &mut Card, rel: &str, mut entries: Vec<Related>) {
    let total = entries.len() as u64;
    entries.sort_by_key(|r| std::cmp::Reverse(r.amount_sat));
    relate_first(card, rel, entries, total);
}

/// What a coin is paid to: the address's root, or, for a script without an address, no root and
/// a label (`OP_RETURN`, `P2PK`, or the API's name of the script type); with its value.
fn coin(rel: &str, out: &Value, network: Network) -> Related {
    let addr = out["scriptpubkey_address"]
        .as_str()
        .and_then(|a| address(a, network));
    let mut r = Related::new(rel, addr.and_then(|a| root(network, Object::Address(a))));
    if r.uri.is_none() {
        r.name = Some(match out["scriptpubkey_type"].as_str().map(str::trim) {
            Some("op_return") => "OP_RETURN".into(),
            Some("p2pk") => "P2PK".into(),
            Some(kind)
                if (1..=32).contains(&kind.len())
                    && kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
            {
                kind.into()
            }
            _ => "unknown".into(),
        });
    }
    r.amount_sat = out["value"].as_u64().and_then(|v| i64::try_from(v).ok());
    r
}

/// What an input spends: the coin it spends (`coin`), or, for a coinbase input, no root and the
/// label `coinbase`.
fn input(vin: &Value, network: Network) -> Related {
    if vin["is_coinbase"].as_bool() == Some(true) {
        let mut r = Related::new("input", None);
        r.name = Some("coinbase".into());
        return r;
    }
    coin("input", &vin["prevout"], network)
}

fn new_card(type_name: &'static str, t: &Target) -> Card {
    Card::new(
        type_name,
        identifier(t).expect("an identified object"),
        page(t),
    )
}

/// A block card from a mempool.space or Esplora block record.
pub(crate) fn block_card(v: &Value, network: Network, hash: &str) -> Result<Card> {
    ensure!(
        v["id"].as_str().and_then(self::hash).as_deref() == Some(hash),
        "the API answered for another block"
    );
    let t = on(network, Object::Block(BlockRef::Hash(hash.into())));
    let mut card = new_card("BitcoinBlock", &t);
    let height = v["height"].as_u64().context("no height")?;
    card.name = Some(format!("Block {}", grouped(height)));
    card.start = Some(unix(&v["timestamp"]).context("no block time")?);
    card.detail("height", height);
    card.detail("hash", hash);
    for (name, field) in [
        ("tx_count", "tx_count"),
        ("size", "size"),
        ("weight", "weight"),
    ] {
        card.detail(name, v[field].as_u64());
    }
    card.detail("previous_block", text(&v["previousblockhash"]));
    if let Some(previous) = v["previousblockhash"].as_str().and_then(self::hash) {
        card.related.push(block_entry(
            "previous",
            network,
            &previous,
            height.checked_sub(1),
        ));
    }
    let extras = &v["extras"];
    card.detail("pool", text(&extras["pool"]["name"]));
    card.detail("total_fees", extras["totalFees"].as_u64());
    card.detail("reward", extras["reward"].as_u64());
    card.detail("median_fee_rate", float(&extras["medianFee"]));
    Ok(card)
}

/// A transaction card from a mempool.space or Esplora transaction record.
pub(crate) fn tx_card(v: &Value, network: Network, txid: &str) -> Result<Card> {
    ensure!(
        v["txid"].as_str().and_then(hash).as_deref() == Some(txid),
        "the API answered for another transaction"
    );
    let t = on(network, Object::Tx(txid.into()));
    let mut card = new_card("BitcoinTransaction", &t);
    card.name = Some(format!("Transaction {txid}"));
    let status = &v["status"];
    let confirmed = status["confirmed"].as_bool().context("no status")?;
    card.detail("confirmed", confirmed);
    if confirmed {
        card.start = unix(&status["block_time"]);
        card.detail("block_height", status["block_height"].as_u64());
        card.detail("block_hash", status["block_hash"].as_str().and_then(hash));
    }
    let weight = v["weight"].as_u64();
    card.detail("fee", v["fee"].as_u64());
    card.detail("size", v["size"].as_u64());
    card.detail("weight", weight);
    card.detail("vsize", weight.map(|w| w.div_ceil(4)));
    let (vin, vout) = (v["vin"].as_array(), v["vout"].as_array());
    card.detail("inputs", vin.map(Vec::len));
    card.detail("outputs", vout.map(Vec::len));
    card.detail(
        "output_value",
        vout.map(|o| {
            o.iter()
                .filter_map(|x| x["value"].as_u64())
                .fold(0u64, u64::saturating_add)
        }),
    );
    card.detail(
        "coinbase",
        vin.and_then(|i| i.first())
            .and_then(|i| i["is_coinbase"].as_bool()),
    );
    // The block it is in, then its inputs and outputs, the largest first.
    if let Some(block) = status["block_hash"]
        .as_str()
        .and_then(hash)
        .filter(|_| confirmed)
    {
        let mut r = block_entry("block", network, &block, status["block_height"].as_u64());
        r.at = unix(&status["block_time"]);
        card.related.push(r);
    }
    if let Some(vin) = vin {
        let inputs = vin.iter().map(|i| input(i, network)).collect();
        relate_largest(&mut card, "input", inputs);
    }
    if let Some(vout) = vout {
        let outputs = vout.iter().map(|o| coin("output", o, network)).collect();
        relate_largest(&mut card, "output", outputs);
    }
    Ok(card)
}

/// An address card from a mempool.space or Esplora address record.
pub(crate) fn address_card(v: &Value, network: Network, addr: &str) -> Result<Card> {
    ensure!(
        v["address"]
            .as_str()
            .and_then(|a| address(a, network))
            .as_deref()
            == Some(addr),
        "the API answered for another address"
    );
    let t = on(network, Object::Address(addr.into()));
    let mut card = new_card("BitcoinAddress", &t);
    card.name = Some(addr.into());
    let chain = &v["chain_stats"];
    let (funded, spent) = (
        chain["funded_txo_sum"].as_u64(),
        chain["spent_txo_sum"].as_u64(),
    );
    card.detail("tx_count", chain["tx_count"].as_u64());
    card.detail("received", funded);
    card.detail(
        "balance",
        funded.zip(spent).map(|(f, s)| f.saturating_sub(s)),
    );
    card.detail(
        "unconfirmed_tx_count",
        v["mempool_stats"]["tx_count"].as_u64(),
    );
    Ok(card)
}

/// A block, or `None` when there is no block with this hash.
pub fn parse_block(status: u16, raw: &[u8], network: Network, hash: &str) -> Result<Option<Card>> {
    json(status, raw, &[404])?
        .map(|v| block_card(&v, network, hash))
        .transpose()
}

/// A transaction, or `None` when it is unknown (never broadcast, or dropped from the mempool).
pub fn parse_tx(status: u16, raw: &[u8], network: Network, txid: &str) -> Result<Option<Card>> {
    json(status, raw, &[404])?
        .map(|v| tx_card(&v, network, txid))
        .transpose()
}

/// An address. Every valid address exists, so there is no "missing" answer.
pub fn parse_address(
    status: u16,
    raw: &[u8],
    network: Network,
    addr: &str,
) -> Result<Option<Card>> {
    json(status, raw, &[])?
        .map(|v| address_card(&v, network, addr))
        .transpose()
}

/// A Lightning node, or `None` when the graph has no such node. mempool.space answers 500
/// ("Failed to get node") for an unknown key as well as on a fault, so a 500 is retried.
pub fn parse_node(status: u16, raw: &[u8], network: Network, key: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        v["public_key"].as_str().and_then(node_key).as_deref() == Some(key),
        "the API answered for another node"
    );
    let t = on(network, Object::Node(key.into()));
    let mut card = new_card("LightningNode", &t);
    card.name = text(&v["alias"]).or_else(|| Some(key.into()));
    // A node's location is IP geolocation, not a place: no point.
    card.detail("first_seen", unix(&v["first_seen"]).map(|t| t.to_rfc3339()));
    card.detail("public_key", key);
    card.detail("alias", text(&v["alias"]));
    card.detail("color", text(&v["color"]));
    card.detail("capacity", int(&v["capacity"]));
    card.detail("channels", v["active_channel_count"].as_u64());
    card.detail("updated_at", unix(&v["updated_at"]).map(|t| t.to_rfc3339()));
    Ok(Some(card))
}

/// A Lightning channel, or `None` when the graph has no such channel.
pub fn parse_channel(status: u16, raw: &[u8], network: Network, id: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        match &v["id"] {
            Value::String(s) => s.parse::<u64>().ok(),
            n => n.as_u64(),
        } == Some(id),
        "the API answered for another channel"
    );
    let t = on(network, Object::Channel(id));
    let mut card = new_card("LightningChannel", &t);
    let short = short_channel_id(id);
    card.name = Some(format!("Channel {short}"));
    card.detail("created", time(&v["created"]).map(|t| t.to_rfc3339()));
    card.detail("closed", time(&v["closing_date"]).map(|t| t.to_rfc3339()));
    card.detail("short_id", short);
    card.detail("capacity", v["capacity"].as_u64());
    card.detail(
        "status",
        match v["status"].as_u64() {
            Some(0) => Some("inactive"),
            Some(1) => Some("active"),
            Some(2) => Some("closed"),
            _ => None,
        },
    );
    card.detail("funding_tx", v["transaction_id"].as_str().and_then(hash));
    card.detail("funding_output", v["transaction_vout"].as_u64());
    card.detail(
        "closing_tx",
        v["closing_transaction_id"].as_str().and_then(hash),
    );
    for (side, name, alias) in [
        ("node_left", "node1", "node1_alias"),
        ("node_right", "node2", "node2_alias"),
    ] {
        card.detail(name, v[side]["public_key"].as_str().and_then(node_key));
        card.detail(alias, text(&v[side]["alias"]));
    }
    // Its two nodes, and the transactions that opened and closed it.
    for side in ["node_left", "node_right"] {
        if let Some(key) = v[side]["public_key"].as_str().and_then(node_key) {
            let mut r = Related::new("node", root(network, Object::Node(key)));
            r.name = text(&v[side]["alias"]);
            card.related.push(r);
        }
    }
    for (rel, field) in [
        ("funding_tx", "transaction_id"),
        ("closing_tx", "closing_transaction_id"),
    ] {
        if let Some(tx) = v[field].as_str().and_then(hash) {
            let mut r = Related::new(rel, root(network, Object::Tx(tx)));
            if rel == "funding_tx" {
                r.amount_sat = v["capacity"].as_u64().and_then(|c| i64::try_from(c).ok());
            }
            card.related.push(r);
        }
    }
    Ok(Some(card))
}

/// The card of an object read from mempool.space or Esplora (the same record shapes), or
/// `None` when the API says it does not exist. A block by height needs its hash first.
pub fn parse(status: u16, raw: &[u8], target: &Target) -> Result<Option<Card>> {
    let n = target.network;
    match &target.object {
        Object::Block(BlockRef::Hash(h)) => parse_block(status, raw, n, h),
        Object::Block(BlockRef::Height(_)) => anyhow::bail!("a block by height needs its hash"),
        Object::Tx(id) => parse_tx(status, raw, n, id),
        Object::Address(a) => parse_address(status, raw, n, a),
        Object::Node(k) => parse_node(status, raw, n, k),
        Object::Channel(id) => parse_channel(status, raw, n, *id),
    }
}

/// The object of a root (`btc:tx/…`, `ln-signet:node/…`).
pub fn from_root(uri: &str) -> Option<Target> {
    let (scheme, rest) = uri.split_once(':')?;
    let network = [
        Network::Mainnet,
        Network::Testnet,
        Network::Testnet4,
        Network::Signet,
    ];
    let (network, lightning) = network.into_iter().find_map(|n| {
        (scheme == n.chain())
            .then_some((n, false))
            .or_else(|| (scheme == n.lightning()).then_some((n, true)))
    })?;
    let (kind, id) = rest.split_once('/')?;
    let object = match (lightning, kind) {
        (false, "block") => Object::Block(BlockRef::Hash(hash(id).filter(|h| h == id)?)),
        (false, "tx") => Object::Tx(hash(id).filter(|h| h == id)?),
        (false, "address") => Object::Address(address(id, network).filter(|a| a == id)?),
        (true, "node") if network != Network::Testnet4 => {
            Object::Node(node_key(id).filter(|k| k == id)?)
        }
        (true, "channel") if network != Network::Testnet4 => Object::Channel(
            plain(id, 20, |c| c.is_ascii_digit())
                .then(|| channel_id(id))
                .flatten()
                .filter(|c| c.to_string() == id)?,
        ),
        _ => return None,
    };
    Some(Target { network, object })
}

/// The object of a root, a mempool.space link or a `bitcoin:` URI.
pub fn target(uri: &str) -> Option<Target> {
    from_root(uri).or_else(|| from_uri(uri))
}

const ROOTS: &[&str] = &[
    "btc:",
    "btc-testnet:",
    "btc-testnet4:",
    "btc-signet:",
    "ln:",
    "ln-testnet:",
    "ln-testnet4:",
    "ln-signet:",
];

/// The source: mempool.space, with Esplora as the fallback for on-chain objects.
pub struct Mempool;

/// What `read` found: the card; whether its root names the link for good (always, except for a
/// block by height that is not yet `settled`); and whether to read it again within the hour
/// (`READ_AGAIN_SECS`): a part of it is not there yet (a best-chain block's settled next block,
/// a transaction's block), a request for one was put off (`RetryLater`), or it is a block read
/// from Esplora, whose record has no pool, fees or reward.
struct Read {
    card: Card,
    named: bool,
    soon: bool,
}

/// Whether a failed request for a part of a card is only put off (a rate limit, a server
/// error): the card is read again soon. Any other failure leaves the part out until the card's
/// own refresh.
fn put_off(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::resolver::RetryLater>()
        .is_some()
}

/// A request for a part of a card, answered by `by`: one still waiting then is put off
/// (`RetryLater`), as a rate limit is.
async fn part(
    cx: &super::Context<'_>,
    request: Option<Request>,
    by: tokio::time::Instant,
) -> Result<Fetched> {
    let request = request.context("no reader for this part")?;
    match tokio::time::timeout_at(by, fetch(cx, &request)).await {
        Ok(fetched) => fetched,
        Err(_) => Err(crate::resolver::RetryLater {
            class: "timeout",
            seconds: 60,
        }
        .into()),
    }
}

/// What `parse` reads from a fetched answer, in a parse slot: a transaction's record or an
/// address's list can be megabytes (`TXS_CAP`), more than the async runtime should parse and
/// sort.
async fn parsed<T, F>(f: Fetched, parse: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(u16, &[u8]) -> Result<T> + Send + 'static,
{
    crate::resolver::html::blocking(move || parse(f.status, &f.body)).await?
}

/// The tip height.
async fn read_tip(cx: &super::Context<'_>, to: Api, network: Network) -> Result<u64> {
    let tip = fetch(cx, &tip_request(to, network).context("tip")?).await?;
    parse_tip(tip.status, &tip.body)
}

/// Reads the object from one API: a block by height through its hash. Then what its card points
/// at beyond its own record (`related`), one request per part (`part`, answered by `by`), each
/// left out when it fails: a block's next block (its status, and the tip when it has one) and
/// its coinbase transaction; an address's latest transactions; a node's biggest channels. A
/// block read by height needs its status and the tip whatever the time: their failure fails the
/// read, as before.
async fn read(
    cx: &super::Context<'_>,
    to: Api,
    target: &Target,
    by: tokio::time::Instant,
) -> Result<Option<Read>> {
    let mut target = target.clone();
    let by_height = matches!(target.object, Object::Block(BlockRef::Height(_)));
    if by_height {
        let f = fetch(cx, &request(to, &target).context("no reader")?).await?;
        let Some(hash) = parse_height(f.status, &f.body)? else {
            return Ok(None);
        };
        target.object = Object::Block(BlockRef::Hash(hash));
    }
    let f = fetch(cx, &request(to, &target).context("no reader")?).await?;
    let read = target.clone();
    let Some(mut card) = parsed(f, move |status, body| parse(status, body, &read)).await? else {
        return Ok(None);
    };
    let (n, mut named, mut soon) = (target.network, true, false);
    match &target.object {
        Object::Block(BlockRef::Hash(hash)) => {
            // Esplora's block record has no pool, fees or reward: mempool.space's come with a
            // later read.
            soon |= to == Api::Esplora;
            // Its place in the best chain: whether its height names it (read by height, which
            // needs both), and its next block, linked once that is settled too.
            let status = if by_height {
                Some(fetch(cx, &status_request(to, n, hash).context("block status")?).await?)
            } else {
                match part(cx, status_request(to, n, hash), by).await {
                    Ok(status) => Some(status),
                    Err(e) => {
                        soon |= put_off(&e);
                        None
                    }
                }
            };
            let place = match status.as_ref().map(|s| parse_status(s.status, &s.body)) {
                Some(Ok(place)) => place,
                Some(Err(e)) if by_height => return Err(e),
                _ => None,
            };
            // A block its status puts outside the best chain is stale (orphaned).
            if place.as_ref().is_some_and(|p| !p.best) {
                card.detail("stale", true);
            }
            let next = place.as_ref().and_then(|p| p.next.clone());
            let tip = if by_height {
                Some(read_tip(cx, to, n).await?)
            } else if next.is_some() {
                match part(cx, tip_request(to, n), by)
                    .await
                    .and_then(|f| parse_tip(f.status, &f.body))
                {
                    Ok(tip) => Some(tip),
                    Err(e) => {
                        soon |= put_off(&e);
                        None
                    }
                }
            } else {
                None
            };
            if let (true, Some(status), Some(tip)) = (by_height, &status, tip) {
                named = settled(status.status, &status.body, tip)?;
            }
            match (next, tip) {
                (Some((next, height)), Some(tip)) if deep(height, tip) => {
                    card.related
                        .push(block_entry("next", n, &next, Some(height)));
                }
                // Its next block is not settled yet, or it is the best chain's tip: the next block
                // comes with a later read.
                (Some(_), Some(_)) => soon = true,
                (None, _) if place.as_ref().is_some_and(|p| p.best) => soon = true,
                // A status or tip that failed was put off (`put_off`), or is left out for good.
                _ => {}
            }
            let coinbase = part(cx, coinbase_request(to, n, hash), by)
                .await
                .and_then(|f| parse_txid(f.status, &f.body));
            match coinbase {
                Ok(txid) => {
                    let mut r = Related::new("coinbase", root(n, Object::Tx(txid)));
                    r.amount_sat = card.details.get("reward").and_then(Value::as_i64);
                    card.related.push(r);
                }
                Err(e) => soon |= put_off(&e),
            }
        }
        Object::Address(addr) => {
            let txs = match part(cx, txs_request(to, n, addr), by).await {
                Ok(f) => {
                    let addr = addr.clone();
                    parsed(f, move |status, body| {
                        parse_address_txs(status, body, n, &addr)
                    })
                    .await
                }
                Err(e) => Err(e),
            };
            match txs {
                Ok(txs) => {
                    let count = |name: &str| card.details.get(name).and_then(Value::as_u64);
                    let total = count("tx_count")
                        .unwrap_or(0)
                        .saturating_add(count("unconfirmed_tx_count").unwrap_or(0));
                    relate_first(&mut card, "tx", txs, total);
                }
                Err(e) => soon |= put_off(&e),
            }
        }
        Object::Node(key) => {
            let channels = part(cx, channels_request(n, key), by)
                .await
                .and_then(|f| parse_channels(f.status, &f.body, n, key));
            match channels {
                Ok((channels, peers)) => {
                    let total = card.details.get("channels").and_then(Value::as_u64);
                    relate_first(&mut card, "channel", channels, total.unwrap_or(0));
                    relate_first(&mut card, "peer", peers, 0);
                }
                Err(e) => soon |= put_off(&e),
            }
        }
        // An unconfirmed transaction's block comes with a later read.
        Object::Tx(_) => soon = card.details.get("confirmed") == Some(&Value::Bool(false)),
        Object::Block(BlockRef::Height(_)) | Object::Channel(_) => {}
    }
    Ok(Some(Read { card, named, soon }))
}

#[async_trait]
impl Source for Mempool {
    fn name(&self) -> &'static str {
        "mempool"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[
                E2eSample {
                    kind: "mempool",
                    uri: "https://mempool.space/lightning/node/03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
                    card_type: "LightningNode",
                    group_with: None,
                },
            ],
            answers: &[
                E2eAnswer {
                    host: "mempool.space",
                    path: "/api/v1/lightning/nodes/03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
                    query: &[],
                    header: None,
                    content_type: "application/json",
                    file: "node-acinq.json",
                },
                E2eAnswer {
                    host: "mempool.space",
                    path: "/api/v1/lightning/channels",
                    query: &[
                        (
                            "public_key",
                            "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
                        ),
                        ("status", "open"),
                    ],
                    header: None,
                    content_type: "application/json",
                    file: "channels-acinq.json",
                },
            ],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?)?;
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        ROOTS.iter().any(|r| uri.starts_with(r)) && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "first_seen",
            "created",
            "closed",
            // Blocks (`tx_count` also on addresses, `size` and `weight` also on transactions)
            "height",
            "hash",
            "tx_count",
            "size",
            "weight",
            "previous_block",
            "pool",
            "total_fees",
            "reward",
            "median_fee_rate",
            "stale",
            // Transactions
            "confirmed",
            "block_height",
            "block_hash",
            "fee",
            "vsize",
            "inputs",
            "outputs",
            "output_value",
            "coinbase",
            // Addresses
            "received",
            "balance",
            "unconfirmed_tx_count",
            // Lightning
            "public_key",
            "alias",
            "color",
            "capacity",
            "channels",
            "updated_at",
            "short_id",
            "status",
            "funding_tx",
            "funding_output",
            "closing_tx",
            "node1",
            "node1_alias",
            "node2",
            "node2_alias",
        ]
    }
    fn budgets(&self) -> &'static [Budget] {
        &[("mempool.space", 1.0), ("blockstream.info", 1.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://mempool.space/tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d",
                card_type: "BitcoinTransaction",
            },
            Example {
                uri: "https://mempool.space/block/481824",
                card_type: "BitcoinBlock",
            },
            Example {
                uri: "https://mempool.space/lightning/node/03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
                card_type: "LightningNode",
            },
        ]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(target) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        // The parts' requests share one budget, the fallback's too.
        let by = tokio::time::Instant::now() + PARTS_BUDGET;
        let by = cx.deadline().map_or(by, |d| d.min(by));
        let read = match read(cx, Api::Mempool, &target, by).await {
            Ok(read) => read,
            // The fallback's "not found" does not say the object is gone while mempool.space
            // fails (a transaction Esplora's node has not seen): mempool.space's error stands,
            // as it does when the fallback fails too.
            Err(e) if request(Api::Esplora, &target).is_some() => {
                match read(cx, Api::Esplora, &target, by).await {
                    Ok(Some(read)) => Some(read),
                    Ok(None) | Err(_) => return Err(e),
                }
            }
            Err(e) => match (
                &target.object,
                e.downcast_ref::<crate::resolver::RetryLater>(),
            ) {
                // mempool.space answers a server error for a node it doesn't know as well as on a
                // fault: the search tells them apart. A node it doesn't list is missing; a listed
                // node whose record keeps failing counts toward the failure limit. A rate limit is
                // only a retry.
                (Object::Node(key), Some(r)) if r.class == "upstream" => {
                    let search = search_request(target.network, key).context("node search")?;
                    let f = fetch(cx, &search).await?;
                    if !node_listed(f.status, &f.body, key)? {
                        return Ok(cx.missing(uri, self));
                    }
                    return Err(anyhow::Error::new(crate::resolver::ErrorClass("http_5xx"))
                        .context("mempool.space fails the record of a node it lists"));
                }
                _ => return Err(e),
            },
        };
        Ok(match read {
            Some(Read { card, named, soon }) => {
                let mut doc = card.into_doc(cx.card(uri, self), SITE);
                if !named {
                    doc.source_root = None;
                }
                if soon {
                    doc.refresh_by =
                        Some(chrono::Utc::now() + chrono::Duration::seconds(READ_AGAIN_SECS));
                }
                doc
            }
            None => cx.missing(uri, self),
        })
    }
}

/// A number with thousands separators, as block heights are written.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The also-on half of this source, `mempool`: a Bitcoin or Lightning object's pages on the other
/// explorers a card lists (`links`), from its root alone, with no request.
pub mod also_on {
    use crate::also_on::{self, Context, Source, Subject};
    use anyhow::Result;
    use async_trait::async_trait;

    /// The pages of a block, transaction, address, Lightning node or channel on the listed
    /// explorers of `EXPLORERS`, for the card of its root (`btc:…`, `ln:…`) once it
    /// is read: each is a link the object's card is the alias root of.
    pub struct Explorers;

    #[async_trait]
    impl Source for Explorers {
        fn id(&self) -> &'static str {
            "mempool"
        }
        fn applies(&self, subject: &Subject) -> bool {
            subject.status == "ok" && super::from_root(&subject.uri).is_some()
        }
        async fn lookup(&self, _cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            let links = super::from_root(&subject.uri)
                .map(|t| super::links(&t))
                .unwrap_or_default()
                .into_iter()
                .map(|(site, uri)| also_on::Link::new(uri, site, super::VIA))
                .collect();
            Ok(also_on::Found {
                links,
                ..Default::default()
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::lookups::fixture::{at, context, sites};

        const HASH: &str = "0000000000000000001c8018d9cb3b742ef25114f27563e3fc4a1902167f9893";

        #[tokio::test]
        async fn a_block_lists_the_explorers_and_each_reads_back_as_it() {
            let root = format!("btc:block/{HASH}");
            let subject = at(&root);
            assert!(Explorers.applies(&subject));
            let found = Explorers.lookup(&context(vec![]), &subject).await.unwrap();
            assert_eq!(
                sites(&found),
                [
                    format!("Blockstream Explorer https://blockstream.info/block/{HASH}"),
                    format!("BTCScan https://btcscan.org/block/{HASH}"),
                    format!("Blockchain.com https://www.blockchain.com/explorer/blocks/btc/{HASH}"),
                    format!("OKLink https://www.oklink.com/bitcoin/block/{HASH}"),
                ]
            );
            for link in &found.links {
                assert_eq!(link.via, "mempool");
                assert_eq!(
                    super::super::from_uri(&link.uri)
                        .as_ref()
                        .and_then(super::super::identifier),
                    Some(root.clone()),
                    "{}",
                    link.uri
                );
            }
            // Only a card that was read; never a link, a missing object or another scheme.
            let mut missing = at(&root);
            missing.status = "missing".into();
            assert!(!Explorers.applies(&missing));
            for uri in [
                format!("https://mempool.space/block/{HASH}"),
                "btc:block/481824".to_owned(),
                format!("https://blockstream.info/block/{HASH}"),
            ] {
                assert!(!Explorers.applies(&at(&uri)), "{uri}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    const HASH: &str = "0000000000000000001c8018d9cb3b742ef25114f27563e3fc4a1902167f9893";
    const PIZZA: &str = "a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d";
    const GENESIS: &str = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";
    const ACINQ: &str = "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f";

    fn read(name: &str) -> Vec<u8> {
        fixture::read("mempool", name)
    }
    fn id(uri: &str) -> Option<String> {
        from_uri(uri).as_ref().and_then(identifier)
    }

    #[test]
    fn every_link_form_of_one_object() {
        let tx = format!("btc:tx/{PIZZA}");
        for uri in [
            format!("https://mempool.space/tx/{PIZZA}"),
            format!("https://mempool.space/tx/{PIZZA}#vin=3"),
            format!("https://mempool.space/tx/{PIZZA}#vout=0"),
            format!("https://mempool.space/de/tx/{PIZZA}"),
            format!("https://www.mempool.space/tx/{}", PIZZA.to_uppercase()),
            format!("https://mempool.space/api/tx/{PIZZA}"),
        ] {
            assert_eq!(id(&uri).as_deref(), Some(tx.as_str()), "{uri}");
        }
        let block = format!("btc:block/{HASH}");
        for uri in [
            format!("https://mempool.space/block/{HASH}"),
            format!("https://mempool.space/ja/block/{HASH}"),
            format!("https://mempool.space/api/v1/block/{HASH}"),
            format!("https://mempool.space/api/block/{HASH}"),
        ] {
            assert_eq!(id(&uri).as_deref(), Some(block.as_str()), "{uri}");
        }
        for uri in [
            format!("https://mempool.space/address/{GENESIS}"),
            format!("bitcoin:{GENESIS}"),
            format!("bitcoin:{GENESIS}?amount=0.001&label=x"),
            format!("https://mempool.space/api/address/{GENESIS}"),
        ] {
            assert_eq!(id(&uri), Some(format!("btc:address/{GENESIS}")), "{uri}");
        }
        let bech = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
        for uri in [
            format!("https://mempool.space/address/{bech}"),
            format!("https://mempool.space/address/{}", bech.to_uppercase()),
            format!("bitcoin:{}", bech.to_uppercase()),
        ] {
            assert_eq!(id(&uri), Some(format!("btc:address/{bech}")), "{uri}");
        }
        for uri in [
            format!("https://mempool.space/lightning/node/{ACINQ}"),
            format!("https://mempool.space/fr/lightning/node/{ACINQ}"),
            format!("https://mempool.space/api/v1/lightning/nodes/{ACINQ}"),
        ] {
            assert_eq!(id(&uri), Some(format!("ln:node/{ACINQ}")), "{uri}");
        }
        for uri in [
            "https://mempool.space/lightning/channel/892785849701564416",
            "https://mempool.space/lightning/channel/811984x2037x0",
            "https://mempool.space/api/v1/lightning/channels/892785849701564416",
        ] {
            assert_eq!(
                id(uri).as_deref(),
                Some("ln:channel/892785849701564416"),
                "{uri}"
            );
        }
    }

    #[test]
    fn test_networks_are_separate_objects() {
        assert_eq!(
            id(&format!("https://mempool.space/testnet4/tx/{PIZZA}")),
            Some(format!("btc-testnet4:tx/{PIZZA}"))
        );
        assert_eq!(
            id(&format!("https://mempool.space/de/signet/block/{HASH}")),
            Some(format!("btc-signet:block/{HASH}"))
        );
        assert_eq!(
            id(&format!(
                "https://mempool.space/testnet/lightning/node/{ACINQ}"
            )),
            Some(format!("ln-testnet:node/{ACINQ}"))
        );
        // A testnet address on mainnet, and a mainnet address on a test network, are not valid.
        let tb = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";
        assert_eq!(
            id(&format!("https://mempool.space/testnet/address/{tb}")),
            Some(format!("btc-testnet:address/{tb}"))
        );
        assert_eq!(id(&format!("https://mempool.space/address/{tb}")), None);
        assert_eq!(
            id(&format!("https://mempool.space/signet/address/{GENESIS}")),
            None
        );
        // A `bitcoin:` URI can't say which test network it means.
        assert_eq!(id(&format!("bitcoin:{tb}")), None);
        let t = from_uri(&format!("https://mempool.space/testnet4/tx/{PIZZA}")).unwrap();
        assert_eq!(
            request(Api::Mempool, &t).unwrap().url.as_str(),
            format!("https://mempool.space/testnet4/api/tx/{PIZZA}")
        );
        assert_eq!(
            page(&t),
            format!("https://mempool.space/testnet4/tx/{PIZZA}")
        );
    }

    #[test]
    fn edge_cases() {
        // The URI scheme in any case.
        for scheme in ["bitcoin", "BITCOIN", "Bitcoin", "bItCoIn"] {
            assert_eq!(
                id(&format!("{scheme}:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa")).as_deref(),
                Some("btc:address/1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa"),
                "{scheme}"
            );
        }
        // BIP 350: a version-1 (taproot) address with a 32-byte program.
        let taproot = "bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqzk5jj0";
        assert_eq!(address(taproot, Network::Mainnet).as_deref(), Some(taproot));
        // A channel id above i64::MAX is still read and checked.
        let big = u64::MAX - 1;
        let raw = read("channel-892785849701564416.json");
        let mut v: Value = serde_json::from_slice(&raw).unwrap();
        v["id"] = big.to_string().into();
        let raw = serde_json::to_vec(&v).unwrap();
        assert!(parse_channel(200, &raw, Network::Mainnet, big)
            .unwrap()
            .is_some());
        assert!(parse_channel(200, &raw, Network::Mainnet, 892785849701564416).is_err());
        // The tip never overflows.
        assert!(settled(200, &read("block-481824-status.json"), u64::MAX).unwrap());
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://mempool.space:8443/tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d",
            "https://mempool.space/lightning/channel/0x0x0",
            // BIP 173: a valid checksum, but version 0 takes a 20- or 32-byte program, not 16.
            "https://mempool.space/address/BC1QR508D6QEJXTDG4Y5R3ZARVARYV98GJ9P",
            "https://mempool.space/address/bc1qr508d6qejxtdg4y5r3zarvaryv98gj9p",
            "https://mempool.space/tx/abc",
            "https://mempool.space/address/1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNb",
            "https://mempool.space/address/bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdx",
            "https://mempool.space/address/bc1qar0srrr7xfkvy5l643lydnw9rE59gtzzwf5mdq",
            "https://mempool.space/lightning/node/04864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
            "https://mempool.space/lightning/channel/0",
            "https://mempool.space/lightning/channel/811984x2037",
            "https://mempool.space/lightning/channel/811984x2037x70000",
            "https://mempool.space/mining/pool/foundryusa",
            "https://mempool.space/xx/yy/tx/abc",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        assert_eq!(
            from_uri(&format!(
                "https://mempool.space/testnet4/lightning/node/{ACINQ}"
            )),
            None
        );
    }

    /// A link template with the test objects' ids in it.
    fn fill(template: &str) -> String {
        template
            .replace("{HASH}", HASH)
            .replace("{PIZZA}", PIZZA)
            .replace("{GENESIS}", GENESIS)
            .replace("{ACINQ}", ACINQ)
            .replace("{TB}", "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx")
    }

    /// Each explorer's link of an object names the same identifier as its mempool.space link.
    #[test]
    fn every_explorer_names_the_one_object() {
        let cases: &[(&str, &[&str])] = &[
            (
                "btc:block/{HASH}",
                &[
                    "https://blockstream.info/block/{HASH}",
                    "https://mempool.emzy.de/de/block/{HASH}/",
                    "https://btcscan.org/block/{HASH}",
                    "https://www.blockchain.com/explorer/blocks/btc/{HASH}",
                    "https://www.blockchain.com/btc/block/{HASH}",
                    "https://blockchain.info/block/{HASH}",
                    "https://www.oklink.com/bitcoin/block/{HASH}",
                    "https://blockchair.com/bitcoin/block/{HASH}",
                    "https://live.blockcypher.com/btc/block/{HASH}/",
                    "https://bitcoinexplorer.org/block/{HASH}",
                    "https://3xpl.com/bitcoin/block/{HASH}",
                ],
            ),
            (
                "btc:tx/{PIZZA}",
                &[
                    "https://blockstream.info/tx/{PIZZA}",
                    "https://mempool.emzy.de/tx/{PIZZA}",
                    "https://btcscan.org/tx/{PIZZA}",
                    "https://www.blockchain.com/explorer/transactions/btc/{PIZZA}?x=1",
                    "https://www.blockchain.com/btc/tx/{PIZZA}",
                    "https://blockchain.info/tx/{PIZZA}",
                    "https://www.oklink.com/btc/tx/{PIZZA}",
                    "https://blockchair.com/bitcoin/transaction/{PIZZA}",
                    "https://live.blockcypher.com/btc/tx/{PIZZA}/",
                    "https://bitcoinexplorer.org/tx/{PIZZA}#JSON",
                    "https://3xpl.com/bitcoin/transaction/{PIZZA}",
                ],
            ),
            (
                "btc:address/{GENESIS}",
                &[
                    "https://blockstream.info/address/{GENESIS}",
                    "https://mempool.emzy.de/address/{GENESIS}",
                    "https://btcscan.org/address/{GENESIS}",
                    "https://blockchain.com/explorer/addresses/btc/{GENESIS}",
                    "https://www.blockchain.com/btc/address/{GENESIS}",
                    "https://www.blockchain.info/address/{GENESIS}",
                    "https://oklink.com/bitcoin/address/{GENESIS}",
                    "https://blockchair.com/bitcoin/address/{GENESIS}",
                    "https://live.blockcypher.com/btc/address/{GENESIS}/",
                    "https://bitcoinexplorer.org/address/{GENESIS}",
                    "https://3xpl.com/bitcoin/address/{GENESIS}",
                ],
            ),
            // Test networks, where the explorer has them.
            (
                "btc-testnet:tx/{PIZZA}",
                &[
                    "https://blockstream.info/testnet/tx/{PIZZA}",
                    "https://blockchair.com/bitcoin/testnet/transaction/{PIZZA}",
                    "https://live.blockcypher.com/btc-testnet/tx/{PIZZA}/",
                ],
            ),
            (
                "btc-testnet:address/{TB}",
                &["https://blockstream.info/testnet/address/{TB}"],
            ),
            (
                "btc-testnet4:tx/{PIZZA}",
                &[
                    "https://blockstream.info/testnet4/tx/{PIZZA}",
                    "https://mempool.emzy.de/testnet4/tx/{PIZZA}",
                ],
            ),
            (
                "btc-signet:block/{HASH}",
                &[
                    "https://blockstream.info/signet/block/{HASH}",
                    "https://mempool.emzy.de/signet/block/{HASH}",
                ],
            ),
            // Lightning.
            (
                "ln:node/{ACINQ}",
                &[
                    "https://1ml.com/node/{ACINQ}",
                    "https://amboss.space/node/{ACINQ}",
                    "https://lightningnetwork.plus/nodes/{ACINQ}",
                ],
            ),
            (
                "ln:channel/892785849701564416",
                &[
                    "https://1ml.com/channel/892785849701564416",
                    "https://amboss.space/edge/892785849701564416",
                ],
            ),
        ];
        let m = Mempool;
        for (root, uris) in cases {
            let root = fill(root);
            for uri in uris.iter().map(|u| fill(u)) {
                assert_eq!(id(&uri).as_ref(), Some(&root), "{uri}");
                assert!(m.reads(&uri), "{uri}");
                assert_eq!(m.alias_root(&uri).as_ref(), Some(&root), "{uri}");
                // Read from mempool.space, whichever explorer the link is on.
                assert!(page(&from_uri(&uri).unwrap()).starts_with("https://mempool.space/"));
            }
        }
        // A block by height takes the height path on every explorer that has one: aliased only
        // once settled.
        for uri in [
            "https://blockstream.info/block-height/481824",
            "https://mempool.emzy.de/block/481824",
            "https://btcscan.org/block-height/481824",
            "https://www.blockchain.com/explorer/blocks/btc/481824",
            "https://www.blockchain.com/btc/block/481824",
            "https://blockchain.info/block/481824",
            "https://blockchain.info/block-height/481824",
            "https://www.oklink.com/bitcoin/block/481824",
            "https://blockchair.com/bitcoin/block/481824",
            "https://bitcoinexplorer.org/block-height/481824",
            "https://3xpl.com/bitcoin/block/481824",
        ] {
            let t = from_uri(uri).unwrap_or_else(|| panic!("{uri}"));
            assert_eq!(t.object, Object::Block(BlockRef::Height(481824)), "{uri}");
            assert_eq!(identifier(&t), None, "{uri}");
            assert_eq!(m.alias_root(uri), None, "{uri}");
            assert_eq!(
                request(Api::Mempool, &t).unwrap().url.as_str(),
                "https://mempool.space/api/block-height/481824"
            );
        }
    }

    /// Other chains on the same sites, networks an explorer doesn't show and malformed ids name
    /// nothing.
    #[test]
    fn other_chains_and_unshown_networks_name_nothing() {
        for uri in [
            // Other chains.
            "https://blockchair.com/bitcoin-cash/block/{HASH}",
            "https://blockchair.com/bitcoin-cash/transaction/{PIZZA}",
            "https://blockchair.com/ethereum/transaction/{PIZZA}",
            "https://www.blockchain.com/explorer/transactions/bch/{PIZZA}",
            "https://www.blockchain.com/explorer/transactions/eth/{PIZZA}",
            "https://www.blockchain.com/bch/tx/{PIZZA}",
            "https://blockstream.info/liquid/tx/{PIZZA}",
            "https://blockstream.info/liquidtestnet/block/{HASH}",
            "https://live.blockcypher.com/ltc/tx/{PIZZA}/",
            "https://live.blockcypher.com/doge/block/{HASH}/",
            "https://3xpl.com/litecoin/transaction/{PIZZA}",
            "https://www.oklink.com/eth/tx/{PIZZA}",
            // Networks an explorer doesn't show: mempool.emzy.de's testnet is down, BTCScan and
            // bitcoinexplorer.org are mainnet only, Blockchain.com's testnet pages name no
            // network, no Lightning object is on testnet4.
            "https://mempool.emzy.de/testnet/tx/{PIZZA}",
            "https://btcscan.org/testnet/tx/{PIZZA}",
            "https://btcscan.org/testnet4/tx/{PIZZA}",
            "https://bitcoinexplorer.org/testnet/tx/{PIZZA}",
            "https://www.blockchain.com/explorer/transactions/btc-testnet/{PIZZA}",
            "https://www.blockchain.com/explorer/addresses/btc-testnet/{TB}",
            "https://www.blockchain.com/explorer/transactions/btc-testnet4/{PIZZA}",
            "https://blockchair.com/bitcoin/testnet4/transaction/{PIZZA}",
            "https://1ml.com/testnet/node/{ACINQ}",
            // Malformed ids, and an id of another kind or network.
            "https://blockstream.info/tx/abc",
            "https://blockstream.info/block/1c8018d9cb3b742ef25114f27563e3fc4a1902167f9893",
            "https://blockstream.info/block/481824",
            "https://blockstream.info/block-height/abc",
            "https://blockstream.info/block-height/{HASH}",
            "https://www.blockchain.com/explorer/addresses/btc/1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNb",
            "https://blockchair.com/bitcoin/address/bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdx",
            "https://btcscan.org/address/{TB}",
            "https://blockstream.info/testnet/address/{GENESIS}",
            "https://1ml.com/node/04864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
            "https://amboss.space/edge/0",
            "https://lightningnetwork.plus/node/{ACINQ}",
            // Other pages, hosts and paths.
            "https://blockstream.info/tx/{PIZZA}/extra",
            "https://blockstream.info/de/tx/{PIZZA}",
            "https://btcscan.org.example.com/tx/{PIZZA}",
            "https://example.com/blockstream.info/tx/{PIZZA}",
            "https://blockstream.info:8443/tx/{PIZZA}",
            "https://www.blockchain.com/explorer/blocks/{HASH}",
            "https://www.oklink.com/bitcoin/{PIZZA}",
            "https://1ml.com/statistics",
        ]
        .map(fill)
        {
            assert_eq!(from_uri(&uri), None, "{uri}");
            assert!(!Mempool.reads(&uri), "{uri}");
        }
    }

    /// Every page the table writes, for every object kind on every network, reads back as the
    /// object; mempool.space's is the card's own page.
    #[test]
    fn every_page_reads_back_as_its_object() {
        let tb = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";
        let networks = [
            Network::Mainnet,
            Network::Testnet,
            Network::Testnet4,
            Network::Signet,
        ];
        let mut written = 0;
        for network in networks {
            let addr = if network == Network::Mainnet {
                GENESIS
            } else {
                tb
            };
            let mut objects = vec![
                Object::Block(BlockRef::Hash(HASH.into())),
                Object::Block(BlockRef::Height(481824)),
                Object::Tx(PIZZA.into()),
                Object::Address(addr.into()),
            ];
            if network != Network::Testnet4 {
                objects.push(Object::Node(ACINQ.into()));
                objects.push(Object::Channel(892785849701564416));
            }
            for object in objects {
                let t = Target { network, object };
                for explorer in EXPLORERS {
                    if let Some(uri) = explorer.page(&t) {
                        assert_eq!(from_uri(&uri).as_ref(), Some(&t), "{uri}");
                        written += 1;
                    }
                }
                assert_eq!(MEMPOOL_SPACE.page(&t).as_deref(), Some(page(&t).as_str()));
                if identifier(&t).is_some() {
                    for (_, uri) in links(&t) {
                        assert!(!uri.starts_with("https://mempool.space/"), "{uri}");
                        assert_eq!(id(&uri), identifier(&t), "{uri}");
                    }
                }
            }
        }
        assert!(written >= 90, "{written}");
        // Each site a card lists is listed once per object.
        let sites: Vec<_> = links(&from_root(&format!("btc:tx/{PIZZA}")).unwrap())
            .into_iter()
            .map(|(site, _)| site)
            .collect();
        assert_eq!(
            sites,
            [
                "Blockstream Explorer",
                "BTCScan",
                "Blockchain.com",
                "OKLink"
            ]
        );
        let sites: Vec<_> = links(&from_root("ln:channel/892785849701564416").unwrap())
            .into_iter()
            .map(|(site, _)| site)
            .collect();
        assert_eq!(sites, ["1ML", "Amboss"]);
        assert!(links(&from_root(&format!("ln-signet:node/{ACINQ}")).unwrap()).is_empty());
    }

    /// A transaction's record is read up to `TXS_CAP`, like an address's list: one with
    /// thousands of inputs passes the egress default.
    #[test]
    fn a_transaction_record_may_pass_the_default_cap() {
        let tx =
            from_root("btc:tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d")
                .unwrap();
        for api in [Api::Mempool, Api::Esplora] {
            let request = request(api, &tx).unwrap();
            assert_eq!(request.options().body_cap, Some(TXS_CAP), "{api:?}");
        }
    }

    #[test]
    fn block_by_height() {
        let t = from_uri("https://mempool.space/block/481824").unwrap();
        assert_eq!(t.object, Object::Block(BlockRef::Height(481824)));
        assert_eq!(identifier(&t), None);
        assert_eq!(
            request(Api::Mempool, &t).unwrap().url.as_str(),
            "https://mempool.space/api/block-height/481824"
        );
        assert_eq!(
            parse_height(200, &read("block-height-481824.txt"))
                .unwrap()
                .as_deref(),
            Some(HASH)
        );
        assert_eq!(
            parse_height(404, &read("block-height-missing.txt")).unwrap(),
            None
        );
        assert!(parse_height(200, b"<html>").is_err());
        assert_eq!(
            status_request(Api::Mempool, Network::Mainnet, HASH)
                .unwrap()
                .url
                .as_str(),
            format!("https://mempool.space/api/block/{HASH}/status")
        );
        let tip = parse_tip(200, &read("tip-height.txt")).unwrap();
        assert_eq!(tip, 972175);
        let status = read("block-481824-status.json");
        assert!(settled(200, &status, tip).unwrap());
        // The tip itself and the four blocks below it are not settled yet.
        assert!(!settled(200, &status, 481824 + 4).unwrap());
        assert!(settled(200, &status, 481824 + 5).unwrap());
        let stale = br#"{"in_best_chain":false,"height":481824}"#;
        assert!(!settled(200, stale, tip).unwrap());
    }

    #[test]
    fn block() {
        let card = parse_block(200, &read("block-481824.json"), Network::Mainnet, HASH)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "BitcoinBlock");
        assert_eq!(card.id, format!("btc:block/{HASH}"));
        assert_eq!(card.url, format!("https://mempool.space/block/{HASH}"));
        assert_eq!(card.name.as_deref(), Some("Block 481,824"));
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "2017-08-24T01:57:37+00:00"
        );
        assert_eq!(card.details["height"], 481824);
        assert_eq!(card.details["tx_count"], 1866);
        assert_eq!(card.details["pool"], "BTCC");
        assert_eq!(card.details["total_fees"], 212514269u64);
        assert_eq!(card.details["reward"], 1462514269u64);
        let links: Vec<_> = links(&from_root(&card.id).unwrap())
            .into_iter()
            .map(|(_, uri)| uri)
            .collect();
        assert_eq!(
            links,
            [
                format!("https://blockstream.info/block/{HASH}"),
                format!("https://btcscan.org/block/{HASH}"),
                format!("https://www.blockchain.com/explorer/blocks/btc/{HASH}"),
                format!("https://www.oklink.com/bitcoin/block/{HASH}"),
            ]
        );
        let other = "00000000b747d47c3b38161693ad05e26924b3775a8be669751f969da836311e";
        assert!(parse_block(200, &read("block-481824.json"), Network::Mainnet, other).is_err());
        assert!(parse_block(429, b"", Network::Mainnet, HASH).is_err());
    }

    /// The answers to a read of the example block by hash on one API (`record` its block
    /// record's path), from the files of `dir`.
    fn block_answers(
        api: &str,
        record: &str,
        dir: &str,
    ) -> Vec<(String, u16, &'static str, Vec<u8>)> {
        let file = |name: &str| read(&format!("{dir}{name}"));
        vec![
            (
                format!("{api}/{record}/{HASH}"),
                200,
                "application/json",
                file("block-481824.json"),
            ),
            (
                format!("{api}/block/{HASH}/status"),
                200,
                "application/json",
                file("block-481824-status.json"),
            ),
            (
                format!("{api}/blocks/tip/height"),
                200,
                "text/plain",
                read("tip-height-1111.txt"),
            ),
            (
                format!("{api}/block/{HASH}/txid/0"),
                200,
                "text/plain",
                file("block-481824-txid-0.txt"),
            ),
        ]
    }

    async fn resolve(
        uri: &str,
        answers: &[(String, u16, &'static str, Vec<u8>)],
    ) -> Result<ResourceMeta> {
        let routes = answers
            .iter()
            .map(|(url, status, kind, body)| (url.as_str(), *status, *kind, body.clone()))
            .collect();
        let (egress, _) = fixture::egress(routes);
        let cx = crate::sources::Context {
            egress: &egress,
            ttl: 3600,
        };
        Mempool.resolve(&cx, uri).await
    }

    /// A block whose status puts it outside the best chain carries `stale: true`; one in the best
    /// chain does not.
    #[tokio::test(start_paused = true)]
    async fn a_block_outside_the_best_chain_is_stale() {
        let api = "https://mempool.space/api";
        let answers = |status: &[u8]| {
            vec![
                (
                    format!("{api}/v1/block/{HASH}"),
                    200,
                    "application/json",
                    read("block-481824.json"),
                ),
                (
                    format!("{api}/block/{HASH}/status"),
                    200,
                    "application/json",
                    status.to_vec(),
                ),
            ]
        };
        let uri = format!("btc:block/{HASH}");
        let stale = br#"{"in_best_chain":false,"height":481824,"next_best":null}"#;
        let doc = resolve(&uri, &answers(stale)).await.unwrap();
        assert_eq!(doc.attributes["stale"], "true");
        let best = read("block-481824-status.json");
        let doc = resolve(&uri, &answers(&best)).await.unwrap();
        assert!(!doc.attributes.contains_key("stale"));
    }

    /// When mempool.space fails a testnet4 object, blockstream.info's testnet4 API reads it.
    #[tokio::test(start_paused = true)]
    async fn testnet4_falls_back_to_blockstream() {
        let txid = "5c5d91f2e7251501dccfe0e7ae684a865f499e77620e483760d9f70d1259e958";
        let answers = vec![
            (
                format!("https://mempool.space/testnet4/api/tx/{txid}"),
                503,
                "text/plain",
                b"Service Unavailable".to_vec(),
            ),
            (
                format!("https://blockstream.info/testnet4/api/tx/{txid}"),
                200,
                "application/json",
                read("esplora/testnet4-tx-coinbase-1000.json"),
            ),
        ];
        let doc = resolve(&format!("btc-testnet4:tx/{txid}"), &answers)
            .await
            .unwrap();
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.type_name, "BitcoinTransaction");
        assert_eq!(doc.attributes["coinbase"], "true");
    }

    /// A transaction's record and an address's list of transactions are parsed in a parse slot,
    /// never on the async runtime: each can be megabytes.
    #[tokio::test(start_paused = true)]
    async fn big_records_are_parsed_in_a_parse_slot() {
        use crate::resolver::html::counted;
        let api = "https://mempool.space/api";
        let bech = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
        let answers = vec![
            (
                format!("{api}/tx/{PIZZA}"),
                200,
                "application/json",
                read("tx-pizza.json.gz"),
            ),
            (
                format!("{api}/address/{bech}"),
                200,
                "application/json",
                read("address-bech32.json"),
            ),
            (
                format!("{api}/address/{bech}/txs"),
                200,
                "application/json",
                read("address-bech32-txs.json.gz"),
            ),
        ];
        for (uri, jobs) in [
            (format!("btc:tx/{PIZZA}"), 1),
            (format!("btc:address/{bech}"), 2),
        ] {
            let before = counted::jobs();
            let doc = resolve(&uri, &answers).await.unwrap();
            assert_eq!(doc.status, crate::model::Status::Ok, "{uri}");
            assert_eq!(counted::jobs() - before, jobs, "{uri}");
        }
    }

    /// A settled block read from mempool.space is complete: no early read again. Read from
    /// Esplora while mempool.space fails, it has no pool, fees or reward and is read again
    /// within the hour.
    #[tokio::test(start_paused = true)]
    async fn a_block_read_from_esplora_is_read_again_soon() {
        let root = format!("btc:block/{HASH}");
        let answers = block_answers("https://mempool.space/api", "v1/block", "");
        let doc = resolve(&root, &answers).await.unwrap();
        assert_eq!(doc.attributes["pool"], "BTCC");
        assert_eq!(doc.refresh_by, None);

        let mut answers = block_answers("https://blockstream.info/api", "block", "esplora/");
        answers.push((
            format!("https://mempool.space/api/v1/block/{HASH}"),
            503,
            "application/json",
            b"{}".to_vec(),
        ));
        let doc = resolve(&root, &answers).await.unwrap();
        assert_eq!(doc.status, crate::model::Status::Ok);
        assert_eq!(doc.attributes["height"], "481824");
        for fact in ["pool", "total_fees", "reward", "median_fee_rate"] {
            assert!(!doc.attributes.contains_key(fact), "{fact}");
        }
        let soon = doc.refresh_by.expect("read again soon") - chrono::Utc::now();
        assert!(soon <= chrono::Duration::hours(1) && soon > chrono::Duration::minutes(50));
    }

    #[test]
    fn testnet4_block() {
        let hash = parse_height(200, &read("testnet4-block-height-1000.txt"))
            .unwrap()
            .unwrap();
        let card = parse_block(
            200,
            &read("testnet4-block-1000.json"),
            Network::Testnet4,
            &hash,
        )
        .unwrap()
        .unwrap();
        assert_eq!(card.id, format!("btc-testnet4:block/{hash}"));
        assert_eq!(
            card.url,
            format!("https://mempool.space/testnet4/block/{hash}")
        );
        assert_eq!(card.details["pool"], "wiz");
        assert_eq!(
            links(&from_root(&card.id).unwrap()),
            [(
                "Blockstream Explorer",
                format!("https://blockstream.info/testnet4/block/{hash}")
            )]
        );
        assert_eq!(
            parse_height(200, &read("signet-block-height-1000.txt"))
                .unwrap()
                .unwrap()
                .len(),
            64
        );
    }

    #[test]
    fn transaction() {
        let card = parse_tx(200, &read("tx-pizza.json.gz"), Network::Mainnet, PIZZA)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "BitcoinTransaction");
        assert_eq!(card.id, format!("btc:tx/{PIZZA}"));
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "2010-05-22T18:16:31+00:00"
        );
        assert_eq!(card.details["block_height"], 57043);
        assert_eq!(card.details["fee"], 99000000);
        assert_eq!(card.details["inputs"], 131);
        assert_eq!(card.details["outputs"], 1);
        assert_eq!(card.details["output_value"], 1000000000000u64);
        assert_eq!(card.details["vsize"], 23620);
        assert_eq!(card.details["coinbase"], false);
        assert_eq!(links(&from_root(&card.id).unwrap()).len(), 4);
        assert_eq!(
            parse_tx(404, &read("tx-missing.txt"), Network::Mainnet, PIZZA).unwrap(),
            None
        );
    }

    /// Output values that pass `u64` together (a malformed record) give the largest value, as
    /// the other amount sums do.
    #[test]
    fn output_values_past_u64_saturate() {
        let body = format!(
            r#"{{"txid":"{PIZZA}","status":{{"confirmed":false}},"vin":[],"vout":[{{"value":{}}},{{"value":2}}]}}"#,
            u64::MAX - 1
        );
        let card = parse_tx(200, body.as_bytes(), Network::Mainnet, PIZZA)
            .unwrap()
            .unwrap();
        assert_eq!(card.details["outputs"], 2);
        assert_eq!(card.details["output_value"], u64::MAX);
    }

    #[test]
    fn addresses() {
        let card = parse_address(
            200,
            &read("address-genesis.json"),
            Network::Mainnet,
            GENESIS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(card.type_name, "BitcoinAddress");
        assert_eq!(card.name.as_deref(), Some(GENESIS));
        assert_eq!(card.details["tx_count"], 67019);
        assert_eq!(card.details["balance"], 5767070988u64);
        assert_eq!(
            links(&from_root(&card.id).unwrap())[3],
            (
                "Blockchain.com",
                format!("https://www.blockchain.com/explorer/addresses/btc/{GENESIS}")
            )
        );
        let bech = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
        let card = parse_address(200, &read("address-bech32.json"), Network::Mainnet, bech)
            .unwrap()
            .unwrap();
        assert_eq!(card.details["balance"], 17997974 - 14293);
        assert!(
            parse_address(200, &read("address-bech32.json"), Network::Mainnet, GENESIS).is_err()
        );
    }

    #[test]
    fn lightning_node() {
        let card = parse_node(200, &read("node-acinq.json"), Network::Mainnet, ACINQ)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "LightningNode");
        assert_eq!(card.id, format!("ln:node/{ACINQ}"));
        assert_eq!(card.name.as_deref(), Some("ACINQ"));
        assert_eq!(card.point, None, "IP geolocation is not a place");
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["first_seen"], "2019-01-02T18:13:39+00:00");
        assert_eq!(card.details["capacity"], 36108321892i64);
        assert_eq!(card.details["channels"], 1834);
        let on = links(&from_root(&card.id).unwrap());
        assert_eq!(on.len(), 3);
        assert_eq!(on[0], ("1ML", format!("https://1ml.com/node/{ACINQ}")));
        // An unknown key and a fault look the same; the search tells them apart.
        assert!(parse_node(500, &read("node-missing.json"), Network::Mainnet, ACINQ).is_err());
        let missing = "020000000000000000000000000000000000000000000000000000000000000001";
        assert_eq!(
            search_request(Network::Mainnet, missing)
                .unwrap()
                .url
                .as_str(),
            format!("https://mempool.space/api/v1/lightning/search?searchText={missing}")
        );
        assert!(node_listed(200, &read("search-acinq.json"), ACINQ).unwrap());
        assert!(!node_listed(200, &read("search-missing.json"), missing).unwrap());
        // A search that lists another node with a similar key is no match.
        assert!(!node_listed(200, &read("search-acinq.json"), missing).unwrap());
        assert!(node_listed(429, b"", ACINQ).is_err());
        assert_eq!(parse_node(404, b"", Network::Mainnet, ACINQ).unwrap(), None);
    }

    #[test]
    fn lightning_channel() {
        assert_eq!(channel_id("811984x2037x0"), Some(892785849701564416));
        assert_eq!(short_channel_id(892785849701564416), "811984x2037x0");
        let id = 892785849701564416;
        let card = parse_channel(
            200,
            &read("channel-892785849701564416.json"),
            Network::Mainnet,
            id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(card.type_name, "LightningChannel");
        assert_eq!(card.name.as_deref(), Some("Channel 811984x2037x0"));
        assert_eq!(
            card.start, None,
            "a date of a thing, not of an event, is a fact"
        );
        assert_eq!(card.details["created"], "2023-10-13T09:32:44+00:00");
        assert!(!card.details.contains_key("closed"), "still open");
        assert_eq!(card.end, None);
        assert_eq!(card.details["capacity"], 500000000);
        assert_eq!(card.details["status"], "active");
        assert_eq!(
            card.details["funding_tx"],
            "a7f10c2595c8ea0cc85c3183709d10aada60c7b87fe0c405f3f5b8a44150d4f5"
        );
        assert_eq!(card.details["node1_alias"], "ACINQ");
        assert_eq!(card.details["node2_alias"], "bfx-lnd0");
        assert_eq!(
            links(&from_root(&card.id).unwrap())[1],
            (
                "Amboss",
                "https://amboss.space/edge/892785849701564416".to_owned()
            )
        );
        assert_eq!(
            parse_channel(404, &read("channel-missing.html"), Network::Mainnet, id).unwrap(),
            None
        );
        assert!(parse_channel(
            200,
            &read("channel-892785849701564416.json"),
            Network::Mainnet,
            1
        )
        .is_err());
    }

    #[test]
    fn roots_and_aliases() {
        let m = Mempool;
        for uri in [
            format!("btc:tx/{PIZZA}"),
            format!("btc:block/{HASH}"),
            format!("btc:address/{GENESIS}"),
            format!("btc-testnet4:tx/{PIZZA}"),
            format!("ln:node/{ACINQ}"),
            "ln:channel/892785849701564416".into(),
            format!("ln-signet:node/{ACINQ}"),
        ] {
            assert!(m.reads(&uri), "{uri}");
            assert_eq!(m.alias_root(&uri), None, "{uri}");
            assert!(!m.invalid(&uri), "{uri}");
            assert_eq!(
                target(&uri).as_ref().and_then(identifier),
                Some(uri.clone())
            );
        }
        assert_eq!(
            m.alias_root(&format!("https://mempool.space/de/tx/{PIZZA}")),
            Some(format!("btc:tx/{PIZZA}"))
        );
        assert_eq!(
            m.alias_root("https://mempool.space/lightning/channel/811984x2037x0")
                .as_deref(),
            Some("ln:channel/892785849701564416")
        );
        // A height names a block only after the fetch, once it is settled.
        assert!(m.reads("https://mempool.space/block/481824"));
        assert_eq!(m.alias_root("https://mempool.space/block/481824"), None);
        for uri in [
            "btc:tx/abc",
            "btc:tx/A1075DB55D416D3CA199F55B6084E2115B9345E16C5CF302FC80E9D5FBF5D48D",
            "btc:block/481824",
            "ln:channel/811984x2037x0",
            "ln-testnet4:node/03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
            "btc:wallet/x",
        ] {
            assert!(m.invalid(uri), "{uri}");
            assert!(!m.reads(uri), "{uri}");
        }
        assert!(!m.invalid("https://example.org/btc:tx/abc"));
        for example in m.examples() {
            assert!(m.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        let m = Mempool;
        let cards = [
            parse_block(200, &read("block-481824.json"), Network::Mainnet, HASH),
            parse_tx(200, &read("tx-pizza.json.gz"), Network::Mainnet, PIZZA),
            parse_address(
                200,
                &read("address-genesis.json"),
                Network::Mainnet,
                GENESIS,
            ),
            parse_node(200, &read("node-acinq.json"), Network::Mainnet, ACINQ),
            parse_channel(
                200,
                &read("channel-892785849701564416.json"),
                Network::Mainnet,
                892785849701564416,
            ),
        ];
        for card in cards {
            let card = card.unwrap().unwrap();
            let doc = crate::sources::kit::checked(&m, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
    }

    /// Every entry of a card's `related` list with a `uri` names a root that reads back as itself
    /// (`POST /v1/resolve` reads it), on the card's own network.
    fn roots_read_back(card: &Card, prefixes: &[&str]) {
        for r in &card.related {
            let Some(uri) = &r.uri else { continue };
            assert!(Mempool.reads(uri) && !Mempool.invalid(uri), "{uri}");
            assert_eq!(
                from_root(uri).as_ref().and_then(identifier).as_ref(),
                Some(uri),
                "{uri}"
            );
            assert!(prefixes.iter().any(|p| uri.starts_with(p)), "{uri}");
        }
    }
    fn rels(card: &Card) -> Vec<&str> {
        card.related.iter().map(|r| r.rel.as_str()).collect()
    }

    /// A transaction with many inputs lists its block, the 10 largest inputs and its output, and
    /// counts the 131 inputs; the whole list fits the card.
    #[test]
    fn transaction_related() {
        let card = parse_tx(200, &read("tx-pizza.json.gz"), Network::Mainnet, PIZZA)
            .unwrap()
            .unwrap();
        let block = &card.related[0];
        assert_eq!(block.rel, "block");
        assert_eq!(
            block.uri.as_deref(),
            Some("btc:block/00000000152340ca42227603908689183edc47355204e7aca59383b0aaac1fd8")
        );
        assert_eq!(block.name.as_deref(), Some("Block 57,043"));
        assert_eq!(block.at.unwrap().to_rfc3339(), "2010-05-22T18:16:31+00:00");
        let inputs: Vec<_> = card.related.iter().filter(|r| r.rel == "input").collect();
        assert_eq!(inputs.len(), RELATED_EACH);
        assert_eq!(card.related_total["input"], 131);
        assert_eq!(
            inputs[0].uri.as_deref(),
            Some("btc:address/1XPTgDRhN8RFnzniWCddobD9iKZatrvH4")
        );
        assert_eq!(inputs[0].amount_sat, Some(375388000000));
        assert_eq!(inputs[9].amount_sat, Some(25000000000));
        assert!(inputs
            .windows(2)
            .all(|w| w[0].amount_sat >= w[1].amount_sat));
        let output = card.related.last().unwrap();
        assert_eq!(output.rel, "output");
        assert_eq!(
            output.uri.as_deref(),
            Some("btc:address/17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ")
        );
        assert_eq!(output.amount_sat, Some(1000000000000));
        assert!(
            !card.related_total.contains_key("output"),
            "one output, all listed"
        );
        assert_eq!(card.related.len(), 1 + RELATED_EACH + 1);
        roots_read_back(&card, &["btc:"]);
        let doc = crate::sources::kit::checked(&Mempool, &card);
        assert_eq!(doc.related, card.related);
        assert_eq!(doc.related_total["input"], 131);
        // The Esplora fallback's record gives the same list.
        let esplora = parse_tx(
            200,
            &fixture::read("mempool", "esplora/tx-pizza.json.gz"),
            Network::Mainnet,
            PIZZA,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (&esplora.related, &esplora.related_total),
            (&card.related, &card.related_total)
        );
    }

    /// A coinbase transaction's input has no address (`coinbase`), and its OP_RETURN output none
    /// either: both are listed without a `uri`, labelled.
    #[test]
    fn coinbase_and_op_return() {
        let txid = "da917699942e4a96272401b534381a75512eeebe8403084500bd637bd47168b3";
        let card = parse_tx(
            200,
            &read("tx-coinbase-481824.json"),
            Network::Mainnet,
            txid,
        )
        .unwrap()
        .unwrap();
        assert_eq!(rels(&card), ["block", "input", "output", "output"]);
        assert_eq!(
            card.related[0].uri.as_deref(),
            Some(format!("btc:block/{HASH}").as_str())
        );
        let input = &card.related[1];
        assert_eq!(
            (
                input.uri.as_deref(),
                input.name.as_deref(),
                input.amount_sat
            ),
            (None, Some("coinbase"), None)
        );
        let (paid, op_return) = (&card.related[2], &card.related[3]);
        assert_eq!(
            paid.uri.as_deref(),
            Some("btc:address/152f1muMCNa7goXYhYAQC61hxEgGacmncB")
        );
        assert_eq!(paid.amount_sat, Some(1462514269));
        assert_eq!(
            (
                op_return.uri.as_deref(),
                op_return.name.as_deref(),
                op_return.amount_sat
            ),
            (None, Some("OP_RETURN"), Some(0))
        );
        assert!(card.related_total.is_empty());
        crate::sources::kit::checked(&Mempool, &card);
    }

    /// A block lists its previous block from its record (none on the genesis block), its next
    /// block from its status once that is settled, and its coinbase transaction.
    #[test]
    fn block_neighbours() {
        let block1 = "00000000839a8e6886ab5951d76f411475428afc90947ee320161bbf18eb6048";
        let card = parse_block(200, &read("block-1.json"), Network::Mainnet, block1)
            .unwrap()
            .unwrap();
        assert_eq!(rels(&card), ["previous"]);
        assert_eq!(
            card.related[0].uri.as_deref(),
            Some("btc:block/000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f")
        );
        assert_eq!(card.related[0].name.as_deref(), Some("Block 0"));
        // The genesis block has none.
        let mut genesis: Value = serde_json::from_slice(&read("block-1.json")).unwrap();
        genesis["previousblockhash"] = Value::Null;
        assert!(block_card(&genesis, Network::Mainnet, block1)
            .unwrap()
            .related
            .is_empty());

        let tip = parse_tip(200, &read("tip-height-1111.txt")).unwrap();
        assert_eq!(tip, 975364);
        let Place { best, next } = parse_status(200, &read("block-1-status.json"))
            .unwrap()
            .unwrap();
        assert!(best);
        let (next, height) = next.unwrap();
        assert_eq!(
            (next.as_str(), height),
            (
                "000000006a625f06636b8bb6ac7b960a8d03705d1ace08b1a19da3fdcc99ddbd",
                2
            )
        );
        assert!(deep(height, tip));
        assert_eq!(
            parse_txid(200, &read("block-1-txid-0.txt")).unwrap(),
            "0e3e2357e806b6cdb1f70b54c3a3a17b6714ee1f0e68bebb44a74b1efd512098"
        );

        // A recent block: its next block is settled at the recorded tip, and would not be four
        // blocks after it.
        let recent = "0000000000000000000008dfb288144ec4626cad43656ff2cdd8cb278886fe80";
        let card = parse_block(200, &read("block-975251.json"), Network::Mainnet, recent)
            .unwrap()
            .unwrap();
        assert_eq!(card.related[0].name.as_deref(), Some("Block 975,250"));
        assert_eq!(card.details["pool"], "Braiins Pool");
        let Place { next, .. } = parse_status(200, &read("block-975251-status.json"))
            .unwrap()
            .unwrap();
        let (next, height) = next.unwrap();
        assert_eq!(
            next,
            "00000000000000000001066efcc7a56714f7d79f27a2dfd71cef047ff25dfefb"
        );
        assert_eq!(height, 975252);
        assert!(deep(height, tip));
        assert!(!deep(height, height + 4));
        assert!(deep(height, height + 5));
        assert_eq!(
            parse_txid(200, &read("block-975251-txid-0.txt")).unwrap(),
            "c2ffe82fbb08c0f6a1e62272fd80b22f36a3fcc166778969d3764617a3de2fbf"
        );
        // A stale block has no next block; the tip has none yet; an unknown block has no status.
        let stale = br#"{"in_best_chain":false,"height":481824,"next_best":null}"#;
        assert_eq!(
            parse_status(200, stale).unwrap(),
            Some(Place {
                best: false,
                next: None
            })
        );
        let tip_block = br#"{"in_best_chain":true,"height":975364,"next_best":null}"#;
        assert_eq!(
            parse_status(200, tip_block).unwrap(),
            Some(Place {
                best: true,
                next: None
            })
        );
        assert_eq!(parse_status(404, b"Block not found").unwrap(), None);
        // A next block that is no block hash, or without a height, is no answer.
        assert!(parse_status(
            200,
            br#"{"in_best_chain":true,"height":1,"next_best":"zz"}"#
        )
        .is_err());
        assert!(parse_status(
            200,
            br#"{"in_best_chain":true,"next_best":"000000006a625f06636b8bb6ac7b960a8d03705d1ace08b1a19da3fdcc99ddbd"}"#
        )
        .is_err());
        assert!(parse_txid(200, b"<html>").is_err());
        assert_eq!(
            coinbase_request(Api::Mempool, Network::Mainnet, HASH)
                .unwrap()
                .url
                .as_str(),
            format!("https://mempool.space/api/block/{HASH}/txid/0")
        );
    }

    /// An address lists its latest 10 transactions, each with what it moved into or out of the
    /// address, and counts them all; a transaction that spends from it is negative.
    #[test]
    fn address_transactions() {
        let addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
        assert_eq!(
            txs_request(Api::Mempool, Network::Mainnet, addr)
                .unwrap()
                .url
                .as_str(),
            format!("https://mempool.space/api/address/{addr}/txs")
        );
        assert_eq!(
            txs_request(Api::Mempool, Network::Mainnet, addr)
                .unwrap()
                .options()
                .body_cap,
            Some(4 * 1024 * 1024)
        );
        let txs = parse_address_txs(
            200,
            &read("address-bech32-txs.json.gz"),
            Network::Mainnet,
            addr,
        )
        .unwrap();
        assert_eq!(txs.len(), RELATED_EACH);
        assert!(txs.iter().all(|r| r.rel == "tx"));
        assert_eq!(
            txs[0].uri.as_deref(),
            Some("btc:tx/d54945326574a95dab44fafcb1c3b04d239f2de106a8b1aa9313208ee2cfa80a")
        );
        assert_eq!(txs[0].amount_sat, Some(100000));
        assert_eq!(txs[0].at.unwrap().timestamp(), 1794217667);
        assert_eq!(
            txs[9].uri.as_deref(),
            Some("btc:tx/1d51c696eab35d7dad7631516cc0845aaab317fe67cb182782752547541d30ea")
        );
        let mut card = parse_address(
            200,
            &read("address-bech32-1111.json"),
            Network::Mainnet,
            addr,
        )
        .unwrap()
        .unwrap();
        relate_first(&mut card, "tx", txs, 125);
        assert_eq!(card.related_total["tx"], 125);
        roots_read_back(&card, &["btc:tx/"]);
        crate::sources::kit::checked(&Mempool, &card);
        // The pizza transaction spent everything its one input address held, and paid one other.
        let pizza = format!("[{}]", String::from_utf8(read("tx-pizza.json.gz")).unwrap());
        let spent = parse_address_txs(
            200,
            pizza.as_bytes(),
            Network::Mainnet,
            "1XPTgDRhN8RFnzniWCddobD9iKZatrvH4",
        )
        .unwrap();
        assert_eq!(spent[0].amount_sat, Some(-1000099000000));
        let paid = parse_address_txs(
            200,
            pizza.as_bytes(),
            Network::Mainnet,
            "17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ",
        )
        .unwrap();
        assert_eq!(paid[0].amount_sat, Some(1000000000000));
        assert!(parse_address_txs(200, b"{}", Network::Mainnet, addr).is_err());
    }

    /// A node lists its 10 biggest channels with their peers' aliases, and each peer once; a
    /// channel lists its two nodes and its funding transaction.
    #[test]
    fn lightning_related() {
        assert_eq!(
            channels_request(Network::Mainnet, ACINQ)
                .unwrap()
                .url
                .as_str(),
            format!(
                "https://mempool.space/api/v1/lightning/channels?public_key={ACINQ}&status=open"
            )
        );
        let (channels, peers) =
            parse_channels(200, &read("channels-acinq.json"), Network::Mainnet, ACINQ).unwrap();
        assert_eq!(channels.len(), RELATED_EACH);
        assert_eq!(
            channels[0].uri.as_deref(),
            Some("ln:channel/892785849701564416")
        );
        assert_eq!(channels[0].name.as_deref(), Some("bfx-lnd0"));
        assert_eq!(channels[0].amount_sat, Some(500000000));
        let peers: Vec<_> = peers
            .iter()
            .map(|p| (p.uri.as_deref().unwrap(), p.name.as_deref().unwrap()))
            .collect();
        assert_eq!(
            peers,
            [
                (
                    "ln:node/033d8656219478701227199cbd6f670335c8d408a92ae88b962c49d4dc0e83e025",
                    "bfx-lnd0"
                ),
                (
                    "ln:node/037f990e61acee8a7697966afd29dd88f3b1f8a7b14d625c4f8742bd952003a590",
                    "fixedfloat.com"
                ),
                (
                    "ln:node/0294ac3e099def03c12a37e30fe5364b1223fd60069869142ef96580c8439c2e0a",
                    "okx"
                ),
            ]
        );
        let mut node = parse_node(200, &read("node-acinq.json"), Network::Mainnet, ACINQ)
            .unwrap()
            .unwrap();
        let (channels, peers) =
            parse_channels(200, &read("channels-acinq.json"), Network::Mainnet, ACINQ).unwrap();
        relate_first(&mut node, "channel", channels, 1834);
        relate_first(&mut node, "peer", peers, 0);
        assert_eq!(node.related.len(), RELATED_EACH + 3);
        assert_eq!(
            node.related_total,
            std::collections::BTreeMap::from([("channel".to_owned(), 1834)])
        );
        roots_read_back(&node, &["ln:"]);
        crate::sources::kit::checked(&Mempool, &node);

        let channel = parse_channel(
            200,
            &read("channel-892785849701564416.json"),
            Network::Mainnet,
            892785849701564416,
        )
        .unwrap()
        .unwrap();
        assert_eq!(rels(&channel), ["node", "node", "funding_tx"]);
        assert_eq!(
            channel.related[0].uri.as_deref(),
            Some(format!("ln:node/{ACINQ}").as_str())
        );
        assert_eq!(channel.related[0].name.as_deref(), Some("ACINQ"));
        assert_eq!(channel.related[1].name.as_deref(), Some("bfx-lnd0"));
        assert_eq!(
            channel.related[2].uri.as_deref(),
            Some("btc:tx/a7f10c2595c8ea0cc85c3183709d10aada60c7b87fe0c405f3f5b8a44150d4f5")
        );
        assert_eq!(channel.related[2].amount_sat, Some(500000000));
        roots_read_back(&channel, &["ln:", "btc:tx/"]);
        crate::sources::kit::checked(&Mempool, &channel);
    }

    /// Roots stay on the card's network: a testnet4 transaction names testnet4 blocks and
    /// addresses, a signet channel signet nodes and transactions; an address of another network
    /// names nothing.
    #[test]
    fn related_roots_keep_their_network() {
        let txid = "5c5d91f2e7251501dccfe0e7ae684a865f499e77620e483760d9f70d1259e958";
        let raw = read("testnet4-tx-coinbase-1000.json");
        let card = parse_tx(200, &raw, Network::Testnet4, txid)
            .unwrap()
            .unwrap();
        assert_eq!(rels(&card), ["block", "input", "output", "output"]);
        assert_eq!(
            card.related[0].uri.as_deref(),
            Some(
                "btc-testnet4:block/00000000b747d47c3b38161693ad05e26924b3775a8be669751f969da836311e"
            )
        );
        assert_eq!(
            card.related[2].uri.as_deref(),
            Some("btc-testnet4:address/tb1q548z58kqvwyjqwy8vc2ntmg33d7s2wyfv7ukq4")
        );
        roots_read_back(&card, &["btc-testnet4:"]);
        assert_eq!(
            parse_txid(200, &read("testnet4-block-1000-txid-0.txt")).unwrap(),
            txid
        );
        assert!(parse_status(200, &read("testnet4-block-1000-status.json"))
            .unwrap()
            .unwrap()
            .next
            .is_some());
        // The same record read as mainnet: its testnet address is no mainnet address.
        let mainnet = parse_tx(200, &raw, Network::Mainnet, txid)
            .unwrap()
            .unwrap();
        assert_eq!(mainnet.related[2].uri, None);
        assert_eq!(mainnet.related[2].name.as_deref(), Some("v0_p2wpkh"));
        assert!(mainnet.related[0]
            .uri
            .as_deref()
            .unwrap()
            .starts_with("btc:block/"));

        let signet = parse_channel(
            200,
            &read("channel-892785849701564416.json"),
            Network::Signet,
            892785849701564416,
        )
        .unwrap()
        .unwrap();
        roots_read_back(&signet, &["ln-signet:node/", "btc-signet:tx/"]);
        let (channels, peers) =
            parse_channels(200, &read("channels-acinq.json"), Network::Signet, ACINQ).unwrap();
        assert!(channels.iter().chain(&peers).all(|r| r
            .uri
            .as_deref()
            .unwrap()
            .starts_with("ln-signet:")));
    }

    #[test]
    fn heights_are_grouped() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(481824), "481,824");
    }

    /// The Esplora fallback: its requests, and the same cards from its answers.
    mod esplora {
        use super::super::{parse, parse_height, request, Api, BlockRef, Network, Object, Target};
        use crate::sources::kit::fixture;

        const HASH: &str = "0000000000000000001c8018d9cb3b742ef25114f27563e3fc4a1902167f9893";
        const PIZZA: &str = "a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d";
        const GENESIS: &str = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";

        fn read(name: &str) -> Vec<u8> {
            fixture::read("mempool", &format!("esplora/{name}"))
        }
        fn on(network: Network, object: Object) -> Target {
            Target { network, object }
        }

        #[test]
        fn requests() {
            let t = |uri: &str| super::super::from_uri(uri).unwrap();
            let cases = [
                (
                    format!("https://mempool.space/tx/{PIZZA}"),
                    Some(format!("https://blockstream.info/api/tx/{PIZZA}")),
                ),
                (
                    format!("https://mempool.space/signet/block/{HASH}"),
                    Some(format!("https://blockstream.info/signet/api/block/{HASH}")),
                ),
                (
                    "https://mempool.space/testnet/block/1000".into(),
                    Some("https://blockstream.info/testnet/api/block-height/1000".into()),
                ),
                (
                    format!("https://mempool.space/testnet4/tx/{PIZZA}"),
                    Some(format!("https://blockstream.info/testnet4/api/tx/{PIZZA}")),
                ),
                (
                    "https://mempool.space/lightning/channel/892785849701564416".into(),
                    None,
                ),
            ];
            for (uri, expected) in cases {
                assert_eq!(
                    request(Api::Esplora, &t(&uri)).map(|r| r.url.to_string()),
                    expected,
                    "{uri}"
                );
            }
            let height = request(Api::Esplora, &t("https://mempool.space/block/1")).unwrap();
            assert_eq!(height.extra_types, ["text/plain"]);
        }

        #[test]
        fn same_cards_as_mempool() {
            assert_eq!(
                parse_height(200, &read("block-height-481824.txt"))
                    .unwrap()
                    .as_deref(),
                Some(HASH)
            );
            let block = on(Network::Mainnet, Object::Block(BlockRef::Hash(HASH.into())));
            let card = parse(200, &read("block-481824.json"), &block)
                .unwrap()
                .unwrap();
            let from_mempool = parse(200, &fixture::read("mempool", "block-481824.json"), &block)
                .unwrap()
                .unwrap();
            assert_eq!(
                (&card.id, &card.url, &card.name, card.start),
                (
                    &from_mempool.id,
                    &from_mempool.url,
                    &from_mempool.name,
                    from_mempool.start
                )
            );
            assert!(!card.details.contains_key("pool"));
            assert_eq!(card.details["tx_count"], 1866);

            let tx = on(Network::Mainnet, Object::Tx(PIZZA.into()));
            let card = parse(200, &read("tx-pizza.json.gz"), &tx).unwrap().unwrap();
            assert_eq!(card.id, format!("btc:tx/{PIZZA}"));
            assert_eq!(card.details["fee"], 99000000);
            assert_eq!(parse(404, &read("tx-missing.txt"), &tx).unwrap(), None);

            let addr = on(Network::Mainnet, Object::Address(GENESIS.into()));
            let card = parse(200, &read("address-genesis.json"), &addr)
                .unwrap()
                .unwrap();
            assert_eq!(card.id, format!("btc:address/{GENESIS}"));
            assert_eq!(card.url, format!("https://mempool.space/address/{GENESIS}"));
        }

        /// The fallback reads a block's next block and coinbase and an address's transactions
        /// from the same routes on Esplora, with the same answers.
        #[test]
        fn related_from_esplora() {
            use super::super::{
                coinbase_request, parse_address_txs, parse_status, parse_txid, status_request,
                txs_request,
            };
            assert_eq!(
                coinbase_request(Api::Esplora, Network::Mainnet, HASH)
                    .unwrap()
                    .url
                    .as_str(),
                format!("https://blockstream.info/api/block/{HASH}/txid/0")
            );
            assert_eq!(
                status_request(Api::Esplora, Network::Signet, HASH)
                    .unwrap()
                    .url
                    .as_str(),
                format!("https://blockstream.info/signet/api/block/{HASH}/status")
            );
            assert_eq!(
                coinbase_request(Api::Esplora, Network::Testnet4, HASH)
                    .unwrap()
                    .url
                    .as_str(),
                format!("https://blockstream.info/testnet4/api/block/{HASH}/txid/0")
            );
            assert_eq!(
                parse_txid(200, &read("block-481824-txid-0.txt")).unwrap(),
                "da917699942e4a96272401b534381a75512eeebe8403084500bd637bd47168b3"
            );
            assert_eq!(
                parse_status(200, &read("block-481824-status.json")).unwrap(),
                parse_status(200, &fixture::read("mempool", "block-481824-status.json")).unwrap()
            );
            let addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
            assert_eq!(
                txs_request(Api::Esplora, Network::Mainnet, addr)
                    .unwrap()
                    .url
                    .as_str(),
                format!("https://blockstream.info/api/address/{addr}/txs")
            );
            let esplora = parse_address_txs(
                200,
                &read("address-bech32-txs.json.gz"),
                Network::Mainnet,
                addr,
            )
            .unwrap();
            let mempool = parse_address_txs(
                200,
                &fixture::read("mempool", "address-bech32-txs.json.gz"),
                Network::Mainnet,
                addr,
            )
            .unwrap();
            assert_eq!(esplora, mempool);
        }
    }
}
