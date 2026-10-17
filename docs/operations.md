
Web access interstitials are stored as `blocked` with no display fields or grouping evidence.
`queue.blocked_ttl_secs` controls probes (default 86400; 3600–2592000 seconds); these probes
never charge failures or dead-letter the resource. Access failure is not evidence of deletion:
accepted event dates and feed occurrences survive, and only two subsequent disappearance
observations can cancel them. `pinakes_blocked_resources{host}` reports current counts by
registrable domain, the top `api.blocked_metric_hosts` (default 20) plus `other`. Wall flips
are `blocked`/`recovered` change lines, and blocked rows are excluded from search.
HTML meta-refresh and literal JavaScript redirects share the HTTP three-hop egress budget.
A meta refresh without a URL is an auto-reload and content; a refresh slower than ten seconds
is a hop only on a short page that does not refresh itself.

Wall heuristics live in `data/interstitial-markers.json` (text markers, captcha selectors,
wall host prefixes, sign-in paths, host overrides); `web.wall_markers_path` replaces the file
and `web.wall_overrides = { "example.org" = "content" }` (or `"blocked"`) decides a host and
its subdomains outright. Visible text excludes `script`, `style`, `noscript` and `template`.
Title, heading and captcha markers count only on short pages (under 2,000 visible bytes) or
challenge statuses (403/429/503); a redirect of the requested path to a listed sign-in path
(`/login`, `/authwall`, `/signin`, `/checkpoint`, …) is a wall. Paths compare lowercased and
without trailing slashes, and a requested path that is itself a sign-in path (`/signin` to
`/signin/callback`) is content. A new reference re-probes a
blocked page early, at most `ingest.blocked_reprobes_per_host_per_day` times per host per UTC
day (default 20, 0–1000; 0 disables) and not within an hour of the last attempt, so false positives
heal when someone cares about the page.

