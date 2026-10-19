//! The Esplora API at blockstream.info: the fallback reader for mempool.space blocks,
//! transactions and addresses (mainnet, testnet and signet). Its records have the shapes of
//! mempool.space's, so `super::mempool` builds the requests (`Api::Esplora`) and parses the
//! answers, with the same roots and pages.
use super::mempool::Network;

/// The API base at blockstream.info, or `None` for a network it does not serve.
pub fn base(network: Network) -> Option<&'static str> {
    match network {
        Network::Mainnet => Some("https://blockstream.info/api"),
        Network::Testnet => Some("https://blockstream.info/testnet/api"),
        Network::Signet => Some("https://blockstream.info/signet/api"),
        Network::Testnet4 => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::sources::{
        kit::fixture,
        mempool::{self, parse, parse_height, request, Api, BlockRef, Network, Object, Target},
    };

    const HASH: &str = "0000000000000000001c8018d9cb3b742ef25114f27563e3fc4a1902167f9893";
    const PIZZA: &str = "a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d";
    const GENESIS: &str = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";

    fn read(name: &str) -> Vec<u8> {
        fixture::read("esplora", name)
    }
    fn on(network: Network, object: Object) -> Target {
        Target { network, object }
    }

    #[test]
    fn requests() {
        let t = |uri: &str| mempool::from_uri(uri).unwrap();
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
            (format!("https://mempool.space/testnet4/tx/{PIZZA}"), None),
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
}
