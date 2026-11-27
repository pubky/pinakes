# Podcast Index catalogue fixture

`podcastindex_feeds.db`: eleven rows of the `podcasts` table of Podcast Index's database
(`https://public.podcastindex.org/podcastindex_feeds.db.tgz`, the file of 2026-11-21), with the
table's own schema: the feeds with IDs 1-8 that the file's first 20 MB hold, and four feeds with
popularity 9. Rows were copied as they are (`scripts/catalogue/sqlite-head.py`). Podcast Index
publishes this database for anyone to download and build on; its terms are on podcastindex.org.
