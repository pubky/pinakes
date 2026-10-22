# Pubky app objects

Public objects of the apps in `resolver/pubky_apps/`, as their owners' homeservers served them
on 2026-10-22 (read-only GETs, nothing under `/priv/`). Each `<name>.json` holds the bytes
served and `<name>.uri` the object's `pubky://` URI. Rooms objects came from Synonym's staging
homeserver, the others from `homeserver.pubky.app`.

One change: in `eventky/event-bern-meetup-recurring.json` the token of a Telegram group invite in
the description is replaced by `REDACTED`.

`eventky/invalid-calendar-wbg-ptr.json` is a real calendar object a client bug wrote; it has no
card.

| Fixture | URI |
|---|---|
| `eventky/calendar-dezentralschweiz.json` | `pubky://51da3n5m8s6oaq38uqs7jznp6ezbc3qbtmic8oy6fj3g6mokdyco/pub/eventky.app/calendars/0034PXJ40MGQG` |
| `eventky/calendar-minimal.json` | `pubky://51da3n5m8s6oaq38uqs7jznp6ezbc3qbtmic8oy6fj3g6mokdyco/pub/eventky.app/calendars/00354MWHX33ZG` |
| `eventky/event-bern-meetup-recurring.json` | `pubky://51da3n5m8s6oaq38uqs7jznp6ezbc3qbtmic8oy6fj3g6mokdyco/pub/eventky.app/events/0034FED2YCDKG` |
| `eventky/event-psywalk-location-no-uri.json` | `pubky://51da3n5m8s6oaq38uqs7jznp6ezbc3qbtmic8oy6fj3g6mokdyco/pub/eventky.app/events/0035DS0K1ZTEG` |
| `eventky/invalid-calendar-wbg-ptr.json` | `pubky://51da3n5m8s6oaq38uqs7jznp6ezbc3qbtmic8oy6fj3g6mokdyco/pub/eventky.app/calendars/7YDE2JFRDRKY6RMMPMJA3SGNWG` |
| `loopky/medeng2026a-url-cover.json` | `pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/medeng2026a/manifest.json` |
| `loopky/p2uuj5jo5ln2-media-cover.json` | `pubky://yjn8k7ww4sc4hskkmbcsep6b1xje8g4f8j3azpdqnwhhqckt7kso/pub/loopky/decks/p2uuj5jo5ln2/manifest.json` |
| `loopky/y9zq5xw1ng1e-bare.json` | `pubky://hbxi6npzi1gyh4ayg97kdnxw6913kax6tuowcezd6g8q4p9q3b1y/pub/loopky/decks/y9zq5xw1ng1e/manifest.json` |
| `mapky/incident-hazard-medium.json` | `pubky://gi169rgooxb3mza8j9uf1xoqjacakgwp4za1dz1hgghbhet4m86y/pub/mapky.app/incidents/00357W2047V80` |
| `mapky/legacy-collection-color.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/collections/00354H3XK357G` |
| `mapky/legacy-collection-empty.json` | `pubky://c5jsbrwmouzedmf11qijk3gp8qeizkdsgtneq5t185jc41wxn6my/pub/mapky.app/collections/00356VA321QBG` |
| `mapky/legacy-post-kind-post.json` | `pubky://c5jsbrwmouzedmf11qijk3gp8qeizkdsgtneq5t185jc41wxn6my/pub/mapky.app/posts/0034WJ4EA0MV0` |
| `mapky/legacy-post-review-place-object.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/posts/0034WH5EPSKA0` |
| `mapky/legacy-post-review-place-url.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/posts/00351MT54YS9G` |
| `mapky/post-short-osm-parent.json` | `pubky://9g3zsjpwijn3gsh7ibghjxxadpjidceos5rw4387a6hhziqi8aoy/pub/mapky.app/posts/0035C8FNKDQS0` |
| `mapky/review-rating-only-way.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/reviews/003529F35DWWG` |
| `mapky/review-with-content.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/reviews/00355XPM88EHG` |
| `mapky/route-giggelitrain.json` | `pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/routes/00351MV7WF0WG` |
| `mapky/route-merlischachen-luzern.json` | `pubky://c5jsbrwmouzedmf11qijk3gp8qeizkdsgtneq5t185jc41wxn6my/pub/mapky.app/routes/00356PC3XMBP0` |
| `marketplace/drop.json` | `pubky://nkcct8tzquo8n4z5ysz9t963ye9kq1w7gb55aad1z4tmsgjjhmto/pub/pubky.app/marketplace/v1/drops/0700b93b984743c39f6d8009f4aa1271` |
| `marketplace/listing-active.json` | `pubky://8fwmk5o1wfmn6whew47zoq31ka1mss15xnkcihattr5zbthp5nbo/pub/pubky.app/marketplace/v1/listings/14838d8cf28d413ea274ac7ec608c5f8` |
| `marketplace/listing-paused.json` | `pubky://nkcct8tzquo8n4z5ysz9t963ye9kq1w7gb55aad1z4tmsgjjhmto/pub/pubky.app/marketplace/v1/listings/04434f48ef834527840efed75a080607` |
| `marketplace/shop.json` | `pubky://rxtcr8ezoja67qq3wcrxtueefpzaau65tr8nf5o8d8e8oc7a33jo/pub/pubky.app/marketplace/v1/shop.json` |
| `rooms/0035SFYCGVBV2-introductions.json` | `pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2` |
| `rooms/0035TNXH48JYE-null-topic.json` | `pubky://8febau67zieo93b9ejk947msc7oc9e6b7jo6whkj5smtzsrmrigo/pub/pubky-rooms/rooms/0035TNXH48JYE` |
