
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
(`/login`, `/authwall`, `/signin`, `/checkpoint`, …) is a wall. A new reference re-probes a
blocked page early, at most `ingest.blocked_reprobes_per_host_per_day` times per host per UTC
day (default 20; 0 disables) and not within an hour of the last attempt, so false positives
heal when someone cares about the page.

Redirect aliases do not establish group identity. Only explicit canonical/main-entity metadata,
validated sameAs evidence, or an entirely permanent HTTP redirect chain contributes URL keys.
A URL key claimed by more than two resources on distinct paths within one publisher is
demoted, but only for non-authoritative claimants: a page's own URI and registry-named
landing pages are never demoted, so squatting pages cannot take a page's identity. Same-page
variants are not counted: the same path (query variants), a path extending the target
(`/article/print`) or the target behind a listed leading segment (`/amp`, `/m`, `/mobile`).
Any other path ending in the target (`/~user/article`) is a claimant. Migration 0033 re-plans
every resource holding a non-authoritative URL claim once so stored flags follow this rule
(it also settles 0031's backfill, which counted variants until each resource re-planned).
Demotion is recomputed from live claims on every reconciliation; demoted claims stay stored
(hidden from planning), and removing a claim (document change or orphan deletion) re-queues
the remaining demoted claimants, so groups heal without operator action. There is no
permanent tombstone. `pinakes_group_url_demotions_total` counts demotion events. In an
automatic group, claimants rank below the member they name for the card, before taggers: a
page whose canonical or sameAs names a page of its own publisher, and a DOI/ISBN card below
the landing page its registry names. A publisher's sameAs to doi.org or Wikidata does not rank
the publisher below that identifier resource. A registry key on another site groups only when
the named page cites the identifier back (`citation_doi`, `prism.doi`, `dc.identifier`,
`citation_isbn` meta, or a JSON-LD identifier/sameAs); migration 0032 re-plans cards holding
registry claims once so earlier one-sided merges split.
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
