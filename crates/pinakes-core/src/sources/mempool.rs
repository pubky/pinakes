//! Bitcoin blocks, transactions and addresses, and Lightning nodes and channels, read from the
//! mempool.space API by the id in the link.
//!
//! Every mempool.space link of one object, with or without a language prefix, is an alias of one
//! identifier: `btc:block/{hash}`, `btc:tx/{txid}`, `btc:address/{address}`, `ln:node/{key}` and
//! `ln:channel/{decimal id}`. Test networks are separate objects with their own prefixes
//! (`btc-testnet4:tx/…`, `ln-signet:node/…`). A block link by height is an alias of the block's
//! hash once the block is buried six deep in the best chain (`settled`).
//!
//! No schema.org type fits these objects, so the cards use local types.
use super::{
    kit::{
        body, fetch, float, host_and_segments, int, json, plain, text, time, unix, Card, Link,
        Request,
    },
    Budget, Example, Source,
};
use crate::model::ResourceMeta;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SITE: &str = "mempool.space";
const VIA: &str = "mempool";
/// How deep a block must be before its height names it for good.
pub const SETTLED_DEPTH: u64 = 6;

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
    fn from_segment(segment: &str) -> Option<Self> {
        match segment {
            "testnet" => Some(Self::Testnet),
            "testnet4" => Some(Self::Testnet4),
            "signet" => Some(Self::Signet),
            _ => None,
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
    let number = |s: &str, max: u64| {
        plain(s, 8, |c| c.is_ascii_digit())
            .then(|| s.parse::<u64>().ok())
            .flatten()
            .filter(|n| *n <= max)
    };
    let (block, tx, output) = (
        number(block, 0xFF_FFFF)?,
        number(tx, 0xFF_FFFF)?,
        number(output, 0xFFFF)?,
    );
    Some(block << 40 | tx << 16 | output)
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
    (bech32_polymod(&values) == expected).then_some(value)
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

fn object(kind: &str, id: &str, network: Network) -> Option<Object> {
    Some(match kind {
        "block" => match hash(id) {
            Some(h) => Object::Block(BlockRef::Hash(h)),
            None => Object::Block(BlockRef::Height(
                plain(id, 8, |c| c.is_ascii_digit())
                    .then(|| id.parse().ok())
                    .flatten()?,
            )),
        },
        "tx" => Object::Tx(hash(id)?),
        "address" => Object::Address(address(id, network)?),
        _ => return None,
    })
}

/// A language prefix of the mempool.space site, e.g. `de` or `zh`.
fn language(segment: &str) -> bool {
    segment.len() == 2 && segment.bytes().all(|b| b.is_ascii_lowercase()) && segment != "tx"
}

/// What a mempool.space link names: site pages with an optional language and network prefix,
/// and the API's own URLs. A `bitcoin:` URI names a mainnet address.
pub fn from_uri(uri: &str) -> Option<Target> {
    if let Some(rest) = uri
        .strip_prefix("bitcoin:")
        .or_else(|| uri.strip_prefix("BITCOIN:"))
    {
        let addr = rest.split(['?', '#']).next()?;
        return Some(Target {
            network: Network::Mainnet,
            object: Object::Address(address(addr, Network::Mainnet)?),
        });
    }
    let (host, segments) = host_and_segments(uri)?;
    if host != "mempool.space" {
        return None;
    }
    let mut s: &[String] = &segments;
    if s.first().is_some_and(|l| language(l)) {
        s = &s[1..];
    }
    let mut network = Network::Mainnet;
    if let Some(n) = s.first().and_then(|n| Network::from_segment(n)) {
        network = n;
        s = &s[1..];
    }
    let s: Vec<&str> = s.iter().map(String::as_str).collect();
    let object = match s.as_slice() {
        ["lightning", "node", key] => Object::Node(node_key(key)?),
        ["lightning", "channel", id] => Object::Channel(channel_id(id)?),
        ["api", "v1", "lightning", "nodes", key] => Object::Node(node_key(key)?),
        ["api", "v1", "lightning", "channels", id] => Object::Channel(channel_id(id)?),
        ["api", "v1", "block", id] | ["api", "block", id] => {
            Object::Block(BlockRef::Hash(hash(id)?))
        }
        ["api", "block-height", h] => object("block", h, network)?,
        ["api", kind @ ("tx" | "address"), id] => object(kind, id, network)?,
        [kind @ ("block" | "tx" | "address"), id] => object(kind, id, network)?,
        _ => return None,
    };
    if matches!(object, Object::Node(_) | Object::Channel(_)) && network == Network::Testnet4 {
        return None;
    }
    Some(Target { network, object })
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
    let net = target.network.path();
    match &target.object {
        Object::Block(BlockRef::Hash(h)) => format!("https://mempool.space{net}/block/{h}"),
        Object::Block(BlockRef::Height(h)) => format!("https://mempool.space{net}/block/{h}"),
        Object::Tx(id) => format!("https://mempool.space{net}/tx/{id}"),
        Object::Address(a) => format!("https://mempool.space{net}/address/{a}"),
        Object::Node(k) => format!("https://mempool.space{net}/lightning/node/{k}"),
        Object::Channel(id) => format!("https://mempool.space{net}/lightning/channel/{id}"),
    }
}

/// Where a request goes: mempool.space, or the Esplora API at blockstream.info, the fallback
/// for on-chain objects (`super::esplora`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Mempool,
    Esplora,
}

fn api(api: Api, network: Network, path: &str) -> Option<Request> {
    let base = match api {
        Api::Mempool => format!("https://mempool.space{}/api", network.path()),
        Api::Esplora => super::esplora::base(network)?.to_owned(),
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
        (Object::Tx(id), _) => api(to, n, &format!("tx/{id}")),
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
    Ok(v["in_best_chain"].as_bool() == Some(true) && tip + 1 >= height + SETTLED_DEPTH)
}

/// Explorers that show the same on-chain object (each checked through its own Esplora API).
fn chain_mirrors(card: &mut Card, network: Network, kind: &str, id: &str) {
    let blockstream = match network {
        Network::Mainnet => Some(""),
        Network::Testnet => Some("/testnet"),
        Network::Signet => Some("/signet"),
        Network::Testnet4 => None,
    };
    if let Some(net) = blockstream {
        card.link(Link::new(
            "Blockstream Explorer",
            format!("https://blockstream.info{net}/{kind}/{id}"),
            VIA,
        ));
    }
    if network != Network::Mainnet {
        return;
    }
    card.link(Link::new(
        "mempool.emzy.de",
        format!("https://mempool.emzy.de/{kind}/{id}"),
        VIA,
    ));
    card.link(Link::new(
        "BTCScan",
        format!("https://btcscan.org/{kind}/{id}"),
        VIA,
    ));
    let plural = match kind {
        "block" => "blocks",
        "tx" => "transactions",
        _ => "addresses",
    };
    card.link(Link::new(
        "Blockchain.com",
        format!("https://www.blockchain.com/explorer/{plural}/btc/{id}"),
        VIA,
    ));
}

fn on(network: Network, object: Object) -> Target {
    Target { network, object }
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
    let extras = &v["extras"];
    card.detail("pool", text(&extras["pool"]["name"]));
    card.detail("total_fees", extras["totalFees"].as_u64());
    card.detail("reward", extras["reward"].as_u64());
    card.detail("median_fee_rate", float(&extras["medianFee"]));
    chain_mirrors(&mut card, network, "block", hash);
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
        vout.map(|o| o.iter().filter_map(|x| x["value"].as_u64()).sum::<u64>()),
    );
    card.detail(
        "coinbase",
        vin.and_then(|i| i.first())
            .and_then(|i| i["is_coinbase"].as_bool()),
    );
    chain_mirrors(&mut card, network, "tx", txid);
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
    chain_mirrors(&mut card, network, "address", addr);
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
    card.start = unix(&v["first_seen"]);
    card.detail("public_key", key);
    card.detail("alias", text(&v["alias"]));
    card.detail("color", text(&v["color"]));
    card.detail("capacity", int(&v["capacity"]));
    card.detail("channels", v["active_channel_count"].as_u64());
    card.detail("updated_at", unix(&v["updated_at"]).map(|t| t.to_rfc3339()));
    if network == Network::Mainnet {
        for (site, uri) in [
            ("1ML", format!("https://1ml.com/node/{key}")),
            ("Amboss", format!("https://amboss.space/node/{key}")),
            (
                "Lightning Network+",
                format!("https://lightningnetwork.plus/nodes/{key}"),
            ),
        ] {
            card.link(Link::new(site, uri, VIA));
        }
    }
    Ok(Some(card))
}

/// A Lightning channel, or `None` when the graph has no such channel.
pub fn parse_channel(status: u16, raw: &[u8], network: Network, id: u64) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    ensure!(
        int(&v["id"]) == i64::try_from(id).ok(),
        "the API answered for another channel"
    );
    let t = on(network, Object::Channel(id));
    let mut card = new_card("LightningChannel", &t);
    let short = short_channel_id(id);
    card.name = Some(format!("Channel {short}"));
    card.start = time(&v["created"]);
    card.end = time(&v["closing_date"]);
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
    if network == Network::Mainnet {
        card.link(Link::new(
            "1ML",
            format!("https://1ml.com/channel/{id}"),
            VIA,
        ));
        card.link(Link::new(
            "Amboss",
            format!("https://amboss.space/edge/{id}"),
            VIA,
        ));
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

/// Reads the object from one API: a block by height through its hash. The flag says whether
/// the card's root names the link for good (always, except for a block by height that is not
/// yet `settled`).
async fn read(cx: &super::Context<'_>, to: Api, target: &Target) -> Result<Option<(Card, bool)>> {
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
    let Some(card) = parse(f.status, &f.body, &target)? else {
        return Ok(None);
    };
    let mut named = true;
    if let (true, Object::Block(BlockRef::Hash(hash))) = (by_height, &target.object) {
        let status = status_request(to, target.network, hash).context("block status")?;
        let tip = tip_request(to, target.network).context("tip")?;
        let (status, tip) = (fetch(cx, &status).await?, fetch(cx, &tip).await?);
        named = settled(
            status.status,
            &status.body,
            parse_tip(tip.status, &tip.body)?,
        )?;
    }
    Ok(Some((card, named)))
}

#[async_trait]
impl Source for Mempool {
    fn name(&self) -> &'static str {
        "mempool"
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
            // Blocks
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
        &[
            ("mempool.space", 1.0, None),
            ("blockstream.info", 1.0, None),
        ]
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
        let read = match read(cx, Api::Mempool, &target).await {
            Ok(read) => read,
            Err(e) if request(Api::Esplora, &target).is_some() => {
                read(cx, Api::Esplora, &target).await.map_err(|_| e)?
            }
            Err(e) => return Err(e),
        };
        Ok(match read {
            Some((card, named)) => {
                let mut doc = card.into_doc(cx.card(uri, self), SITE);
                if !named {
                    doc.source_root = None;
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
    fn invalid_links() {
        for uri in [
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
            "https://mempool.emzy.de/tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d",
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
        let links: Vec<_> = card.also_on.iter().map(|l| l.uri.as_str()).collect();
        assert_eq!(
            links,
            [
                format!("https://blockstream.info/block/{HASH}"),
                format!("https://mempool.emzy.de/block/{HASH}"),
                format!("https://btcscan.org/block/{HASH}"),
                format!("https://www.blockchain.com/explorer/blocks/btc/{HASH}"),
            ]
        );
        let other = "00000000b747d47c3b38161693ad05e26924b3775a8be669751f969da836311e";
        assert!(parse_block(200, &read("block-481824.json"), Network::Mainnet, other).is_err());
        assert!(parse_block(429, b"", Network::Mainnet, HASH).is_err());
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
        assert!(card.also_on.is_empty(), "no mirror shows testnet4");
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
        assert_eq!(card.also_on.len(), 4);
        assert_eq!(
            parse_tx(404, &read("tx-missing.txt"), Network::Mainnet, PIZZA).unwrap(),
            None
        );
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
            card.also_on.last().unwrap().uri,
            format!("https://www.blockchain.com/explorer/addresses/btc/{GENESIS}")
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
            card.start.unwrap().to_rfc3339(),
            "2019-01-02T18:13:39+00:00"
        );
        assert_eq!(card.details["capacity"], 36108321892i64);
        assert_eq!(card.details["channels"], 1834);
        assert_eq!(card.also_on.len(), 3);
        assert_eq!(card.also_on[0].uri, format!("https://1ml.com/node/{ACINQ}"));
        // An unknown key and a fault look the same: retry.
        assert!(parse_node(500, &read("node-missing.json"), Network::Mainnet, ACINQ).is_err());
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
            card.start.unwrap().to_rfc3339(),
            "2023-10-13T09:32:44+00:00"
        );
        assert_eq!(card.end, None, "still open");
        assert_eq!(card.details["capacity"], 500000000);
        assert_eq!(card.details["status"], "active");
        assert_eq!(
            card.details["funding_tx"],
            "a7f10c2595c8ea0cc85c3183709d10aada60c7b87fe0c405f3f5b8a44150d4f5"
        );
        assert_eq!(card.details["node1_alias"], "ACINQ");
        assert_eq!(card.details["node2_alias"], "bfx-lnd0");
        assert_eq!(
            card.also_on[1].uri,
            "https://amboss.space/edge/892785849701564416"
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
            let doc = card
                .clone()
                .into_doc(ResourceMeta::new(&card.id, m.name(), 60), SITE);
            for name in doc.attributes.keys() {
                assert!(m.facts().contains(&name.as_str()), "{name}");
            }
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
            assert_eq!(doc.site, "mempool.space");
        }
    }

    #[test]
    fn heights_are_grouped() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(481824), "481,824");
    }
}