Redirect aliases do not establish group identity. Only explicit canonical/main-entity metadata,
validated sameAs evidence, or an entirely permanent HTTP redirect chain contributes URL keys.
A URL key claimed by more than two resources on distinct paths within one publisher is
demoted, but only for non-authoritative claimants: a page's own URI and registry-named
landing pages are never demoted, so squatting pages cannot take a page's identity. Same-page
variants are not counted: the same path (query variants), a path extending the target
(`/article/print`) or the target behind a listed leading segment (`/amp`, `/m`, `/mobile`).
Any other path ending in the target (`/~user/article`) is a claimant, and so is a path with a
segment that decodes to a separator, a dot segment, a further escape or a path parameter
(`/article/..%2Fevil`, `/article/..%252Fevil`, `/article/..;/x`), or a URL with another scheme
or port. Migration 0033 re-plans
every resource holding a non-authoritative URL claim once so stored flags follow this rule
(it also settles 0031's backfill, which counted variants until each resource re-planned).
Demotion is recomputed from live claims on every reconciliation; demoted claims stay stored
(hidden from planning), and removing a claim (document change or orphan deletion) re-queues
the remaining demoted claimants, so groups heal without operator action. There is no
permanent tombstone. `pinakes_group_url_demotions_total` counts demotion events. In an
automatic group, claimants rank below the member they name for the card, before taggers: a
DOI/ISBN card below the landing page its registry names (unless that page is on the card's own
registry origin: an Open Library edition page is the registry's own record and ranks below its
ISBN card, so a book group shows the Book card), a member whose canonical names
another member (an ICS occurrence naming its event page included), and a member whose sameAs
names another member. Two sameAs exemptions apply: a sameAs to an identifier-registry resource
on another origin (doi.org, openlibrary.org, wikidata.org), so a publisher keeps the card, and
a sameAs the named member returns, because reciprocal pairs are peers. A registry key on another
site groups only when the named page cites the identifier back (`citation_doi`, `prism.doi`,
`dc.identifier`, `citation_isbn` meta in the document head, or a JSON-LD identifier/sameAs).
Only Open Library's own edition pages are exempt, and an ISBN card names only its record's own
edition (records are wiki-editable). Any doi.org host is the DOI proxy, never a landing page.
Migration 0032 re-plans cards holding registry claims once so earlier one-sided merges split.
Migration 0034 refetches ISBN cards once over an hour and re-plans registry claimants and
encoded-path claimants. The penalty is a yes/no flag per candidate, not a pairwise order: in a
chain A→B→C both A and B are claimants and taggers order them, so only the non-claimant end of
the chain is guaranteed on top. Demoted claims carry no penalty: three or more same-origin
claimants naming one page on distinct paths are demoted, stop naming it for ranking and fall
back to the no-claim baseline where taggers decide (they also stop joining through that key).
Canonical, alternate and oEmbed links are read from the document head only. JSON-LD identity
fields come only from the page's own entity: a head script's, plus a body script's chosen entity
when it names the page or its head canonical by `url`, `@id` or `mainEntityOfPage` (a URL, node
or array). When a body entity is chosen for the card, the head script's claims still apply if
its entity is a WebPage or better; a head Organization/WebSite/Person node donates nothing. Migration 0035 refetches
resources holding a non-authoritative canonical claim once, staggered over six hours (never
later than their scheduled refresh). Migration 0036 does the same, in batches of 1000, for
every resource holding a canonical or sameAs URL claim other than its own URI, trusted or not.
Its new coverage over 0035 is sameAs-basis claims (body JSON-LD sameAs, and claimants with
double-encoded or path-parameter segments that hold only sameAs). Same-origin canonicals were
never trusted (only own-URI and registry keys are), so 0035 already covered them. A deployment
that ran 0035 refetches those resources a second time, which is harmless; the 0036 header
calls them trusted, but it is published and stays byte-identical.
At startup, before the re-queue below, the daemon brings every stored routing hint up to the
current classification (batches of 1000): a release that adds a route (OSM object aliases and
location links in round 11) re-routes existing rows without a migration. Failed rows are revived
by the reroute trigger, `ok` rows re-resolve through the re-queue, and a row entering or leaving
the OSM route gets or drops its identity envelope. Pages promoted to `media`/`activitypub` by
their own card keep that route. Migration 0038 also turns earlier OSM-host refusals from dead
letters into final `unsupported` rows, so they no longer count toward the dead-letter alert.
Parser changes reach stored documents the same way. Each resolver kind has an extractor version
(`resolver::EXTRACTOR_VERSIONS`), bumped in the same commit as any change to what it extracts
or how it builds group evidence, and every save records it (`resource.extracted_with`). After
the re-queue above, with `queue.reextract_on_upgrade` (default true), the daemon re-fetches `ok`
rows whose version is below their resolver's current one. These re-fetches are spread over
`queue.reextract_window_secs` (default 21600, 3600–604800) and are never later than the
scheduled refresh. They run in batches of 1000, with leases fenced and fetch validators
cleared; OSM rows also expire their `osm_place` cache entry so the object is looked up again.
Rows already due within the window are skipped, so restarts are idempotent. A re-plan cannot
replace this, because it reuses the stored document. Migration 0040 marks every resolved row
as version 0, so the first start after it re-extracts everything once, throttled by the normal
per-host and OSM limits.
A save records the route that produced the document (`resource.resolved_by`). At startup the
daemon re-queues `ok` rows whose `resolved_by` differs from `resolver` (batches of 1000,
staggered over one hour, never later than the scheduled refresh, generation bump, event-fetch
validators cleared so a 304 cannot re-save the old card). Rows already due within the hour are
skipped, so restarts are idempotent. Migration 0037 leaves `resolved_by` empty for `ok` cards
from the generic web extractor on the `event_platform`, `media`, `nostr`, `isbn`, `doi` and
`activitypub` routes, which 0012/0013/0015 re-routed without refetching (0013 restores the
pre-0012 `next_attempt_at`, and its and 0015's `greatest(...)` keep it). A config-disabled
card kind served by the web worker records its own route, so toggling it is not re-queued.
Aliases remain available as navigation metadata. Lock contention between group writers
retries after 1–3 seconds without advancing the conflict backoff; inputs recorded under a
partial (pair-budget) plan are recomputed on the next reconciliation instead of cached.

Grouping conflicts use a separate durable retry counter with 5–10 seconds of initial jitter,
exponential growth and a one-hour ceiling. Saves respect that window, and the daemon drains
full batches continuously. Reference churn does not charge resource resolution attempts.
The pair budget scales as `max(100000, 2 * max_members²)`, capped at two million evaluations;
`max_members_per_origin` cannot exceed `max_members`. On exhaustion the partial plan preserves
established memberships, records `group_input.last_error='pair_budget'`, and increments
`pinakes_group_pair_budget_total` without repeatedly parking the same fingerprint.
Curation waits up to two seconds for the writer and returns 503 with Retry-After on contention.

Startup persists the grouping policy (including tenant rules) and re-dirties memberships when
it changes. For a manual rebuild use `DELETE FROM group_input; INSERT INTO group_dirty(resource_id)
SELECT DISTINCT resource_id FROM group_unit ON CONFLICT DO NOTHING;`. Existing retry windows
remain in force. Changing `groups.cross_container_occurrences` takes effect as that queue drains.

Tenant rules support `"subdomain"`, `"first_path_segment"`, `{"path_segment":3}` (one-based,
1–8) and `{"path":{"segment":3,"skip":["u/*"],"extend":["s"],"query":["src"]}}`: `skip`
removes the first matching noise window (Google Calendar's per-user `u/{n}`), a tenant segment
listed in `extend` takes the next segment too (anchor.fm `/s/{id}` feeds beside `/{show}`
pages), and when the path is too short the first listed query parameter present scopes the
tenant (`embed?src=`); otherwise each URL is its own scope. Prefix segments remain part of
the scope. Hosted `*.podbean.com` sites are subdomain tenants. Tests cover the concrete URL shapes for every
seed, including constant-prefix Acast, Anchor, Google, SoundCloud and Podbean feed URLs.
Capability-only iCloud/Outlook path rules are removed; native calendars already use their
referenced feed identity as the container scope, without treating shared service paths as tenants.
The public feed shapes were checked against [Spotify's hosting guide](https://support.spotify.com/se/creators/article/switching-your-show-hosting-to-spotify-for-creators/),
[Acast's RSS guide](https://learn.acast.com/en/articles/3404332-how-to-add-your-podcast-to-tunein),
and [Podbean's feed guide](https://help.podbean.com/support/solutions/articles/25000004906-validating-my-podcast-feed).
Redirected RSS keeps the referenced URI as its publisher and cannot borrow the destination's
podcast GUID evidence. Migration 0027 isolates stale container origins and staggers re-fetches
for historical podcast cards without feed provenance.

The capability-URL guard scans every tracked source file and every nonignored new source file,
including root configuration, Rust and scripts. Git-ignored runtime credentials/artifacts and
`.git`, `target`, `node_modules` are excluded. Exact synthetic fixture URLs are allowed only
in named test files; failures print paths without URL values.
