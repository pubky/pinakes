Podcast Index API responses (`api.podcastindex.org/api/1.0`), recorded once on 2026-10-21 with
the Pinakes user agent and the operator's signed headers (the key and secret are in no file
here). Parser regression fixtures for `pinakes_core::sources::podcast_index`; test runs make no
requests.

- `feed-577105.json`, `feed-48166.json`: `podcasts/byfeedid` for Darknet Diaries and Acquired.
- `episode-61684996715.json`: `episodes/byid` for an episode of feed 255500.
- `feed-missing.json`, `episode-missing.json`: ids that name nothing. The API answers 200 with
  an empty list.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served.
