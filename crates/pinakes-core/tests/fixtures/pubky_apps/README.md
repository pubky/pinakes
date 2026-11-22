# Pubky app objects

Public objects of the apps in `resolver/pubky_apps/`, as their owners' homeservers served them
on 2026-10-22 (read-only GETs, nothing under `/priv/`); `old-layouts/` holds objects of the old
Mapky and Eventky layouts, which get the general card, as `homeserver.pubky.app` served them on
2026-11-22. Each `<name>.json` holds the bytes
served and `<name>.uri` the object's `pubky://` URI. Rooms objects came from Synonym's staging
homeserver, the others from `homeserver.pubky.app`. The marketplace's records are the ones its
client wrote; their `.uri` is where the tests read them, in the marketplace's namespace.

| Fixture | URI |
|---|---|
| `loopky/medeng2026a-url-cover.json` | `pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/medeng2026a/manifest.json` |
| `loopky/p2uuj5jo5ln2-media-cover.json` | `pubky://yjn8k7ww4sc4hskkmbcsep6b1xje8g4f8j3azpdqnwhhqckt7kso/pub/loopky/decks/p2uuj5jo5ln2/manifest.json` |
| `loopky/y9zq5xw1ng1e-bare.json` | `pubky://hbxi6npzi1gyh4ayg97kdnxw6913kax6tuowcezd6g8q4p9q3b1y/pub/loopky/decks/y9zq5xw1ng1e/manifest.json` |
| `marketplace/drop.json` | `pubky://nkcct8tzquo8n4z5ysz9t963ye9kq1w7gb55aad1z4tmsgjjhmto/pub/app.marketplace/v1/drops/0700b93b984743c39f6d8009f4aa1271` |
| `marketplace/listing-active.json` | `pubky://8fwmk5o1wfmn6whew47zoq31ka1mss15xnkcihattr5zbthp5nbo/pub/app.marketplace/v1/listings/14838d8cf28d413ea274ac7ec608c5f8` |
| `marketplace/listing-paused.json` | `pubky://nkcct8tzquo8n4z5ysz9t963ye9kq1w7gb55aad1z4tmsgjjhmto/pub/app.marketplace/v1/listings/04434f48ef834527840efed75a080607` |
| `marketplace/shop.json` | `pubky://rxtcr8ezoja67qq3wcrxtueefpzaau65tr8nf5o8d8e8oc7a33jo/pub/app.marketplace/v1/shop.json` |
| `rooms/0035SFYCGVBV2-introductions.json` | `pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2` |
| `rooms/0035TNXH48JYE-null-topic.json` | `pubky://8febau67zieo93b9ejk947msc7oc9e6b7jo6whkj5smtzsrmrigo/pub/pubky-rooms/rooms/0035TNXH48JYE` |
| `old-layouts/eventky-event-0034M8WQCPK20.json` | `pubky://gabri5dk51q1c1kzi53hjpqw5yhmw1nqsryzsfx5kubd7jpm63jy/pub/eventky.app/events/0034M8WQCPK20` |
| `old-layouts/mapky-collection-00351MVGE211G.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/collections/00351MVGE211G` |
| `old-layouts/mapky-geo-capture-0035C4V0GVX90.json` | `pubky://9g3zsjpwijn3gsh7ibghjxxadpjidceos5rw4387a6hhziqi8aoy/pub/mapky.app/geo_captures/0035C4V0GVX90` |
| `old-layouts/mapky-post-review-00351MT54YS9G.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/posts/00351MT54YS9G` |
| `old-layouts/mapky-review-rating-only-003529F35DWWG.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/reviews/003529F35DWWG` |
| `old-layouts/mapky-route-00351MV7WF0WG.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/routes/00351MV7WF0WG` |
